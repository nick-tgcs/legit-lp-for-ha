//! Binary wiring: env options -> HaClient -> web server + solve loop.
//! The only place `anyhow` and the wall clock are allowed.

use std::sync::Arc;
use std::time::Duration;

use legit_lp_scheduler::cycle::Cycle;
use legit_lp_scheduler::ha_client::{resolve_endpoint, HaClient};
use legit_lp_scheduler::lp::LpPlanner;
use legit_lp_scheduler::profile::Profiles;
use legit_lp_scheduler::status::SolveReport;
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
    let ha = HaClient::new(base, token);

    let registry = legit_lp_scheduler::config::parse(&legit_lp_scheduler::config::load_or_seed(
        std::path::Path::new(&loads_path),
    )?)?;
    let planner = LpPlanner {
        grid_minutes: registry.global.planning.grid_minutes,
        horizon_hours: registry.global.planning.horizon_hours,
    };
    let profile_path = std::path::PathBuf::from(env("SCHED_DATA_DIR").unwrap_or("/data".into()))
        .join("profile.json");
    let mut profiles = Profiles::load(&profile_path);
    let cycle = Cycle { registry, planner, dry_run, profile_path: Some(profile_path) };

    let (tx, rx) = watch::channel(SolveReport::default());
    let solve_now = Arc::new(Notify::new());
    let web = WebState { report: rx, solve_now: solve_now.clone() };
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    tokio::spawn(async move {
        axum::serve(listener, router(web)).await.ok();
    });
    tracing::info!("panel on :{port}; interval {interval}s; dry_run={dry_run}");

    let mut tick = tokio::time::interval(Duration::from_secs(interval));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip); // single-flight
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = solve_now.notified() => { tracing::info!("solve-now nudge"); }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("SIGTERM/SIGINT: exiting; devices left as-is (reconcile on restart)");
                return Ok(());
            }
        }
        let now = chrono::Utc::now().with_timezone(&tz);
        let report = cycle.run(&ha, &mut profiles, now).await;
        for line in report.log_lines() {
            tracing::info!("{line}");
        }
        tx.send_replace(report);
    }
}
