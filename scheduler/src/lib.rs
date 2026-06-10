//! legit-lp-scheduler — provider-neutral MILP load scheduler for Home Assistant.
//!
//! Library target so integration tests (`tests/`) can link the modules; the
//! binary (`main.rs`) is a thin wrapper.

// Construction phase: modules land tests-first, consumers arrive in later
// milestones. Tighten once the cycle orchestrator wires everything.
#![allow(dead_code)]

pub mod config;
pub mod cycle;
pub mod error;
pub mod executor;
pub mod forecast;
pub mod ha_client;
pub mod lp;
pub mod model;
pub mod profile;
pub mod rules;
pub mod status;
pub mod testkit;
pub mod time;
pub mod web;
