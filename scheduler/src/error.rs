//! Typed errors for the core. `anyhow` is allowed only at the `main.rs` /
//! config boundary.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum SchedulerError {
    #[error("config error: {0}")]
    Config(String),

    #[error("HA API error: {0}")]
    HaApi(String),

    #[error("solver error: {0}")]
    Solver(String),
}
