//! The REAL binary against a stub HA (wiremock): boots, parses the example
//! registry, solves, serves the panel — and in dry-run POSTs nothing.

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
    let registry = concat!(env!("CARGO_MANIFEST_DIR"), "/../addon/example.yaml");
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

    // Give it time to boot + at least one solve cycle.
    tokio::time::sleep(Duration::from_secs(4)).await;

    // Panel over plain HTTP.
    let health = reqwest::get(format!("http://127.0.0.1:{port}/health")).await.unwrap();
    assert!(health.status().is_success());
    let status: serde_json::Value = reqwest::get(format!("http://127.0.0.1:{port}/api/status"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["dry_run"], true);
    assert_eq!(status["loads"].as_array().unwrap().len(), 3);

    child.kill().unwrap();
    child.wait().unwrap(); // reap; clippy zombie_processes
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
