//! Shared library for all Sectr crates: the Rust mirror of the `schema/`
//! contracts plus the traits that let server, worker, and CLI swap
//! infrastructure drivers. No HTTP services, no business orchestration.
//!
//! Modules:
//! - [`agui`] — the pure journal → AG-UI projection.
//! - [`events`] — event envelope, payloads, runner stream frames, invoke
//!   request (mirrors `schema/event.schema.json` + `invocation.schema.json`).
//! - [`inspect`] — running the SDK inspector, the import that yields an
//!   app's manifest.
//! - [`registry`] — the relational model: all non-event tables of
//!   `schema/data-model.sql` (sessions, deployments, leases, …). Consumed by
//!   the server-role layer only.

pub mod agui;
pub mod api;
pub mod drivers;
pub mod events;
pub mod inspect;
pub mod journal;
pub mod migrations;
pub mod reconcile;
pub mod registry;
pub mod secret;
pub mod secret_cipher;
pub mod sidecar;
pub mod store;
pub mod telemetry;

#[cfg(test)]
pub(crate) mod test_pg;
