//! Panel API against a canned watch channel: status JSON, SSE on update,
//! solve-now notify, valid SVG, relative URLs only.

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
            target_unmet: 0.0,
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
        }],
        ..Default::default()
    }
}

fn state_with(r: SolveReport) -> (WebState, watch::Sender<SolveReport>, Arc<Notify>) {
    let (tx, rx) = watch::channel(r);
    let notify = Arc::new(Notify::new());
    (WebState { report: rx, solve_now: notify.clone() }, tx, notify)
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
async fn w2_health_is_ok() {
    let (s, _tx, _n) = state();
    let resp =
        router(s).oneshot(Request::get("/health").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}
