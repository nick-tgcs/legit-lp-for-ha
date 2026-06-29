//! Binary wiring: env options -> HaClient -> web server + solve loop.
//! The only place `anyhow` and the wall clock are allowed.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use legit_lp_scheduler::cycle::Cycle;
use legit_lp_scheduler::ha_client::{resolve_endpoint, HaClient};
use legit_lp_scheduler::lp::LpPlanner;
use legit_lp_scheduler::profile::Profiles;
use legit_lp_scheduler::status::{Severity, SolveReport};
use legit_lp_scheduler::web::{router, WebState};
use tokio::sync::{watch, Notify};

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let interval: u64 = env("SCHED_INTERVAL_SECONDS").and_then(|v| v.parse().ok()).unwrap_or(60);
    let dry_run = env("SCHED_DRY_RUN").map(|v| v != "false").unwrap_or(true);
    let tz: chrono_tz::Tz =
        env("SCHED_TIME_ZONE").and_then(|v| v.parse().ok()).unwrap_or(chrono_tz::Australia::Sydney);
    let loads_path = env("SCHED_LOADS_CONFIG").unwrap_or("/config/legit_lp.yaml".into());
    let port: u16 = env("SCHED_WEB_PORT").and_then(|v| v.parse().ok()).unwrap_or(8099);

    // HA connection: explicit url+token (when set), else the Supervisor proxy.
    // `env()` already drops empties; `resolve_endpoint` also drops bashio's
    // "null" sentinel and rejects non-absolute URLs, so an unset `hass_url`
    // can't poison the base into "null/api".
    let (base, token) =
        resolve_endpoint(env("SCHED_HASS_URL"), env("SCHED_TOKEN"), env("SUPERVISOR_TOKEN"));
    tracing::info!(base = %base, "HA API endpoint resolved");
    // Arc so the solve loop and the panel's entity-catalog endpoint share one client.
    let ha = Arc::new(HaClient::new(base, token));

    let registry = legit_lp_scheduler::config::parse(&legit_lp_scheduler::config::load_registry(
        std::path::Path::new(&loads_path),
    )?)?;
    let planner = LpPlanner {
        grid_minutes: registry.global.planning.grid_minutes,
        horizon_hours: registry.global.planning.horizon_hours,
    };
    let profile_path = std::path::PathBuf::from(env("SCHED_DATA_DIR").unwrap_or("/data".into()))
        .join("profile.json");
    let mut profiles = Profiles::load(&profile_path);
    // Runtime preview toggle shared between the panel (checkbox -> POST /api/preview)
    // and the solve loop. Starts off; not persisted across restarts (the optional
    // HA `preview_entity` boolean is the persistent path).
    let preview = Arc::new(AtomicBool::new(false));
    // Registry hot-swap channel: the panel publishes device add/edit/remove edits
    // here (validated + atomically persisted first), and this loop applies them to
    // the running `Cycle` before the next solve — no add-on restart required.
    let (registry_tx, mut registry_rx) = watch::channel(Arc::new(registry.clone()));
    let registry_tx = Arc::new(registry_tx);
    let mut cycle = Cycle {
        registry,
        planner,
        dry_run,
        profile_path: Some(profile_path),
        preview_override: preview.clone(),
    };

    let (tx, rx) = watch::channel(SolveReport::default());
    let solve_now = Arc::new(Notify::new());
    let web = WebState {
        report: rx,
        solve_now: solve_now.clone(),
        preview: preview.clone(),
        registry: registry_tx.clone(),
        registry_path: std::path::PathBuf::from(&loads_path),
        ha: ha.clone(),
        write_lock: Arc::new(tokio::sync::Mutex::new(())),
    };
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    tokio::spawn(async move {
        axum::serve(listener, router(web)).await.ok();
    });
    tracing::info!("panel on :{port}; interval {interval}s; dry_run={dry_run}");

    let mut tick = tokio::time::interval(Duration::from_secs(interval));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip); // single-flight
                                                                          // Last successfully-solved report, kept so a failed solve can show the previous
                                                                          // plan marked stale instead of blanking the panel to zeros.
    let mut last_good: Option<SolveReport> = None;
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = solve_now.notified() => { tracing::info!("solve-now nudge"); }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("SIGTERM/SIGINT: exiting; devices left as-is (reconcile on restart)");
                return Ok(());
            }
        }
        // Apply any registry edit published by the panel before solving. Rebuild the
        // planner too, so a change to planning grid/horizon takes effect as well.
        if registry_rx.has_changed().unwrap_or(false) {
            let next = registry_rx.borrow_and_update().clone();
            cycle.planner = LpPlanner {
                grid_minutes: next.global.planning.grid_minutes,
                horizon_hours: next.global.planning.horizon_hours,
            };
            cycle.registry = (*next).clone();
            tracing::info!("registry reloaded from a panel edit; applied to this solve");
        }
        let now = chrono::Utc::now().with_timezone(&tz);
        let report = cycle.run(ha.as_ref(), &mut profiles, now).await;
        for line in report.log_lines() {
            tracing::info!("{line}");
        }
        // Each triaged alert at its own level, with a greppable `ALERT[<sev>]` prefix
        // so a critical is one `grep` away in the add-on log.
        for a in &report.alerts {
            match a.severity {
                Severity::Critical => tracing::error!("{}", a.log_line()),
                Severity::Warning => tracing::warn!("{}", a.log_line()),
                Severity::Info => tracing::info!("{}", a.log_line()),
            }
        }
        // On a SOLVE failure, keep the last good plan on screen, marked stale, with
        // this cycle's fresh context (price/now) and the critical alert overlaid —
        // never blank to zeros while a prior plan exists. A successful, non-empty
        // solve becomes the new fallback.
        let to_send = if report.is_solver_failure() {
            match &last_good {
                // Keep the last good plan, marked stale, with this cycle's fresh context.
                Some(prev) => prev.stale_view(&report),
                None => report, // no good plan yet — show the empty report + banner
            }
        } else {
            if !report.grid.is_empty() {
                last_good = Some(report.clone()); // a real solved plan; remember it
            }
            report
        };
        tx.send_replace(to_send);
    }
}
