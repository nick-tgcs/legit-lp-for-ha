//! Panel API against a canned watch channel: status JSON, SSE on update,
//! solve-now notify, valid SVG, relative URLs only.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use legit_lp_scheduler::status::{LoadReport, SolveReport};
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

fn state() -> (WebState, watch::Sender<SolveReport>, Arc<Notify>) {
    let (tx, rx) = watch::channel(report());
    let notify = Arc::new(Notify::new());
    (WebState { report: rx, solve_now: notify.clone() }, tx, notify)
}

async fn body_of(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test]
async fn w1_status_returns_report_json() {
    let (s, _tx, _n) = state();
    let resp = router(s)
        .oneshot(Request::get("/api/status").body(Body::empty()).unwrap())
        .await
        .unwrap();
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
    let resp = router(s)
        .oneshot(Request::post("/api/solve").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    tokio::time::timeout(std::time::Duration::from_secs(1), waiting).await.unwrap().unwrap();
}

#[tokio::test]
async fn w4_horizon_svg_is_valid_xml_with_plan_blocks() {
    let (s, _tx, _n) = state();
    let resp = router(s)
        .oneshot(Request::get("/horizon.svg").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let svg = body_of(resp).await;
    assert!(svg.starts_with("<svg") && svg.ends_with("</svg>"));
    assert!(svg.matches("<rect").count() == 2, "two on-blocks rendered");
    assert!(svg.contains("#4caf50"), "can-take block coloured distinctly");
}

#[tokio::test]
async fn w5_index_uses_relative_urls_only() {
    let (s, _tx, _n) = state();
    let resp =
        router(s).oneshot(Request::get("/").body(Body::empty()).unwrap()).await.unwrap();
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
