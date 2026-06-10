//! legit-lp-scheduler — provider-neutral MILP load scheduler for Home Assistant.
//!
//! Wiring lives here (and only here may `anyhow` appear). The solve loop, web
//! server, and module wiring arrive in later milestones per docs/PLAN.md.

// Scaffold stage: types exist before their consumers (TDD builds inside-out).
// Remove once the planner/executor land.
#![allow(dead_code)]

mod config;
mod error;
mod executor;
mod forecast;
mod ha_client;
mod lp;
mod model;
mod profile;
mod rules;
mod status;
mod time;
mod web;

fn main() -> anyhow::Result<()> {
    println!(
        "legit-lp-scheduler {} — scaffold; solve loop arrives in milestone 10",
        env!("CARGO_PKG_VERSION")
    );
    Ok(())
}
