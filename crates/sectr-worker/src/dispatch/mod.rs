//! The server-role layer: everything around the engine that "the server
//! does" in prod, executed in-process by `sectr dev`. The engine core takes
//! `(assignment, &dyn EventSink)` and knows nothing about leases, sandboxes,
//! or the registry — this module is the composition glue.
//!
//! Mode split (the reason this directory has exactly one dev-only file):
//!
//! - [`sink`] is a **mode-neutral server-role primitive**: the
//!   ingest-boundary lease refresh (the prod WS ingest handler implements
//!   the identical hook inline — heartbeat-before-append). The reconciler
//!   (the other server-role primitive) lives in `sectr_common::reconcile`
//!   so sectr-server can run it without depending on this crate.
//! - [`dev`] is the **dev-only orchestration** (`DevDispatch`): session
//!   create, lease claim/release, sandbox spawn + readiness, teardown. In
//!   prod these duties belong to sectr-server (sessions, platform events,
//!   leases) and the sidecar (spawn/readiness/teardown, over the WS
//!   protocol) — same ordering contract, different transport. Nothing in
//!   `dev.rs` is shared with prod; nothing in `sink.rs`/`reconciler.rs` is
//!   dev-specific.
//!
//! Not in this directory: the `SessionService` impl (`service.rs`, crate
//! root) — it CONSUMES the dispatch; the API boundary is a different
//! altitude from the per-turn machinery.

mod dev;
mod sink;

pub use dev::{DevDispatch, DispatchConfig, DispatchError, TurnOpts};
pub use sink::LeaseRefreshingSink;
