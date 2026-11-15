//! sectr-server library — the gateway + control plane. Split from the thin
//! binary so integration tests can build the router in-process.

#![cfg_attr(
  test,
  allow(
    clippy::disallowed_macros,
    reason = "tests build JSON fixtures with `json!`"
  )
)]

pub mod agui;
pub mod analytics;
pub mod apps;
pub mod auth;
pub mod clerk;
pub mod cli_login;
pub mod config;
pub mod deploy;
pub mod dispatch;
pub mod http;
pub mod ingest;
pub mod limits;
pub mod metrics;
pub mod platform;
pub mod reaper;
pub mod secrets;
pub mod sessions;
pub mod signup;
pub mod state;
pub mod usage_ping;

#[cfg(test)]
pub mod test_pg;
#[cfg(test)]
pub mod testkeys;
