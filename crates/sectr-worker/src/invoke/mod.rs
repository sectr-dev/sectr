//! The invoke half of the turn engine: POST the
//! [`InvocationRequest`](sectr_common::events::InvocationRequest) to the
//! runner's `http://127.0.0.1:$SECTR_PORT/invocations`, SSE-decode the
//! response stream, and validate it against the turn contract
//! (`schema/invocation.schema.json` + `runnerEvent`) — the engine-side
//! mirror of the SDK's `sectr.drive` driver (which produces streams under
//! the same rules; this consumes them).
//!
//! Layers, each independently testable:
//!
//! - [`sse`] — byte chunks → SSE `data:` payloads (transport-agnostic).
//! - [`client`] — HTTP + SSE: [`InvokeClient::invoke`] yields
//!   [`StreamItem`]s (frames, clean end, transport failure).
//! - [`validate`] — the finality state machine: consumes `StreamItem`s,
//!   hands back journal-append directives, and concludes the turn with an
//!   outcome. Never touches I/O.
//!
//! Contract policy: a runner that
//! violates the stream contract — an unparseable frame, a non-runner event
//! type, a frame after `TOOL_APPROVAL_REQUIRED`, a second approval in one
//! turn, or a stream that ends without `NO_MORE_ACTIONS` and without a
//! suspension — fails the turn. The engine journals a synthesized
//! `SESSION_ERROR` + `NO_MORE_ACTIONS` and nothing from the offending
//! frame: a malformed stream means the runner is broken, and a partial
//! transcript from it is worse than a clean failed turn (the journal must
//! always see a well-formed turn end).

mod client;
mod sse;
mod validate;

pub use client::{InvokeClient, InvokeError, TurnStream};
pub use validate::{
  Directive, Failure, FeedError, FrameError, StreamItem, StreamValidator, TurnOutcome,
};
