//! The REAL binary against a stub HA (wiremock): boots, parses the test
//! registry fixture, solves, serves the panel — and in dry-run POSTs nothing.

use std::io::Read;
use std::time::Duration;

use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn stub_ha() -> MockServer {
    let server = MockServer::start().await;
    let states: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/states.json")).unwrap();
    for (entity, body) in states.as_object().unwrap() {
        Mock::given(method("GET"))
            .and(path_regex(format!("^/api/states/{}$", regex::escape(entity))))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
    }
    Mock::given(method("GET"))
        .and(path_regex("^/api/states/input_boolean.grid_power_use_lp_scheduler$"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"state":"on","attributes":{}})),
        )
        .mount(&server)
        .await;
    // Preview HA boolean OFF: the only thing that can enable preview in this test
    // is the in-panel checkbox (POST /api/preview), so e1 exercises the full
    // panel -> solve-loop wiring through the real binary.
    Mock::given(method("GET"))
        .and(path_regex("^/api/states/input_boolean.lp_scheduler_preview$"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"state":"off","attributes":{}})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex("^/api/states/sensor.beckton_general_forecast$"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                serde_json::from_str::<serde_json::Value>(include_str!(
                    "fixtures/forecast_amber.json"
                ))
                .unwrap(),
            ),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex("^/api/states/sensor.beckton_feed_in_forecast$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "state": "0.05",
            "attributes": {"forecasts": [
                {"per_kwh": 0.06, "start_time": "2026-06-10T00:00:00+00:00",
                 "end_time": "2026-06-10T00:30:00+00:00"},
                {"per_kwh": 0.03, "start_time": "2026-06-10T00:30:00+00:00",
                 "end_time": "2026-06-10T01:00:00+00:00"}
            ]}
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex("^/api/history/period/.*"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                serde_json::from_str::<serde_json::Value>(include_str!(
                    "fixtures/history_hot_water_running.json"
                ))
                .unwrap(),
            ),
        )
        .mount(&server)
        .await;
    // Record any service POST (there must be none in dry-run).
    Mock::given(method("POST"))
        .and(path_regex("^/api/services/.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn e1_boot_solve_dry_run_no_service_posts_and_panel_serves() {
    let server = stub_ha().await;
    let registry = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/registry.yaml");
    let port = 18099;
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_legit-lp-scheduler"))
        .env("SCHED_HASS_URL", server.uri())
        .env("SCHED_TOKEN", "test-token")
        .env("SCHED_LOADS_CONFIG", registry)
        .env("SCHED_INTERVAL_SECONDS", "1")
        .env("SCHED_DRY_RUN", "true")
        .env("SCHED_WEB_PORT", port.to_string())
        .env("SCHED_DATA_DIR", std::env::temp_dir().join("lp-e2e").to_str().unwrap())
        .env("RUST_LOG", "info")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("binary spawns");

    // Drain the child's stdout continuously, like the add-on supervisor does in
    // prod. Without this, the solve loop's per-cycle log lines fill the 64KB pipe
    // buffer and `tracing` BLOCKS the loop mid-run — it would stop solving and the
    // panel would freeze on a stale report. (The binary logs to stdout.)
    let mut child_stdout = child.stdout.take().unwrap();
    let stdout_drain = std::thread::spawn(move || {
        let mut sink = String::new();
        child_stdout.read_to_string(&mut sink).ok();
    });

    // Give it time to boot + at least one solve cycle.
    tokio::time::sleep(Duration::from_secs(4)).await;

    // Panel over plain HTTP.
    let health = reqwest::get(format!("http://127.0.0.1:{port}/health")).await.unwrap();
    assert!(health.status().is_success());
    // First solve, preview OFF (HA boolean off, panel not yet toggled): the
    // observe-only loads are NOT solved.
    let status0: serde_json::Value = reqwest::get(format!("http://127.0.0.1:{port}/api/status"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status0["dry_run"], true);
    assert_eq!(status0["loads"].as_array().unwrap().len(), 3);
    assert_eq!(status0["preview"], false, "preview starts off");
    let solved_before = status0["loads"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|l| !l["on"].as_array().unwrap().is_empty())
        .count();

    // Toggle preview ON from the PANEL — exactly what the in-panel checkbox does.
    // This sets the runtime flag the solve loop reads and nudges an immediate
    // re-solve, all through the real binary.
    let toggled = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/api/preview?on=true"))
        .send()
        .await
        .unwrap();
    assert!(toggled.status().is_success(), "panel preview toggle accepted");
    tokio::time::sleep(Duration::from_secs(2)).await; // let the nudged re-solve land

    let status: serde_json::Value = reqwest::get(format!("http://127.0.0.1:{port}/api/status"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // Preview ON end-to-end via the panel toggle: every observe-only load is now
    // SOLVED into a horizon plan (non-empty `on`), proving the checkbox is wired
    // through the real binary into the solve loop. The zero-POST assertion below
    // proves it stays a pure preview — nothing is ever written to HA.
    assert_eq!(status["preview"], true, "panel toggle turned preview on end-to-end");
    assert!(
        status["loads"].as_array().unwrap().iter().all(|l| !l["on"].as_array().unwrap().is_empty()),
        "preview solves every load into a horizon plan: {}",
        status["loads"]
    );
    let solved_after = status["loads"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|l| !l["on"].as_array().unwrap().is_empty())
        .count();
    assert!(
        solved_after > solved_before,
        "toggling preview solved additional observe-only loads: {solved_before} -> {solved_after}"
    );
    // The storage devices from the test registry were read (states.json serves
    // their SoC) and planned end-to-end: a trajectory is present and grid-aligned.
    let storage = status["storage"].as_array().expect("storage array");
    assert_eq!(storage.len(), 2, "both cabinets were planned");
    assert_eq!(storage[0]["id"], "sonnen01");
    let soc = storage[0]["soc_kwh"].as_array().expect("soc trajectory");
    assert!(soc.len() > 1, "SoC trajectory spans the horizon");
    // Forecast context series the panel draws are populated too.
    assert!(status["grid_kw"].as_array().unwrap().len() > 1, "grid_kw series present");
    assert!(status["pv"].as_array().unwrap().len() > 1, "pv series present");
    // The separate feed-in forecast sensor is read and its series published.
    // (Per-step VARIATION is owned by the unit/integration tests, which inject a
    // clock; here the real wall clock vs the dated fixture leaves slots in the
    // past, so this asserts only that the wiring populates a grid-sized series.)
    assert!(status["feedin"].as_array().unwrap().len() > 1, "feed-in series present");
    // The served chart carries the hover layer (per-step hit-bands + <title>
    // readouts) end-to-end through the real binary. The browser-native hover
    // rendering itself has no headless seam (see tests/web.rs w6 note).
    let svg = reqwest::get(format!("http://127.0.0.1:{port}/horizon.svg"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(svg.contains(r#"class="hit""#) && svg.contains("<title>"), "chart has the hover layer");

    child.kill().unwrap();
    child.wait().unwrap(); // reap; clippy zombie_processes
    stdout_drain.join().ok(); // reap the stdout-drain thread once stdout closes
    let mut err = String::new();
    child.stderr.take().unwrap().read_to_string(&mut err).ok();

    // ZERO service POSTs reached the stub in dry-run.
    let posts = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method == wiremock::http::Method::POST)
        .count();
    assert_eq!(posts, 0, "dry-run must not POST; stderr: {err}");
}
