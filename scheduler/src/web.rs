//! Axum ingress panel: status JSON, SSE, horizon SVG, solve-now, /health.
//! All URLs relative (HA ingress path prefix). One process with the loop:
//! handlers read the latest SolveReport from a watch channel.

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::State;
use axum::response::sse::{Event, Sse};
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::Json;
use futures_core::Stream;
use tokio::sync::{watch, Notify};

use crate::status::SolveReport;

#[derive(Clone)]
pub struct WebState {
    pub report: watch::Receiver<SolveReport>,
    pub solve_now: Arc<Notify>,
}

pub fn router(state: WebState) -> axum::Router {
    axum::Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .route("/api/status", get(status))
        .route("/api/events", get(events))
        .route("/api/solve", post(solve_now))
        .route("/horizon.svg", get(horizon))
        .with_state(state)
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../assets/index.html"))
}

async fn health() -> &'static str {
    "ok"
}

async fn status(State(s): State<WebState>) -> Json<SolveReport> {
    Json(s.report.borrow().clone())
}

async fn solve_now(State(s): State<WebState>) -> &'static str {
    s.solve_now.notify_one();
    "solving"
}

async fn events(
    State(s): State<WebState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut rx = s.report.clone();
    let stream = async_stream::stream! {
        loop {
            if rx.changed().await.is_err() {
                break;
            }
            let json = serde_json::to_string(&*rx.borrow()).unwrap_or_default();
            yield Ok(Event::default().data(json));
        }
    };
    Sse::new(stream)
}

/// Server-rendered horizon: load rows of planned on-blocks. Deliberately
/// minimal-but-valid SVG; the price curve lands with calibration polish.
async fn horizon(State(s): State<WebState>) -> impl IntoResponse {
    let r = s.report.borrow().clone();
    let n = r.grid.len().max(1);
    let row_h = 22;
    let w = 1000;
    let h = 30 + row_h * r.loads.len().max(1);
    let mut svg = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w} {h}" font-family="Roboto,sans-serif" font-size="11">"#
    );
    for (li, l) in r.loads.iter().enumerate() {
        let y = 20 + li * row_h;
        svg += &format!(r##"<text x="0" y="{}" fill="#888">{}</text>"##, y + 12, l.id);
        for (t, on) in l.on.iter().enumerate() {
            if *on {
                let x = 120.0 + (t as f64 / n as f64) * (w as f64 - 120.0);
                let bw = (w as f64 - 120.0) / n as f64;
                let color = if l.ct.get(t) == Some(&true) { "#4caf50" } else { "#03a9f4" };
                svg += &format!(
                    r#"<rect x="{x:.1}" y="{y}" width="{bw:.1}" height="16" fill="{color}"/>"#
                );
            }
        }
    }
    svg += "</svg>";
    ([(axum::http::header::CONTENT_TYPE, "image/svg+xml")], svg)
}
