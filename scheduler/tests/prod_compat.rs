//! Production-registry compatibility guard.
//!
//! `fixtures/prod_registry.yaml` is a byte-for-byte copy of the **live deployed**
//! Beckton registry (`/addon_configs/67dae2bb_legit_lp_scheduler/registry-storage.yaml`,
//! mirrored in the parent repo as `live_ha_config/lp_registry.yaml`). It predates
//! v0.2.0, so it still carries the legacy `type: hot_water|dehumidifier|aircon`
//! lines and the `kind: humidity_below` demand. This test pins the contract that
//! v0.2.0 loads it **unchanged** — both the parse the add-on runs on boot AND the
//! serialize→re-parse the panel runs on the first save. If a future struct change
//! breaks prod, this goes red before the add-on does.

use legit_lp_scheduler::config::{self, DemandCfg};

fn prod_yaml() -> String {
    let path = format!("{}/tests/fixtures/prod_registry.yaml", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(path).expect("prod fixture present")
}

/// The exact path `main.rs` runs on boot: `parse` = serde + `validate`.
/// Legacy `type:` lines must be ignored (no `deny_unknown_fields` on loads), and
/// `kind: humidity_below` must still resolve.
#[test]
fn prod_registry_loads_under_v020() {
    let cfg =
        config::parse(&prod_yaml()).expect("deployed prod registry must still parse+validate");

    let load_ids: Vec<&str> = cfg.loads.iter().map(|l| l.id.as_str()).collect();
    assert_eq!(load_ids, ["hot_water", "dehumidifier", "aircon"], "prod loads, in order");

    let storage_ids: Vec<&str> = cfg.global.storage.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(storage_ids, ["sonnen01", "sonnen02"], "prod batteries, in order");

    // The dehumidifier's legacy demand still parses as HumidityBelow (NOT an error,
    // NOT silently dropped) — both must_have and can_take.
    let dehum = cfg.loads.iter().find(|l| l.id == "dehumidifier").unwrap();
    assert!(
        matches!(dehum.must_have, DemandCfg::HumidityBelow { .. }),
        "must_have humidity_below preserved"
    );
    assert!(
        matches!(dehum.can_take, Some(DemandCfg::HumidityBelow { .. })),
        "can_take humidity_below preserved"
    );
}

/// The panel owns the whole file (D1): the first save is serialize→atomic-write.
/// Serializing the parsed prod struct then re-parsing must yield an identical
/// struct — i.e. a panel save can never silently change or reject the live config.
/// (The legacy `type:` lines and comments are not struct fields, so they drop on
/// the first save; everything the engine acts on is preserved.)
#[test]
fn prod_registry_survives_a_panel_save_round_trip() {
    let original = config::parse(&prod_yaml()).expect("parse");
    let reserialized = config::serialize_registry(&original).expect("serialize validated");
    let reparsed = config::parse(&reserialized).expect("re-parse the panel-written file");
    assert_eq!(original, reparsed, "panel save round-trip is struct-lossless");
}
