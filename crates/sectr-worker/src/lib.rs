//! Sectr worker sidecar: user-process supervision, stream interception,
//! journal writes.
//!
//! The turn-lifecycle core lives here as a library so `sectr dev` can embed
//! it with dev wiring (SQLite journal, subprocess sandbox) while the prod
//! sidecar uses the same code with the WS ingest sink.

#![cfg_attr(
  test,
  allow(
    clippy::disallowed_macros,
    reason = "tests build JSON fixtures with `json!`"
  )
)]

pub mod dispatch;
pub mod invoke;
pub mod runner;
pub mod serve;
pub mod service;
pub mod sidecar;
pub mod testing;
pub mod turn;
