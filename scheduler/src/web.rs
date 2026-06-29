//! Axum ingress panel: status JSON, SSE, horizon SVG, solve-now, /health.
//! All URLs relative (HA ingress path prefix). One process with the loop:
//! handlers read the latest SolveReport from a watch channel.

use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::Json;
use futures_core::Stream;
use tokio::sync::{watch, Notify};

use crate::config::{self, LoadConfig, RegistryConfig, StorageConfig};
use crate::error::SchedulerError;
use crate::ha_client::{filter_by_domains, HaClient};
use crate::status::SolveReport;

#[derive(Clone)]
pub struct WebState {
    pub report: watch::Receiver<SolveReport>,
    pub solve_now: Arc<Notify>,
    /// Runtime preview toggle shared with the solve loop. The panel checkbox
    /// POSTs /api/preview to flip it; the loop reads it each tick. Preview solves
    /// observe-only loads for the panel but never controls them.
    pub preview: Arc<AtomicBool>,
    /// The live registry the solve loop runs on. The panel reads the current value
    /// (`borrow`) to render/edit devices and publishes a new one on save; the main
    /// loop watches the receiving end and hot-swaps `Cycle.registry` — so a device
    /// add/edit/remove takes effect on the next solve WITHOUT an add-on restart.
    /// `Arc` so `WebState` stays `Clone` (a `watch::Sender` is not).
    pub registry: Arc<watch::Sender<Arc<RegistryConfig>>>,
    /// Where the registry is persisted (the loader's path). Saves are atomic.
    pub registry_path: PathBuf,
    /// Live HA client, used only to serve the entity catalog (`/api/entities`) to
    /// the wizard's entity picker. The solve loop has its own borrow of the client.
    pub ha: Arc<HaClient>,
}

impl WebState {
    /// The registry the solve loop is currently running on.
    pub fn current_registry(&self) -> Arc<RegistryConfig> {
        self.registry.borrow().clone()
    }

    /// Commit an edited registry: **validate + atomically persist**, publish it for
    /// the solve loop to hot-swap, and nudge an immediate re-solve. Validation +
    /// the atomic temp-file write live in `config::save_registry`, so a rejected
    /// or interrupted save never corrupts the live file or the in-memory plan. The
    /// new registry only becomes visible to the loop after the write succeeds.
    pub fn commit_registry(&self, next: RegistryConfig) -> Result<(), SchedulerError> {
        config::save_registry(&self.registry_path, &next)?;
        self.registry.send_replace(Arc::new(next));
        self.solve_now.notify_one();
        Ok(())
    }
}

pub fn router(state: WebState) -> axum::Router {
    axum::Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .route("/api/status", get(status))
        .route("/api/events", get(events))
        .route("/api/solve", post(solve_now))
        .route("/api/preview", post(set_preview))
        // Device CRUD (the wizard) — every write hot-swaps the running registry.
        .route("/api/devices", get(list_devices).post(add_device))
        .route("/api/devices/{id}", put(edit_device).delete(delete_device))
        // Live HA entity catalog for the wizard's entity picker.
        .route("/api/entities", get(list_entities))
        .route("/horizon.svg", get(horizon))
        .with_state(state)
}

/// The device lists the panel renders + the wizard edits — the registry's loads
/// and storage, exactly as persisted (config is the contract; the UI owns it).
#[derive(serde::Serialize)]
struct DevicesView {
    loads: Vec<LoadConfig>,
    storage: Vec<StorageConfig>,
}

impl DevicesView {
    fn of(r: &RegistryConfig) -> Self {
        DevicesView { loads: r.loads.clone(), storage: r.global.storage.clone() }
    }
}

async fn list_devices(State(s): State<WebState>) -> Json<DevicesView> {
    Json(DevicesView::of(&s.current_registry()))
}

/// A device to add or replace. Adjacently tagged so the wizard sends
/// `{"type":"load"|"storage","config":{…}}` — the config IS the engine contract
/// (D2: the wizard provides the full explicit mapping), validated on commit.
#[derive(serde::Deserialize)]
#[serde(tag = "type", content = "config", rename_all = "snake_case")]
enum DeviceUpsert {
    Load(Box<LoadConfig>),
    Storage(Box<StorageConfig>),
}

impl DeviceUpsert {
    fn id(&self) -> &str {
        match self {
            DeviceUpsert::Load(l) => &l.id,
            DeviceUpsert::Storage(d) => &d.id,
        }
    }
}

/// Persist an edited registry and return the new device list, or a 400 with the
/// validation message (the commit validated + rolled back without touching the file).
fn commit_or_error(s: &WebState, next: RegistryConfig) -> Response {
    match s.commit_registry(next) {
        Ok(()) => (StatusCode::OK, Json(DevicesView::of(&s.current_registry()))).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

async fn add_device(State(s): State<WebState>, Json(dev): Json<DeviceUpsert>) -> Response {
    let mut next = (*s.current_registry()).clone();
    let id = dev.id().to_string();
    if next.loads.iter().any(|l| l.id == id) || next.global.storage.iter().any(|d| d.id == id) {
        return (StatusCode::CONFLICT, format!("a device with id '{id}' already exists"))
            .into_response();
    }
    match dev {
        DeviceUpsert::Load(l) => next.loads.push(*l),
        DeviceUpsert::Storage(d) => next.global.storage.push(*d),
    }
    commit_or_error(&s, next)
}

async fn edit_device(
    State(s): State<WebState>,
    Path(id): Path<String>,
    Json(dev): Json<DeviceUpsert>,
) -> Response {
    let mut next = (*s.current_registry()).clone();
    // A rename must not collide with ANY other device — ids are a single namespace across
    // loads + storage (validation enforces this too, but a 409 here is the clearer error and
    // catches a cross-type collision before the commit). Exclude the device being replaced.
    let new_id = dev.id().to_string();
    if new_id != id
        && (next.loads.iter().any(|l| l.id == new_id)
            || next.global.storage.iter().any(|d| d.id == new_id))
    {
        return (StatusCode::CONFLICT, format!("a device with id '{new_id}' already exists"))
            .into_response();
    }
    // Replace the device at the path id (the body may carry a new id = rename).
    let replaced = match dev {
        DeviceUpsert::Load(l) => match next.loads.iter_mut().find(|x| x.id == id) {
            Some(slot) => {
                *slot = *l;
                true
            }
            None => false,
        },
        DeviceUpsert::Storage(d) => match next.global.storage.iter_mut().find(|x| x.id == id) {
            Some(slot) => {
                *slot = *d;
                true
            }
            None => false,
        },
    };
    if !replaced {
        return (StatusCode::NOT_FOUND, format!("no device with id '{id}'")).into_response();
    }
    commit_or_error(&s, next)
}

async fn delete_device(State(s): State<WebState>, Path(id): Path<String>) -> Response {
    let mut next = (*s.current_registry()).clone();
    let before = next.loads.len() + next.global.storage.len();
    next.loads.retain(|l| l.id != id);
    next.global.storage.retain(|d| d.id != id);
    if next.loads.len() + next.global.storage.len() == before {
        return (StatusCode::NOT_FOUND, format!("no device with id '{id}'")).into_response();
    }
    commit_or_error(&s, next)
}

#[derive(serde::Deserialize)]
struct EntitiesQuery {
    /// Comma-separated HA domains to keep (e.g. `switch,sensor,climate,select`).
    /// Empty/absent = all domains.
    domains: Option<String>,
}

/// The live HA entity catalog for the wizard's entity picker, filtered to the
/// requested domains. A read failure is a 502 (HA unreachable), never fake data.
async fn list_entities(State(s): State<WebState>, Query(q): Query<EntitiesQuery>) -> Response {
    let domains: Vec<String> = q
        .domains
        .unwrap_or_default()
        .split(',')
        .map(|d| d.trim())
        .filter(|d| !d.is_empty())
        .map(String::from)
        .collect();
    match s.ha.list_states().await {
        Ok(items) => (StatusCode::OK, Json(filter_by_domains(items, &domains))).into_response(),
        Err(e) => {
            (StatusCode::BAD_GATEWAY, format!("could not read HA entities: {e}")).into_response()
        }
    }
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

#[derive(serde::Deserialize)]
struct PreviewQuery {
    on: bool,
}

/// Set the runtime preview toggle (the in-panel checkbox) to an explicit state
/// and nudge an immediate re-solve so the change is reflected at once. Preview
/// solves observe-only loads for the panel but never controls a device.
async fn set_preview(State(s): State<WebState>, Query(q): Query<PreviewQuery>) -> &'static str {
    s.preview.store(q.on, Ordering::Relaxed);
    s.solve_now.notify_one();
    "ok"
}

async fn events(State(s): State<WebState>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
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

/// Server-rendered multi-lane horizon chart: price, power (PV + net grid),
/// battery SoC, and per-load run windows — all sharing one time axis. The
/// render is a pure function of the report so lanes degrade gracefully when
/// their series are absent (a minimal report draws only load rows + axis).
async fn horizon(State(s): State<WebState>) -> impl IntoResponse {
    let svg = render_horizon(&s.report.borrow());
    ([(axum::http::header::CONTENT_TYPE, "image/svg+xml")], svg)
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// "HH:MM" from an RFC3339 grid timestamp, falling back to the step index.
fn hhmm(ts: &str, fallback: usize) -> String {
    match chrono::DateTime::parse_from_rfc3339(ts) {
        Ok(t) => t.format("%H:%M").to_string(),
        Err(_) => fallback.to_string(),
    }
}

pub fn render_horizon(r: &SolveReport) -> String {
    const W: f64 = 1000.0;
    const LX: f64 = 96.0; // left gutter for lane labels
    const RX: f64 = 14.0; // right margin
    let n = r.grid.len();
    if n == 0 {
        return format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W:.0} 44" font-family="Roboto,system-ui,sans-serif" font-size="11"><text x="10" y="26" fill="#888">no plan solved yet</text></svg>"##
        );
    }
    let pw = W - LX - RX;
    let bw = pw / n as f64;
    let x = |t: f64| LX + t / n as f64 * pw;
    let cx = |t: usize| x(t as f64) + bw / 2.0;
    // Map a value in [lo,hi] to a y inside a lane (top..top+h), inverted.
    let scale = |v: f64, lo: f64, hi: f64, top: f64, h: f64| {
        if (hi - lo).abs() < 1e-9 {
            top + h / 2.0
        } else {
            top + h * (1.0 - (v - lo) / (hi - lo))
        }
    };

    let mut body = String::new();
    let pad = 12.0;
    let plot_top = 6.0;
    let mut y = plot_top;

    // ---- price lane (import line + feed-in line) ----
    if r.price.iter().any(|p| p.is_some()) {
        let lh = 80.0;
        let top = y;
        let mut lo = f64::INFINITY;
        let mut hi = f64::NEG_INFINITY;
        for v in r.price.iter().flatten() {
            lo = lo.min(*v);
            hi = hi.max(*v);
        }
        for f in &r.feedin {
            lo = lo.min(*f);
            hi = hi.max(*f);
        }
        if !lo.is_finite() {
            lo = 0.0;
            hi = 1.0;
        }
        lo = lo.min(0.0);
        hi += (hi - lo).max(0.01) * 0.12;
        body += &format!(
            r##"<text x="6" y="{:.0}" fill="#888">price</text><text x="6" y="{:.0}" fill="#aaa" font-size="9">$/kWh</text>"##,
            top + 13.0,
            top + 25.0
        );
        let zy = scale(0.0, lo, hi, top, lh);
        body += &format!(
            r##"<line class="zero" x1="{LX:.0}" y1="{zy:.1}" x2="{:.0}" y2="{zy:.1}" stroke="#ddd" stroke-width="0.5"/>"##,
            LX + pw
        );
        // Import: a segmented polyline, broken at unknown (None) steps.
        let mut seg: Vec<String> = Vec::new();
        for (t, p) in r.price.iter().enumerate() {
            if let Some(v) = p {
                seg.push(format!("{:.1},{:.1}", cx(t), scale(*v, lo, hi, top, lh)));
            } else {
                if seg.len() >= 2 {
                    body += &format!(
                        r##"<polyline class="price-import" fill="none" stroke="#5c6bc0" stroke-width="2" points="{}"/>"##,
                        seg.join(" ")
                    );
                }
                seg.clear();
            }
        }
        if seg.len() >= 2 {
            body += &format!(
                r##"<polyline class="price-import" fill="none" stroke="#5c6bc0" stroke-width="2" points="{}"/>"##,
                seg.join(" ")
            );
        }
        if r.feedin.len() == n {
            let pts: String = r
                .feedin
                .iter()
                .enumerate()
                .map(|(t, f)| format!("{:.1},{:.1}", cx(t), scale(*f, lo, hi, top, lh)))
                .collect::<Vec<_>>()
                .join(" ");
            body += &format!(
                r##"<polyline class="price-feedin" fill="none" stroke="#26a69a" stroke-width="1" stroke-dasharray="3 2" points="{pts}"/>"##
            );
        }
        y = top + lh + pad;
    }

    // ---- power lane (PV area, net grid bars, baseload line) ----
    if r.pv.len() == n || r.grid_kw.len() == n {
        let lh = 84.0;
        let top = y;
        let mut hi = 0.1f64;
        let mut lo = 0.0f64;
        for v in &r.pv {
            hi = hi.max(*v);
        }
        for v in &r.baseload {
            hi = hi.max(*v);
        }
        for v in &r.grid_kw {
            hi = hi.max(*v);
            lo = lo.min(*v);
        }
        hi *= 1.12;
        let zy = scale(0.0, lo, hi, top, lh);
        body += &format!(
            r##"<text x="6" y="{:.0}" fill="#888">power</text><text x="6" y="{:.0}" fill="#aaa" font-size="9">kW</text>"##,
            top + 13.0,
            top + 25.0
        );
        body += &format!(
            r##"<line class="zero" x1="{LX:.0}" y1="{zy:.1}" x2="{:.0}" y2="{zy:.1}" stroke="#ddd" stroke-width="0.5"/>"##,
            LX + pw
        );
        if r.grid_kw.len() == n {
            for (t, g) in r.grid_kw.iter().enumerate() {
                if g.abs() < 1e-3 {
                    continue;
                }
                let gy = scale(*g, lo, hi, top, lh);
                let (ry, rh, cls, col) = if *g > 0.0 {
                    (gy, zy - gy, "grid-import", "#ef9a9a")
                } else {
                    (zy, gy - zy, "grid-export", "#a5d6a7")
                };
                body += &format!(
                    r##"<rect class="{cls}" x="{:.1}" y="{:.1}" width="{:.1}" height="{:.1}" fill="{col}"/>"##,
                    x(t as f64),
                    ry,
                    bw.max(0.5),
                    rh.max(0.0)
                );
            }
        }
        if r.pv.len() == n && r.pv.iter().any(|v| *v > 0.0) {
            let mut pts = format!("{:.1},{:.1} ", x(0.0), zy);
            for (t, v) in r.pv.iter().enumerate() {
                pts += &format!("{:.1},{:.1} ", cx(t), scale(*v, lo, hi, top, lh));
            }
            pts += &format!("{:.1},{:.1}", x(n as f64), zy);
            body += &format!(
                r##"<polygon class="pv-area" points="{pts}" fill="#ffb300" fill-opacity="0.25"/>"##
            );
        }
        if r.baseload.len() == n {
            let line: String = r
                .baseload
                .iter()
                .enumerate()
                .map(|(t, v)| format!("{:.1},{:.1}", cx(t), scale(*v, lo, hi, top, lh)))
                .collect::<Vec<_>>()
                .join(" ");
            body += &format!(
                r##"<polyline class="baseload-line" fill="none" stroke="#b0bec5" stroke-width="1" points="{line}"/>"##
            );
        }
        y = top + lh + pad;
    }

    // ---- storage lanes: one per device (SoC area + reserve + action strip) ----
    for b in &r.storage {
        if b.soc_kwh.len() >= 2 {
            let lh = 64.0;
            let top = y;
            let cap = b.capacity_kwh.max(b.max_soc_kwh).max(1e-3);
            let bot = top + lh;
            body += &format!(
                r##"<text x="6" y="{:.0}" fill="#888">{}</text><text x="6" y="{:.0}" fill="#aaa" font-size="9">{:.1}/{:.0} kWh</text>"##,
                top + 13.0,
                esc(&b.id),
                top + 25.0,
                b.soc_now_kwh,
                cap
            );
            // SoC boundary series has one point per grid edge (len n+1).
            let m = b.soc_kwh.len();
            let mut pts = format!("{:.1},{:.1} ", x(0.0), bot);
            for (i, s) in b.soc_kwh.iter().enumerate() {
                pts += &format!("{:.1},{:.1} ", x(i as f64), scale(*s, 0.0, cap, top, lh));
            }
            pts += &format!("{:.1},{:.1}", x((m - 1) as f64), bot);
            body += &format!(
                r##"<polygon class="soc-area" points="{pts}" fill="#42a5f5" fill-opacity="0.25"/>"##
            );
            let line: String = b
                .soc_kwh
                .iter()
                .enumerate()
                .map(|(i, s)| format!("{:.1},{:.1}", x(i as f64), scale(*s, 0.0, cap, top, lh)))
                .collect::<Vec<_>>()
                .join(" ");
            body += &format!(
                r##"<polyline class="soc-line" fill="none" stroke="#1e88e5" stroke-width="2" points="{line}"/>"##
            );
            let ry = scale(b.min_soc_kwh, 0.0, cap, top, lh);
            body += &format!(
                r##"<line class="soc-reserve" x1="{LX:.0}" y1="{ry:.1}" x2="{:.0}" y2="{ry:.1}" stroke="#ef5350" stroke-width="0.75" stroke-dasharray="4 3"/>"##,
                LX + pw
            );
            // Action strip: green where charging, amber where discharging.
            let sy = bot + 2.0;
            for t in 0..n {
                let c = b.charge_kw.get(t).copied().unwrap_or(0.0);
                let d = b.discharge_kw.get(t).copied().unwrap_or(0.0);
                let (cls, col) = if c > 1e-3 {
                    ("batt-charge", "#66bb6a")
                } else if d > 1e-3 {
                    ("batt-discharge", "#ffa726")
                } else {
                    continue;
                };
                body += &format!(
                    r##"<rect class="{cls}" x="{:.1}" y="{:.1}" width="{:.1}" height="5" fill="{col}"/>"##,
                    x(t as f64),
                    sy,
                    bw.max(0.5)
                );
            }
            y = bot + 9.0 + pad;
        }
    }

    // ---- load lanes (planned on / can-take blocks) ----
    let row_h = 22.0;
    for l in &r.loads {
        let top = y;
        body +=
            &format!(r##"<text x="6" y="{:.0}" fill="#888">{}</text>"##, top + 15.0, esc(&l.id));
        for (t, on) in l.on.iter().enumerate() {
            if *on {
                let (cls, col) = if l.ct.get(t) == Some(&true) {
                    ("load-ct", "#4caf50")
                } else {
                    ("load-on", "#03a9f4")
                };
                body += &format!(
                    r##"<rect class="{cls}" x="{:.1}" y="{:.1}" width="{:.1}" height="14" fill="{col}"/>"##,
                    x(t as f64),
                    top + 4.0,
                    bw.max(0.5)
                );
            }
        }
        y = top + row_h;
    }

    // ---- shared time axis: vertical gridlines, hour ticks, now marker ----
    let plot_bot = y + 2.0;
    let total_h = plot_bot + 20.0;
    let step = (n / 8).max(1);
    let mut axis = String::new();
    let mut t = 0;
    while t <= n {
        let xt = x(t as f64);
        axis += &format!(
            r##"<line class="grid-v" x1="{xt:.1}" y1="{plot_top:.0}" x2="{xt:.1}" y2="{plot_bot:.1}" stroke="#eee" stroke-width="0.5"/>"##
        );
        let label = if t < n { hhmm(&r.grid[t], t) } else { String::new() };
        axis += &format!(
            r##"<text class="tick" x="{xt:.1}" y="{:.1}" fill="#999" font-size="9" text-anchor="middle">{label}</text>"##,
            plot_bot + 12.0
        );
        t += step;
    }
    let nx = x(0.0);
    axis += &format!(
        r##"<line class="now-line" x1="{nx:.1}" y1="{plot_top:.0}" x2="{nx:.1}" y2="{plot_bot:.1}" stroke="#ff9800" stroke-width="1.5"/><text class="now" x="{:.1}" y="{:.0}" fill="#ff9800" font-size="9">now</text>"##,
        nx + 3.0,
        plot_top + 8.0
    );

    // ---- hover layer: a transparent full-height band per step, each carrying a
    // <title> readout of that step's values. The page inlines this SVG so the
    // browser shows the readout natively on hover (an <img>-loaded SVG cannot).
    let mut hover = String::new();
    let band_h = plot_bot - plot_top;
    for t in 0..n {
        let mut tip = hhmm(&r.grid[t], t);
        if let Some(Some(p)) = r.price.get(t) {
            tip += &format!("\nimport {p:.3} $/kWh");
        }
        if let Some(f) = r.feedin.get(t) {
            tip += &format!("\nfeed-in {f:.3} $/kWh");
        }
        if matches!(r.pv.get(t), Some(v) if *v > 0.0) {
            tip += &format!("\nsolar {:.2} kW", r.pv[t]);
        }
        if let Some(g) = r.grid_kw.get(t) {
            let dir = if *g >= 0.0 { "import" } else { "export" };
            tip += &format!("\ngrid {g:+.2} kW ({dir})");
        }
        for b in &r.storage {
            if let Some(s) = b.soc_kwh.get(t) {
                let act = if matches!(b.charge_kw.get(t), Some(c) if *c > 1e-3) {
                    format!(" (charging {:.1} kW)", b.charge_kw[t])
                } else if matches!(b.discharge_kw.get(t), Some(d) if *d > 1e-3) {
                    format!(" (discharging {:.1} kW)", b.discharge_kw[t])
                } else {
                    String::new()
                };
                tip += &format!("\n{} {s:.1} kWh{act}", b.id);
            }
        }
        let on: Vec<&str> =
            r.loads.iter().filter(|l| l.on.get(t) == Some(&true)).map(|l| l.id.as_str()).collect();
        if !on.is_empty() {
            tip += &format!("\non: {}", on.join(", "));
        }
        hover += &format!(
            r##"<rect class="hit" x="{:.1}" y="{plot_top:.0}" width="{:.1}" height="{band_h:.1}" fill="transparent"><title>{}</title></rect>"##,
            x(t as f64),
            bw.max(0.5),
            esc(&tip),
        );
    }

    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W:.0} {total_h:.0}" font-family="Roboto,system-ui,sans-serif" font-size="11">{axis}{body}{hover}</svg>"##
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::{LoadReport, SolveReport};

    #[test]
    fn horizon_emits_a_hover_band_with_a_readout_per_step() {
        let r = SolveReport {
            grid: vec!["2026-06-10T10:00:00+10:00".into(), "2026-06-10T10:15:00+10:00".into()],
            price: vec![Some(0.142), Some(0.20)],
            feedin: vec![0.05, 0.05],
            loads: vec![LoadReport {
                id: "hot_water".into(),
                planning: "runtime".into(),
                authority: false,
                running: Some(false),
                action: "NoChange".into(),
                reason: "observe-only".into(),
                unmet: 0.0,
                executed: false,
                on: vec![true, false],
                ct: vec![false, false],
                reasoning: Default::default(),
            }],
            ..Default::default()
        };
        let svg = render_horizon(&r);
        assert_eq!(svg.matches(r#"class="hit""#).count(), 2, "one hit-band per step");
        assert!(svg.contains("<title>10:00"), "readout starts with the step time");
        assert!(svg.contains("import 0.142 $/kWh"), "readout carries the price");
        assert!(svg.contains("on: hot_water"), "readout lists the running loads");
    }
}
