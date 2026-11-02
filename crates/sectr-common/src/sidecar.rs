//! Contract-4 wire frames (schema/sidecar.schema.json): the sidecar↔server
//! protocol shared by sectr-server's ingest endpoint and the
//! sidecar client in sectr-worker. One WebSocket per sidecar container,
//! dialed by the sidecar; JSON frames; NO heartbeat frames — ingest traffic
//! IS the lease heartbeat (see schema/sidecar.schema.json). Frame kinds:
//!
//! - sidecar → server: [`SidecarFrame`] (ready / event / complete)
//! - server → sidecar: [`ServerFrame`] (assignment / ack / abort)
//!
//! Unknown kinds are ignored by receivers (forward compat) — parse to a
//! `Value`, match the `kind` string, and treat unknown values as no-ops.
//! The full validation story (typed payloads, UUIDv7 ids) lives in
//! `events.rs` types these frames embed.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::events::Event;

// ---------------------------------------------------------------------------
// Wire frames (schema/sidecar.schema.json)
// ---------------------------------------------------------------------------

/// sidecar → server frames. Unknown kinds are IGNORED (forward compat) —
/// the WS handler parses to `Value` and matches known kinds, so never
/// route this enum through a naive `serde_json::from_str`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SidecarFrame {
  Ready,
  Event {
    batch_id: Uuid,
    events: Vec<crate::events::NewEvent>,
  },
  Complete {
    outcome: CompleteOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    message: Option<String>,
  },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompleteOutcome {
  /// Sentinel journaled — turn over.
  Completed,
  /// Approval pending (the server already released the lease when it
  /// journaled the suspension).
  Suspended,
  /// SESSION_ERROR + sentinel journaled by the sidecar.
  Failed,
  /// The WS died before the final batch — the reconciler owns the turn.
  IngestLost,
}

/// server → sidecar frames.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ServerFrame {
  Assignment {
    #[serde(flatten)]
    assignment: Box<Assignment>,
  },
  Ack {
    batch_id: Uuid,
    events: Vec<AckEntry>,
  },
  Abort {
    reason: String,
  },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AckEntry {
  pub id: Uuid,
  pub seq: i64,
  pub duplicate: bool,
}

/// One invocation to execute (the `assignment` frame's payload).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Assignment {
  pub session_id: Uuid,
  pub deployment_id: Uuid,
  pub invocation_id: Uuid,
  pub route: String,
  pub reason: crate::events::InvocationReason,
  /// Full transcript INCLUDING this invocation's INVOCATION_STARTED
  /// (the service journals it before dispatch).
  pub transcript: Vec<Event>,
  /// The app's resolved secrets (write-only: values go to the container
  /// env, no read API returns them).
  #[serde(default)]
  pub env: HashMap<String, String>,
  /// User-process spawn spec (argv, no shell).
  pub runner: RunnerSpec,
  /// Tool names the platform requires approval for (injected as
  /// [`APPROVAL_POLICY_ENV`]).
  #[serde(default)]
  pub approval_tools: Vec<String>,
}

/// The runner env var Sectr sets to the JSON array of a deployment's
/// `approval_tools`, in dev and deployed. Only Sectr sets it: the policy's
/// one source is `[deploy] approval_tools` in sectr.toml, so a user setting
/// the variable (a secret, `.env`, the `sectr dev` environment) is an error,
/// not an override.
pub const APPROVAL_POLICY_ENV: &str = "SECTR_APPROVAL_POLICY";

/// Why a user can't set [`APPROVAL_POLICY_ENV`], for the errors that refuse it.
pub const APPROVAL_POLICY_ENV_IS_RESERVED: &str = "Sectr sets SECTR_APPROVAL_POLICY from \
  `[deploy] approval_tools` in sectr.toml; list the tools that need approval there instead";

/// The value of [`APPROVAL_POLICY_ENV`] for `approval_tools`.
pub fn approval_policy_env(approval_tools: &[String]) -> String {
  serde_json::to_string(approval_tools).expect("a list of names serializes")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerSpec {
  pub command: Vec<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub workdir: Option<String>,
  #[serde(default)]
  pub env: HashMap<String, String>,
}

/// Ingest-token prefix (`sit_...`). Scoped to one turn;
/// minted by the server, verified at the WS upgrade.
pub const INGEST_TOKEN_PREFIX: &str = "sit_";
