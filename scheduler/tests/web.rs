//! Panel API against a canned watch channel: status JSON, SSE on update,
//! solve-now notify, valid SVG, relative URLs only.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use legit_lp_scheduler::status::{LoadReport, SolveReport, StorageReport};
use legit_lp_scheduler::web::{router, WebState};
use tokio::sync::{watch, Notify};
use tower::ServiceExt; // oneshot

fn report() -> SolveReport {
    SolveReport {
        at: "2026-06-10T10:00:00+10:00".into(),
        global_enabled: true,
        dry_run: true,
        grid: vec!["2026-06-10T10:00:00+10:00".into(); 4],
        loads: vec![LoadReport {
            id: "hot_water".into(),
            planning: "runtime".into(),
            authority: true,
            running: Some(false),
            action: "NoChange".into(),
            reason: "idle; lp plan".into(),
            unmet: 0.0,
            executed: false,
            on: vec![false, true, true, false],
            ct: vec![false, false, true, false],
            reasoning: Default::default(),
        }],
        ..Default::default()
    }
}

/// A report with every series populated — drives all of the chart's lanes.
/// Price carries a deliberate `None` gap (step 2) to exercise segmentation.
fn full_report() -> SolveReport {
    SolveReport {
        at: "2026-06-10T00:00:00+10:00".into(),
        global_enabled: true,
        dry_run: true,
        grid: (0..8).map(|h| format!("2026-06-10T0{h}:00:00+10:00")).collect(),
        price: vec![
            Some(0.10),
            Some(0.10),
            None,
            Some(0.50),
            Some(0.50),
            Some(0.20),
            Some(0.20),
            Some(0.20),
        ],
        feedin: vec![0.05; 8],
        pv: vec![0.0, 0.0, 0.0, 1.0, 3.0, 3.0, 1.0, 0.0],
        baseload: vec![0.8; 8],
        grid_kw: vec![0.8, 0.8, -2.0, 0.8, -1.0, 0.5, 0.8, 0.8],
        storage: vec![StorageReport {
            id: "sonnen".into(),
            capacity_kwh: 10.0,
            min_soc_kwh: 1.0,
            max_soc_kwh: 10.0,
            soc_now_kwh: 5.0,
            soc_kwh: vec![5.0, 5.5, 6.0, 6.0, 5.0, 4.0, 4.0, 4.0, 4.0],
            charge_kw: vec![2.0, 2.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            discharge_kw: vec![0.0, 0.0, 0.0, 0.0, 4.0, 4.0, 0.0, 0.0],
            action: "charging".into(),
            authority: true,
            charge_authority: true,
            discharge_authority: false,
            target_unmet: 0.0,
            reasoning: Default::default(),
        }],
        loads: vec![LoadReport {
            id: "hot_water".into(),
            planning: "runtime".into(),
            authority: false,
            running: Some(false),
            action: "NoChange".into(),
            reason: "observe-only".into(),
            unmet: 0.0,
            executed: false,
            on: vec![false, true, true, false, false, true, false, false],
            ct: vec![false, false, true, false, false, false, false, false],
            reasoning: Default::default(),
        }],
        ..Default::default()
    }
}

fn state_with(r: SolveReport) -> (WebState, watch::Sender<SolveReport>, Arc<Notify>) {
    let (tx, rx) = watch::channel(r);
    let notify = Arc::new(Notify::new());
    let preview = Arc::new(AtomicBool::new(false));
    (WebState { report: rx, solve_now: notify.clone(), preview }, tx, notify)
}

fn state() -> (WebState, watch::Sender<SolveReport>, Arc<Notify>) {
    state_with(report())
}

async fn body_of(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test]
async fn w1_status_returns_report_json() {
    let (s, _tx, _n) = state();
    let resp =
        router(s).oneshot(Request::get("/api/status").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body_of(resp).await).unwrap();
    assert_eq!(v["loads"][0]["id"], "hot_water");
    assert_eq!(v["global_enabled"], true);
}

#[tokio::test]
async fn w3_solve_now_fires_the_notify() {
    let (s, _tx, notify) = state();
    let waiter = notify.clone();
    let waiting = tokio::spawn(async move { waiter.notified().await });
    let resp =
        router(s).oneshot(Request::post("/api/solve").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    tokio::time::timeout(std::time::Duration::from_secs(1), waiting).await.unwrap().unwrap();
}

#[tokio::test]
async fn w4_horizon_svg_is_valid_with_load_blocks_and_axis() {
    // Minimal report (only loads): the chart degrades to load rows + axis.
    let (s, _tx, _n) = state();
    let resp =
        router(s).oneshot(Request::get("/horizon.svg").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let svg = body_of(resp).await;
    assert!(svg.starts_with("<svg") && svg.ends_with("</svg>"));
    assert_eq!(svg.matches(r#"class="load-on""#).count(), 1, "one must-have block");
    assert_eq!(svg.matches(r#"class="load-ct""#).count(), 1, "one can-take block");
    assert!(svg.contains("#4caf50"), "can-take block coloured distinctly");
    assert!(svg.contains(r#"class="now-line""#), "now marker present");
    // No forecast/battery data -> those lanes are absent, not faked.
    assert!(!svg.contains("price-import") && !svg.contains("soc-line"));
}

#[tokio::test]
async fn w6_horizon_svg_renders_every_lane_for_a_full_report() {
    let (s, _tx, _n) = state_with(full_report());
    let resp =
        router(s).oneshot(Request::get("/horizon.svg").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let svg = body_of(resp).await;
    assert!(svg.starts_with("<svg") && svg.ends_with("</svg>"));
    // Price lane, broken into two polylines around the None gap at step 2.
    assert_eq!(svg.matches(r#"class="price-import""#).count(), 2, "price line split at the gap");
    assert!(svg.contains(r#"class="price-feedin""#), "feed-in line");
    // Power lane: PV area + both grid directions.
    assert!(svg.contains(r#"class="pv-area""#), "solar area");
    assert!(svg.contains(r#"class="grid-import""#), "grid import bars");
    assert!(svg.contains(r#"class="grid-export""#), "grid export bars");
    assert!(svg.contains(r#"class="baseload-line""#), "baseload line");
    // Storage lane (labelled by device id): SoC + reserve + both action colours.
    assert!(svg.contains(">sonnen</text>"), "storage lane labelled by device id");
    assert!(svg.contains(r#"class="soc-area""#) && svg.contains(r#"class="soc-line""#), "SoC");
    assert!(svg.contains(r#"class="soc-reserve""#), "reserve floor");
    assert!(svg.contains(r#"class="batt-charge""#), "charging strip");
    assert!(svg.contains(r#"class="batt-discharge""#), "discharging strip");
    // Loads + shared axis.
    assert!(svg.contains(r#"class="load-on""#) && svg.contains(r#"class="load-ct""#), "loads");
    assert!(svg.contains(r#"class="now-line""#) && svg.contains(r#"class="tick""#), "axis");
    // Hover layer: one transparent full-height hit-band per step, each carrying a
    // <title> readout of that step's data. Inlined in the page (w7), the browser
    // shows it on hover — so "hover a line to see the price" works.
    assert_eq!(svg.matches(r#"class="hit""#).count(), 8, "one hover band per step");
    assert!(svg.contains("<title>"), "hover readout present");
    assert!(svg.contains("import 0.100 $/kWh"), "title shows the step's import price");
    assert!(svg.contains("feed-in 0.050 $/kWh"), "title shows the step's feed-in");
    // NOTE (TDD three-level rule): the actual hover-to-show-tooltip interaction is
    // browser-native and has no headless seam, so it is NOT unit/e2e tested here.
    // What IS testable — the SVG *content* (per-step hit-bands + value readouts)
    // and that the page inlines the SVG — is covered (this test, w7, e2e svg check).
}

#[tokio::test]
async fn w5_index_uses_relative_urls_only() {
    let (s, _tx, _n) = state();
    let resp = router(s).oneshot(Request::get("/").body(Body::empty()).unwrap()).await.unwrap();
    let html = body_of(resp).await;
    for needle in ["./api/status", "./api/events", "./api/solve", "./horizon.svg"] {
        assert!(html.contains(needle), "missing {needle}");
    }
    assert!(
        !html.contains("\"/api") && !html.contains("'/api") && !html.contains("(\"/"),
        "absolute URLs would break under the ingress prefix"
    );
}

#[tokio::test]
async fn w7_index_inlines_the_svg_so_hover_tooltips_work() {
    // An <img>-loaded SVG is a flat image: no hover, no <title> tooltips. The page
    // must inline the SVG (fetch its text and inject it) so the per-step hover
    // readouts render and the user can see the value under the cursor.
    let (s, _tx, _n) = state();
    let resp = router(s).oneshot(Request::get("/").body(Body::empty()).unwrap()).await.unwrap();
    let html = body_of(resp).await;
    assert!(!html.contains("<img"), "no <img>: an img-loaded SVG cannot show tooltips");
    assert!(html.contains("./horizon.svg"), "still loads the chart");
    assert!(html.contains("innerHTML"), "injects the SVG inline for interactivity");
}

#[tokio::test]
async fn w11_index_has_per_device_why_panels_for_loads_and_storage() {
    // Every load AND storage device gets an Overview/Why two-tab card driven by the
    // serialised `reasoning` object — so the user can see why the LP did what it did.
    let (s, _tx, _n) = state();
    let resp = router(s).oneshot(Request::get("/").body(Body::empty()).unwrap()).await.unwrap();
    let html = body_of(resp).await;
    assert!(html.contains("setTab"), "tab switching is wired");
    assert!(
        html.contains(">Overview<") && html.contains(">Why<") && html.contains(">Plan<"),
        "all three tabs are present (Overview / Why / Plan)"
    );
    assert!(html.contains(r#"id="storage""#), "storage devices get their own cards");
    assert!(html.contains("reasoning"), "cards render the reasoning view-model");
    assert!(html.contains("Resolved inputs"), "the Why tab lists resolved live inputs");
    // The Plan tab renders each device's full-horizon schedule from the streamed
    // arrays (no API change): a sparkline + an exact per-block table.
    assert!(
        html.contains("planView") && html.contains("planBattery") && html.contains("planLoad"),
        "the Plan tab builds per-device full-horizon views"
    );
    assert!(
        html.contains("class=\"plan-tbl\"") || html.contains("plan-tbl"),
        "the Plan tab renders the exact per-block table"
    );
}

#[tokio::test]
async fn w8_preview_toggle_sets_the_shared_flag_and_nudges_a_resolve() {
    // The in-panel checkbox POSTs its new state to /api/preview?on=<bool>. The
    // handler stores it into the shared runtime flag (which the solve loop reads
    // each tick) and fires solve-now so the change takes effect immediately.
    let (tx, rx) = watch::channel(report());
    let _tx = tx; // keep the channel's last value readable
    let notify = Arc::new(Notify::new());
    let preview = Arc::new(AtomicBool::new(false));

    let waiter = notify.clone();
    let waiting = tokio::spawn(async move { waiter.notified().await });
    let on = WebState { report: rx.clone(), solve_now: notify.clone(), preview: preview.clone() };
    let resp = router(on)
        .oneshot(Request::post("/api/preview?on=true").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(preview.load(Ordering::Relaxed), "toggle-on set the shared preview flag");
    tokio::time::timeout(std::time::Duration::from_secs(1), waiting)
        .await
        .expect("solve-now fired")
        .unwrap();

    // ...and the same endpoint turns it back off (deterministic set, not a blind toggle).
    let off = WebState { report: rx, solve_now: notify.clone(), preview: preview.clone() };
    let resp = router(off)
        .oneshot(Request::post("/api/preview?on=false").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(!preview.load(Ordering::Relaxed), "toggle-off cleared the shared preview flag");
}

#[tokio::test]
async fn w9_index_has_a_preview_checkbox_wired_to_the_api() {
    // The user asked to toggle preview from the panel itself. The page must carry
    // a checkbox that POSTs to ./api/preview (relative, for the ingress prefix)
    // and reflects the server's reported preview state.
    let (s, _tx, _n) = state();
    let resp = router(s).oneshot(Request::get("/").body(Body::empty()).unwrap()).await.unwrap();
    let html = body_of(resp).await;
    assert!(html.contains(r#"type="checkbox""#), "a checkbox control is present");
    assert!(html.contains(r#"id="preview""#), "it is the preview checkbox");
    assert!(html.contains("./api/preview"), "it POSTs to the relative preview endpoint");
    assert!(html.contains("r.preview"), "its checked state binds to the reported preview flag");
}

#[tokio::test]
async fn w10_index_has_an_instant_crosshair_tooltip() {
    // Beyond the native <title> fallback, the page shows an instant, styled
    // tooltip + crosshair driven by the SAME per-step readouts: it maps the
    // cursor to the hovered hit-band and reuses that band's <title> text — no new
    // data plumbing, just immediate styled presentation of the server's readout.
    let (s, _tx, _n) = state();
    let resp = router(s).oneshot(Request::get("/").body(Body::empty()).unwrap()).await.unwrap();
    let html = body_of(resp).await;
    assert!(
        html.contains(r#"id="tip""#) && html.contains(r#"id="xhair""#),
        "tooltip + crosshair elements present"
    );
    assert!(html.contains("elementFromPoint"), "maps the cursor to the hovered step band");
    assert!(html.contains("closest('.hit')"), "reuses the hovered hit-band's readout");
}

#[tokio::test]
async fn w2_health_is_ok() {
    let (s, _tx, _n) = state();
    let resp =
        router(s).oneshot(Request::get("/health").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn w12_panel_applies_the_pr35_review_fixes() {
    // Three PR #35 Codex review fixes are wired into the served panel asset:
    let (s, _tx, _n) = state();
    let resp = router(s).oneshot(Request::get("/").body(Body::empty()).unwrap()).await.unwrap();
    let html = body_of(resp).await;
    // (1) The red solve-failure banner keys off the critical scheduler alert, not
    //     `stale` alone — so a first-cycle failure with no last-good plan still shows.
    assert!(
        html.contains("a.scope==='scheduler'") && html.contains("solveFailed"),
        "the failure banner triggers on the critical scheduler alert"
    );
    // (2) The storage action pill tags by the ACTIVE direction's authority, so a
    //     charge-only cabinet's advisory discharge is not mislabelled "live".
    assert!(
        html.contains("charge_authority") && html.contains("discharge_authority"),
        "the storage pill uses per-direction authority"
    );
    // (3) Plan-tab durations / energy derive from the report's grid step length
    //     rather than assuming 15-minute steps.
    assert!(
        html.contains("r.grid_minutes"),
        "plan durations derive from grid_minutes, not a hard-coded 15"
    );
}
