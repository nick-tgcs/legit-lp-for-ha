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

fn example_registry() -> legit_lp_scheduler::config::RegistryConfig {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../addon/example.yaml");
    legit_lp_scheduler::config::parse(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// An HA client pointed at nothing — fine for every test except the entity-catalog
/// one, which is the only handler that actually dials HA.
fn dummy_ha() -> Arc<legit_lp_scheduler::ha_client::HaClient> {
    Arc::new(legit_lp_scheduler::ha_client::HaClient::new("http://127.0.0.1:1", "x"))
}

fn state_with(r: SolveReport) -> (WebState, watch::Sender<SolveReport>, Arc<Notify>) {
    let (tx, rx) = watch::channel(r);
    let notify = Arc::new(Notify::new());
    let preview = Arc::new(AtomicBool::new(false));
    let registry = Arc::new(watch::channel(Arc::new(example_registry())).0);
    let registry_path = std::env::temp_dir().join("lp_web_test_unused_registry.yaml");
    (
        WebState {
            report: rx,
            solve_now: notify.clone(),
            preview,
            registry,
            registry_path,
            ha: dummy_ha(),
        },
        tx,
        notify,
    )
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
    for needle in ["./api/status", "./api/events", "./api/solve", "./api/preview"] {
        assert!(html.contains(needle), "missing {needle}");
    }
    assert!(
        !html.contains("\"/api") && !html.contains("'/api") && !html.contains("(\"/"),
        "absolute URLs would break under the ingress prefix"
    );
}

#[tokio::test]
async fn w7_index_is_client_rendered_from_the_report_no_server_svg() {
    // The Friendly panel replaces the server-rendered multi-lane SVG with a
    // client-rendered schedule + price sparkline built from the streamed report,
    // injected live over SSE — no <img>, no /horizon.svg fetch in the page.
    let (s, _tx, _n) = state();
    let resp = router(s).oneshot(Request::get("/").body(Body::empty()).unwrap()).await.unwrap();
    let html = body_of(resp).await;
    assert!(!html.contains("<img"), "no raster/img chart");
    assert!(html.contains("./api/events"), "the panel is driven live over SSE");
    assert!(html.contains("innerHTML"), "renders the view client-side from the report");
    assert!(html.contains(r#"id="schedcard""#), "the client-rendered schedule timeline is present");
}

#[tokio::test]
async fn w11_index_has_per_device_overview_why_plan_cards() {
    // Every load AND storage device gets an Overview/Why/Plan card driven by the
    // serialised `reasoning` object — plain-language first (Overview/Why), with the
    // raw internal values in the Plan tab's monospace `tech` line for power users.
    let (s, _tx, _n) = state();
    let resp = router(s).oneshot(Request::get("/").body(Body::empty()).unwrap()).await.unwrap();
    let html = body_of(resp).await;
    assert!(html.contains("setTab"), "tab switching is wired");
    assert!(
        html.contains("'Overview'") && html.contains("'Why'") && html.contains("'Plan'"),
        "all three tabs are present (Overview / Why / Plan)"
    );
    assert!(html.contains(r#"id="storage""#), "storage devices get their own cards");
    assert!(html.contains("reasoning"), "cards render the reasoning view-model");
    // The Plan tab keeps the raw internal values in a monospace "tech" line — the
    // plain-language-first, raw-values-in-Plan pattern that is the redesign's core.
    assert!(
        html.contains("class=\"tech\"") || html.contains("tech"),
        "Plan tab has the raw tech line"
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
    let registry = Arc::new(watch::channel(Arc::new(example_registry())).0);
    let registry_path = std::env::temp_dir().join("lp_web_test_unused_registry.yaml");

    let waiter = notify.clone();
    let waiting = tokio::spawn(async move { waiter.notified().await });
    let on = WebState {
        report: rx.clone(),
        solve_now: notify.clone(),
        preview: preview.clone(),
        registry: registry.clone(),
        registry_path: registry_path.clone(),
        ha: dummy_ha(),
    };
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
    let off = WebState {
        report: rx,
        solve_now: notify.clone(),
        preview: preview.clone(),
        registry,
        registry_path,
        ha: Arc::new(legit_lp_scheduler::ha_client::HaClient::new("http://127.0.0.1:1", "x")),
    };
    let resp = router(off)
        .oneshot(Request::post("/api/preview?on=false").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(!preview.load(Ordering::Relaxed), "toggle-off cleared the shared preview flag");
}

#[tokio::test]
async fn w9_index_has_a_preview_toggle_wired_to_the_api() {
    // The Friendly panel's Preview control is a switch (toggle), not a checkbox. It
    // POSTs to the relative ./api/preview endpoint and reflects the reported state.
    let (s, _tx, _n) = state();
    let resp = router(s).oneshot(Request::get("/").body(Body::empty()).unwrap()).await.unwrap();
    let html = body_of(resp).await;
    assert!(
        html.contains(r#"id="pvbtn""#) && html.contains(r#"id="pvtrack""#),
        "a switch toggle is present"
    );
    assert!(html.contains("./api/preview"), "it POSTs to the relative preview endpoint");
    assert!(html.contains("r.preview"), "its on-state binds to the reported preview flag");
}

#[tokio::test]
async fn w10_index_has_the_friendly_hero_banner_and_now_tiles() {
    // The Friendly redesign leads with glanceable summary: an eyebrow + hero
    // headline, a mode banner (Preview/Live/Dry-run), and the four "now" stat tiles.
    let (s, _tx, _n) = state();
    let resp = router(s).oneshot(Request::get("/").body(Body::empty()).unwrap()).await.unwrap();
    let html = body_of(resp).await;
    assert!(html.contains(r#"id="banner""#), "the mode banner is present");
    assert!(html.contains(r#"id="tiles""#), "the four 'now' stat tiles are present");
    assert!(
        html.contains(r#"id="heroH""#) && html.contains("eyebrow"),
        "the hero headline is present"
    );
}

#[tokio::test]
async fn w14_commit_registry_persists_publishes_and_nudges_a_resolve() {
    // The hot-swap seam the CRUD API builds on: committing an edited registry must
    // (1) atomically persist it to the registry path, (2) publish it on the watch
    // channel so the solve loop hot-swaps it, and (3) fire solve-now for an instant
    // re-plan — all WITHOUT an add-on restart.
    let (tx, rx) = watch::channel(report());
    let _tx = tx;
    let notify = Arc::new(Notify::new());
    let waiter = notify.clone();
    let waiting = tokio::spawn(async move { waiter.notified().await });

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legit_lp.yaml");
    let registry = Arc::new(watch::channel(Arc::new(example_registry())).0);
    let mut reg_rx = registry.subscribe();
    let s = WebState {
        report: rx,
        solve_now: notify.clone(),
        preview: Arc::new(AtomicBool::new(false)),
        registry: registry.clone(),
        registry_path: path.clone(),
        ha: dummy_ha(),
    };

    // Edit: drop the last load, then commit.
    let mut next = (*s.current_registry()).clone();
    let removed = next.loads.pop().unwrap().id;
    s.commit_registry(next).expect("commit succeeds");

    // (1) persisted + reloads without the removed device.
    let on_disk = legit_lp_scheduler::config::parse(&std::fs::read_to_string(&path).unwrap())
        .expect("persisted registry re-parses");
    assert!(!on_disk.loads.iter().any(|l| l.id == removed), "edit hit the file");
    // (2) published to the watch channel for the loop to hot-swap.
    assert!(reg_rx.has_changed().unwrap());
    assert!(!reg_rx.borrow_and_update().loads.iter().any(|l| l.id == removed), "published");
    // (3) nudged a re-solve.
    tokio::time::timeout(std::time::Duration::from_secs(1), waiting)
        .await
        .expect("commit fired solve-now")
        .unwrap();
}

#[tokio::test]
async fn w15_commit_registry_rejects_invalid_without_persisting() {
    // A bad edit must be refused by validation BEFORE it touches the file or the
    // running plan — fail loud + safe.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legit_lp.yaml");
    std::fs::write(&path, "sentinel: untouched").unwrap();
    let registry = Arc::new(watch::channel(Arc::new(example_registry())).0);
    let s = WebState {
        report: watch::channel(report()).1,
        solve_now: Arc::new(Notify::new()),
        preview: Arc::new(AtomicBool::new(false)),
        registry: registry.clone(),
        registry_path: path.clone(),
        ha: dummy_ha(),
    };
    let mut bad = (*s.current_registry()).clone();
    bad.global.planning.grid_minutes = 7; // does not divide 60
    assert!(s.commit_registry(bad).is_err(), "invalid edit rejected");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "sentinel: untouched", "file untouched");
    assert!(!registry.borrow().loads.is_empty(), "live registry unchanged");
}

// ---- CRUD API over HTTP (the wizard's backend) ----

/// A WebState backed by a real temp registry file + an example registry. Returns
/// the dir (keep it alive) and the registry channel (to assert the live swap).
fn crud_state() -> (
    WebState,
    tempfile::TempDir,
    Arc<watch::Sender<Arc<legit_lp_scheduler::config::RegistryConfig>>>,
) {
    let dir = tempfile::tempdir().unwrap();
    let registry = Arc::new(watch::channel(Arc::new(example_registry())).0);
    let s = WebState {
        report: watch::channel(report()).1,
        solve_now: Arc::new(Notify::new()),
        preview: Arc::new(AtomicBool::new(false)),
        registry: registry.clone(),
        registry_path: dir.path().join("legit_lp.yaml"),
        ha: dummy_ha(),
    };
    (s, dir, registry)
}

fn json_req(method: &str, uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// Build a `{"type":"load","config":{…}}` upsert body from the example's first
/// load, renamed to `id`.
fn load_upsert(id: &str) -> serde_json::Value {
    let mut cfg = serde_json::to_value(&example_registry().loads[0]).unwrap();
    cfg["id"] = id.into();
    serde_json::json!({ "type": "load", "config": cfg })
}

#[tokio::test]
async fn w16_get_devices_lists_loads_and_storage() {
    let (s, _dir, _reg) = crud_state();
    let resp =
        router(s).oneshot(Request::get("/api/devices").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body_of(resp).await).unwrap();
    let loads: Vec<&str> =
        v["loads"].as_array().unwrap().iter().map(|l| l["id"].as_str().unwrap()).collect();
    assert_eq!(loads, ["hot_water", "dehumidifier", "aircon"]);
    assert_eq!(v["storage"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn w17_add_device_persists_and_hot_swaps() {
    let (s, _dir, reg) = crud_state();
    let path = s.registry_path.clone();
    let resp = router(s)
        .oneshot(json_req("POST", "/api/devices", load_upsert("pool_pump")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // Persisted to disk, hot-swapped into the live registry, and returned.
    let on_disk =
        legit_lp_scheduler::config::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(on_disk.loads.iter().any(|l| l.id == "pool_pump"), "written to the file");
    assert!(
        reg.borrow().loads.iter().any(|l| l.id == "pool_pump"),
        "swapped into the live registry"
    );
    let v: serde_json::Value = serde_json::from_str(&body_of(resp).await).unwrap();
    assert_eq!(v["loads"].as_array().unwrap().len(), 4, "returns the updated list");
}

#[tokio::test]
async fn w18_add_duplicate_id_is_rejected() {
    let (s, _dir, _reg) = crud_state();
    let resp = router(s)
        .oneshot(json_req("POST", "/api/devices", load_upsert("hot_water")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT, "an existing id can't be re-added");
}

#[tokio::test]
async fn w19_edit_device_replaces_by_id() {
    let (s, _dir, reg) = crud_state();
    // Edit hot_water: bump its power. Send the full (modified) config.
    let mut cfg = serde_json::to_value(&example_registry().loads[0]).unwrap();
    cfg["capability"]["power_kw"] = serde_json::json!(9.9);
    let body = serde_json::json!({ "type": "load", "config": cfg });
    let resp = router(s).oneshot(json_req("PUT", "/api/devices/hot_water", body)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let hw = reg.borrow().loads.iter().find(|l| l.id == "hot_water").unwrap().clone();
    assert_eq!(hw.capability.power_kw.as_literal(), Some(9.9), "the edit applied live");
}

#[tokio::test]
async fn w20_edit_missing_device_is_404() {
    let (s, _dir, _reg) = crud_state();
    let resp =
        router(s).oneshot(json_req("PUT", "/api/devices/nope", load_upsert("nope"))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn w21_delete_device_removes_it() {
    let (s, _dir, reg) = crud_state();
    let resp = router(s.clone())
        .oneshot(Request::delete("/api/devices/aircon").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(!reg.borrow().loads.iter().any(|l| l.id == "aircon"), "removed from the live registry");
    // Deleting something that isn't there is a 404.
    let resp = router(s)
        .oneshot(Request::delete("/api/devices/aircon").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn w22_add_invalid_device_is_rejected_without_persisting() {
    let (s, _dir, reg) = crud_state();
    let path = s.registry_path.clone();
    // A load with planning=runtime but no runtime/program amount fails validation.
    let mut cfg = serde_json::to_value(&example_registry().loads[0]).unwrap();
    cfg["id"] = "broken".into();
    cfg["must_have"] =
        serde_json::json!({ "kind": "runtime", "window": { "start": "00:00", "end": "06:00" } });
    let body = serde_json::json!({ "type": "load", "config": cfg });
    let resp = router(s).oneshot(json_req("POST", "/api/devices", body)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "validation rejects it");
    assert!(!reg.borrow().loads.iter().any(|l| l.id == "broken"), "live registry untouched");
    assert!(!path.exists(), "nothing was written");
}

#[tokio::test]
async fn w25_storage_device_crud_round_trip() {
    // Add → edit → remove a STORAGE device over the API (the battery wizard path).
    let (s, _dir, reg) = crud_state();
    let mut cfg = serde_json::to_value(&example_registry().global.storage[0]).unwrap();
    cfg["id"] = "ev".into();
    let add = serde_json::json!({ "type": "storage", "config": cfg });
    let resp = router(s.clone()).oneshot(json_req("POST", "/api/devices", add)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "battery added");
    assert!(reg.borrow().global.storage.iter().any(|d| d.id == "ev"), "added to the live registry");

    // Edit it: change the discharge limit.
    let mut cfg2 =
        serde_json::to_value(reg.borrow().global.storage.iter().find(|d| d.id == "ev").unwrap())
            .unwrap();
    cfg2["max_discharge_kw"] = serde_json::json!(0.0); // charge-only
    let edit = serde_json::json!({ "type": "storage", "config": cfg2 });
    let resp = router(s.clone()).oneshot(json_req("PUT", "/api/devices/ev", edit)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "battery edited");
    assert_eq!(
        reg.borrow()
            .global
            .storage
            .iter()
            .find(|d| d.id == "ev")
            .unwrap()
            .max_discharge_kw
            .as_literal(),
        Some(0.0),
        "edit applied live"
    );

    // Remove it.
    let resp = router(s)
        .oneshot(Request::delete("/api/devices/ev").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "battery removed");
    assert!(!reg.borrow().global.storage.iter().any(|d| d.id == "ev"), "gone from the registry");
}

#[tokio::test]
async fn w24_panel_has_device_management_and_the_add_edit_wizard() {
    // The v2 capability: a devices-management view + a 4-step add/edit wizard with an
    // entity picker and the literal-or-entity affordance, all wired to the CRUD API.
    let (s, _tx, _n) = state();
    let resp = router(s).oneshot(Request::get("/").body(Body::empty()).unwrap()).await.unwrap();
    let html = body_of(resp).await;
    // The three views + entry points.
    assert!(
        html.contains(r#"id="view-devices""#) && html.contains(r#"id="view-wizard""#),
        "devices + wizard views present"
    );
    assert!(
        html.contains(r#"id="manageBtn""#) && html.contains(r#"id="addBtn""#),
        "Manage devices + Add device entry points"
    );
    assert!(html.contains("edit-link"), "dashboard cards carry an Edit affordance");
    // The 4 wizard steps + the 5 device kinds.
    for step in ["Type", "Connect", "Rules", "Review"] {
        assert!(html.contains(&format!("'{step}'")), "wizard step {step}");
    }
    for kind in ["Scheduled run", "Comfort range", "Keep under a limit", "Fixed program", "Battery"]
    {
        assert!(html.contains(kind), "kind card '{kind}'");
    }
    // Entity picker hits the catalog; CRUD hits the devices API.
    assert!(html.contains("./api/entities"), "entity picker queries the live catalog");
    assert!(html.contains("./api/devices"), "wizard saves via the devices CRUD API");
    // The literal-or-entity affordance (preserves no-hardcoding entity-refs on edit).
    assert!(
        html.contains("vr-toggle") && html.contains("vrOut"),
        "literal-or-entity affordance present"
    );
    // Remove uses an inline confirm.
    assert!(
        html.contains("data-askremove") && html.contains("data-remove"),
        "inline remove confirm"
    );
}

#[tokio::test]
async fn w23_entities_endpoint_is_wired_and_fails_gracefully() {
    // The catalog handler dials HA; with HA unreachable it must 502, never panic or
    // fake data. (Catalog parsing/filtering is unit-tested in ha_client.)
    let (s, _tx, _n) = state();
    let resp = router(s)
        .oneshot(Request::get("/api/entities?domains=switch,sensor").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
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

#[tokio::test]
async fn w13_preview_banner_does_not_overclaim_safety() {
    // The preview banner must not overclaim safety: in live preview (no dry-run) it
    // warns that Optimiser-mode devices (the batteries) are STILL actuated, rather
    // than the misleading "nothing is controlled".
    let (s, _tx, _n) = state();
    let resp = router(s).oneshot(Request::get("/").body(Body::empty()).unwrap()).await.unwrap();
    let html = body_of(resp).await;
    assert!(
        !html.contains("No devices are being controlled"),
        "the misleading 'nothing controlled' preview text is gone"
    );
    assert!(
        html.contains("still controlled live"),
        "live preview warns that Optimiser devices still actuate"
    );
    // Durations/labels derive from the report's grid step length, not a hard-coded 15.
    assert!(html.contains("r.grid_minutes"), "time math uses grid_minutes");
}
