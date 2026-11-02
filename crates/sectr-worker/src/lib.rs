//! Sectr worker sidecar: user-process supervision, stream interception,
//! journal writes.
//!
//! The turn-lifecycle core lives here as a library so `sectr dev` can embed
//! it with dev wiring (SQLite journal, subprocess sandbox) while the prod
//! sidecar uses the same code with the WS ingest sink.

pub mod dispatch;
pub mod invoke;
pub mod runner;
pub mod service;
pub mod sidecar;
pub mod testing;
pub mod turn;
