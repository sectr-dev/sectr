//! The turn-dispatch seam.
//!
//! In prod the turn is NOT an in-process task: the server journals the
//! turn start, then pushes the assignment to a runner container whose
//! sidecar dials the ingest WebSocket (Contract 4). This trait is the
//! boundary the sidecar-ingest + runner-spawn paths implement; the session
//! service only needs "dispatch happened or failed" to keep the journal coherent.

use uuid::Uuid;

use sectr_common::journal::SessionId;

#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
  #[error("runner spawn failed: {0}")]
  Spawn(String),
  #[error("dispatch unavailable: {0}")]
  Unavailable(String),
}

#[async_trait::async_trait]
pub trait TurnDispatcher: Send + Sync {
  /// Trigger the runner for a turn whose lease is ALREADY claimed and
  /// whose INVOCATION_STARTED is ALREADY journaled (ordering contract:
  /// claim → [decision] → INVOCATION_STARTED → dispatch). The sidecar
  /// dials the ingest path for its assignment; turn completion (through
  /// `NO_MORE_ACTIONS` and the lease release) arrives asynchronously via
  /// ingest. On `Err` the CALLER journals `SESSION_ERROR` and releases
  /// the lease — the journal always sees a well-formed turn end.
  async fn dispatch_turn(
    &self,
    session: SessionId,
    deployment_id: Uuid,
    invocation_id: Uuid,
    route: &str,
  ) -> Result<(), DispatchError>;
}

/// Interim dispatcher (when no runner path is wired): every turn fails to dispatch,
/// which surfaces as `SESSION_ERROR` on the journal tail — the API/journal
/// plumbing is fully exercised end to end while the runner path is built.
pub struct UnimplementedDispatcher;

#[async_trait::async_trait]
impl TurnDispatcher for UnimplementedDispatcher {
  async fn dispatch_turn(
    &self,
    _session: SessionId,
    _deployment_id: Uuid,
    _invocation_id: Uuid,
    _route: &str,
  ) -> Result<(), DispatchError> {
    Err(DispatchError::Unavailable(
      "no runner platform is configured".into(),
    ))
  }
}
