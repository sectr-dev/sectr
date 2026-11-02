//! Event model — the Rust mirror of `schema/event.schema.json` and
//! `schema/invocation.schema.json`.
//!
//! Mirrored types (not codegen), kept honest
//! by golden tests that round-trip every fixture in `schema/fixtures/`.
//!
//! Forward compatibility is part of the contract: every object (envelope,
//! payloads, reasons, decisions) preserves unknown fields in an `extra` map
//! (`#[serde(flatten)]`), mirroring the schema's deliberate lack of
//! `additionalProperties: false`.
//!
//! The `type` ↔ payload correspondence is enforced at the deserialization
//! boundary: `Event`/`RunnerEvent` deserialize by dispatching on the `type`
//! string and parsing the payload as that type's struct, so a `MESSAGE_DELTA`
//! envelope with a `TOOL_CALL` payload is rejected (`payload-mismatch`
//! fixture). Constructors maintain the same invariant.

use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

/// Full session history as journal envelopes, rehydrated by the platform and
/// passed to the runner in the invoke request (`$defs/transcript`).
pub type Transcript = Vec<Event>;

// ---------------------------------------------------------------------------
// Event type enums
// ---------------------------------------------------------------------------

/// The canonical closed enum of event types (`$defs/eventType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EventType {
  MessageStart,
  MessageDelta,
  MessageEnd,
  ToolCall,
  ToolApprovalRequired,
  Error,
  NoMoreActions,
  SessionCreated,
  InvocationStarted,
  ToolResult,
  ToolApprovalGranted,
  ToolApprovalDenied,
  SessionError,
  TranscriptCompacted,
}

/// The subset of event types a runner may emit over the invoke response
/// stream (`$defs/runnerEventType`). Everything else is platform-emitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RunnerEventType {
  MessageStart,
  MessageDelta,
  MessageEnd,
  ToolCall,
  ToolResult,
  ToolApprovalRequired,
  Error,
  NoMoreActions,
  TranscriptCompacted,
}

impl EventType {
  /// The wire name, e.g. `"TOOL_CALL"`.
  pub fn name(self) -> &'static str {
    match self {
      EventType::MessageStart => "MESSAGE_START",
      EventType::MessageDelta => "MESSAGE_DELTA",
      EventType::MessageEnd => "MESSAGE_END",
      EventType::ToolCall => "TOOL_CALL",
      EventType::ToolApprovalRequired => "TOOL_APPROVAL_REQUIRED",
      EventType::Error => "ERROR",
      EventType::NoMoreActions => "NO_MORE_ACTIONS",
      EventType::SessionCreated => "SESSION_CREATED",
      EventType::InvocationStarted => "INVOCATION_STARTED",
      EventType::ToolResult => "TOOL_RESULT",
      EventType::ToolApprovalGranted => "TOOL_APPROVAL_GRANTED",
      EventType::ToolApprovalDenied => "TOOL_APPROVAL_DENIED",
      EventType::SessionError => "SESSION_ERROR",
      EventType::TranscriptCompacted => "TRANSCRIPT_COMPACTED",
    }
  }

  /// The runner-emittable subset of this type, if any.
  pub fn as_runner(self) -> Option<RunnerEventType> {
    match self {
      EventType::MessageStart => Some(RunnerEventType::MessageStart),
      EventType::MessageDelta => Some(RunnerEventType::MessageDelta),
      EventType::MessageEnd => Some(RunnerEventType::MessageEnd),
      EventType::ToolCall => Some(RunnerEventType::ToolCall),
      EventType::ToolResult => Some(RunnerEventType::ToolResult),
      EventType::ToolApprovalRequired => Some(RunnerEventType::ToolApprovalRequired),
      EventType::Error => Some(RunnerEventType::Error),
      EventType::NoMoreActions => Some(RunnerEventType::NoMoreActions),
      EventType::TranscriptCompacted => Some(RunnerEventType::TranscriptCompacted),
      EventType::SessionCreated
      | EventType::InvocationStarted
      | EventType::ToolApprovalGranted
      | EventType::ToolApprovalDenied
      | EventType::SessionError => None,
    }
  }
}

impl fmt::Display for EventType {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(self.name())
  }
}

impl fmt::Display for RunnerEventType {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(self.name())
  }
}

impl RunnerEventType {
  /// The wire name, e.g. `"NO_MORE_ACTIONS"`.
  pub fn name(self) -> &'static str {
    self.widen().name()
  }

  /// Widen to the full event type enum.
  pub fn widen(self) -> EventType {
    match self {
      RunnerEventType::MessageStart => EventType::MessageStart,
      RunnerEventType::MessageDelta => EventType::MessageDelta,
      RunnerEventType::MessageEnd => EventType::MessageEnd,
      RunnerEventType::ToolCall => EventType::ToolCall,
      RunnerEventType::ToolResult => EventType::ToolResult,
      RunnerEventType::ToolApprovalRequired => EventType::ToolApprovalRequired,
      RunnerEventType::Error => EventType::Error,
      RunnerEventType::NoMoreActions => EventType::NoMoreActions,
      RunnerEventType::TranscriptCompacted => EventType::TranscriptCompacted,
    }
  }
}

// ---------------------------------------------------------------------------
// Payload structs (one per event type, mirroring $defs)
// ---------------------------------------------------------------------------

/// Unknown fields preserved for forward compatibility. Every payload struct
/// (and the envelope) carries one.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Extra {
  #[serde(flatten)]
  pub map: Map<String, Value>,
}

/// `MESSAGE_START` — start of an assistant message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageStart {
  /// e.g. "assistant"; opaque to the platform.
  pub role: String,
  /// Runner-scoped message identifier; correlates START/DELTA/END.
  pub message_id: String,
  #[serde(flatten)]
  pub extra: Extra,
}

/// `MESSAGE_DELTA` — content delta for a message. Deltas are interpretable,
/// not opaque: the platform (e.g. a future AG-UI-compatible endpoint) must be
/// able to reconstruct both the exact transcript and its streaming semantics
/// from the journal alone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageDelta {
  pub message_id: String,
  pub delta: TypedDelta,
  #[serde(flatten)]
  pub extra: Extra,
}

/// `MESSAGE_END` — end of an assistant message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageEnd {
  pub message_id: String,
  /// Opaque to the platform, e.g. "stop", "length", "tool_use".
  #[serde(skip_serializing_if = "Option::is_none")]
  pub finish_reason: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub framework_state: Option<FrameworkState>,
  #[serde(flatten)]
  pub extra: Extra,
}

/// The most a [`FrameworkState`] may take, serialized: the worker drops a
/// larger one from its `MESSAGE_END`.
pub const FRAMEWORK_STATE_LIMIT: usize = 256 * 1024;

/// `MESSAGE_END.framework_state`: what the adapter that wrote it needs to
/// rehydrate and replay that one model response as its agent framework
/// stored it (e.g. signed reasoning). Its `data` is opaque to the
/// platform: stored and served, never interpreted; read back only by
/// `adapter`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrameworkState {
  /// The adapter that wrote it, e.g. "langgraph".
  pub adapter: String,
  /// The adapter's format version for `data`.
  pub v: u64,
  /// The adapter's own data.
  pub data: Map<String, Value>,
  #[serde(flatten)]
  pub extra: Extra,
}

impl FrameworkState {
  /// Its size serialized, as [`FRAMEWORK_STATE_LIMIT`] counts it.
  pub fn serialized_len(&self) -> usize {
    serde_json::to_vec(self).map_or(usize::MAX, |bytes| bytes.len())
  }
}

/// `TOOL_CALL` — journal record of a tool call initiated inside the runner.
/// The platform NEVER executes tools; `name` + `arguments` must be
/// sufficient for a resumed runner to execute an approved call from journal
/// data alone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
  pub tool_call_id: String,
  pub name: String,
  /// Opaque JSON (conventionally an object), journaled verbatim.
  pub arguments: Value,
  #[serde(flatten)]
  pub extra: Extra,
}

/// `TOOL_APPROVAL_REQUIRED` — requests human approval for a tool call. MUST
/// be the final runner event of an invocation; it suspends (does not end)
/// the turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolApprovalRequired {
  pub tool_call_id: String,
  pub name: String,
  pub arguments: Value,
  /// Human-readable explanation shown in the approval inbox.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub reason: Option<String>,
  #[serde(flatten)]
  pub extra: Extra,
}

/// `ERROR` — runner-level error; the sidecar synthesizes `NO_MORE_ACTIONS`
/// after it (errors are terminal in MVP).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Error {
  pub message: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub code: Option<String>,
  /// True if a retry of the same invocation may succeed.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub transient: Option<bool>,
  #[serde(flatten)]
  pub extra: Extra,
}

/// `NO_MORE_ACTIONS` — THE turn-ending sentinel: the agent has no pending
/// actions. Deliberately NOT emitted on an approval pause (a gated tool is a
/// pending action, so the turn is suspended, not over).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NoMoreActions {
  #[serde(flatten)]
  pub extra: Extra,
}

/// `SESSION_CREATED` — platform-emitted, seq 0 of a session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionCreated {
  pub app_id: Uuid,
  #[serde(deserialize_with = "de_route")]
  pub route: String,
  /// Owner identity: set when the creating principal was a user
  /// token — the app-defined end-user the session belongs to. Absent for
  /// backend/platform-created sessions (additive, forward-compatible).
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub external_user_id: Option<String>,
  #[serde(flatten)]
  pub extra: Extra,
}

/// `INVOCATION_STARTED` — platform-emitted, one per invocation, appended
/// BEFORE the sidecar fetches the transcript (so the runner's transcript
/// already includes its own INVOCATION_STARTED).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InvocationStarted {
  pub invocation_id: Uuid,
  #[serde(deserialize_with = "de_route")]
  pub route: String,
  pub deployment_id: Uuid,
  pub reason: InvocationReason,
  #[serde(flatten)]
  pub extra: Extra,
}

/// `TOOL_RESULT` — journaled after every successful tool execution (normal
/// mid-turn calls and post-approval executions alike) and for a denied
/// approval. Not guaranteed for every `TOOL_CALL`: a crash or a failing tool
/// can leave a call unanswered, and the SDK's canonical fold then supplies
/// a synthesized output (schema/README.md, "every prefix of a valid journal
/// is a valid journal").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
  pub tool_call_id: String,
  /// Opaque JSON, journaled verbatim.
  pub result: Value,
  #[serde(flatten)]
  pub extra: Extra,
}

/// `TOOL_APPROVAL_GRANTED` / `TOOL_APPROVAL_DENIED` — the approval decision
/// itself, journaled by the platform.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolApprovalDecision {
  pub tool_call_id: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub approver: Option<String>,
  #[serde(flatten)]
  pub extra: Extra,
}

/// `SESSION_ERROR` — platform-level failure (runner crash, timeout, lease
/// expiry).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionError {
  pub message: String,
  #[serde(flatten)]
  pub extra: Extra,
}

/// `TRANSCRIPT_COMPACTED` — the FULL post-compaction context in canonical
/// message form. Context assembly is a positional fold: these messages
/// replace everything up to `replaces_up_to_seq` (journal head at compaction
/// time — metadata/validation only). Constraint: must not drop an unanswered
/// tool call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptCompacted {
  pub replaces_up_to_seq: i64,
  /// OpenAI-style message dicts (the canonical view).
  pub messages: Vec<Map<String, Value>>,
  #[serde(flatten)]
  pub extra: Extra,
}

/// Interpretable content delta for `MESSAGE_DELTA`:
/// `{"type": "text" | "reasoning", "text": ...}` — internally tagged on
/// `type`, unknown fields preserved. The kind vocabulary is CLOSED so the
/// platform can interpret every journaled delta — e.g. an AG-UI-compatible
/// endpoint maps `text` → `TEXT_MESSAGE_CONTENT`, `reasoning` →
/// `REASONING_MESSAGE_*` (AG-UI 1.0 names; see `schema/agui-mapping.md`). Framework chunk types outside the vocabulary must be
/// normalized by the SDK adapter before journaling (e.g. Anthropic
/// `input_json_delta` accumulates into `TOOL_CALL.arguments`). Anything else
/// (bare strings, scalars, arrays, unknown kinds, missing `text`) is
/// rejected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TypedDelta {
  Text {
    text: String,
    #[serde(flatten)]
    extra: Extra,
  },
  Reasoning {
    text: String,
    #[serde(flatten)]
    extra: Extra,
  },
}

impl TypedDelta {
  /// A text delta.
  pub fn text(s: impl Into<String>) -> Self {
    TypedDelta::Text {
      text: s.into(),
      extra: Extra::default(),
    }
  }

  /// A reasoning delta.
  pub fn reasoning(s: impl Into<String>) -> Self {
    TypedDelta::Reasoning {
      text: s.into(),
      extra: Extra::default(),
    }
  }
}

// ---------------------------------------------------------------------------
// Invocation reason (mirrors $defs/invocationReason; tag = "kind")
// ---------------------------------------------------------------------------

/// A user message's input, checked: what every path that starts a user
/// turn takes, so none journals one unchecked. The message text, journaled
/// verbatim as `INVOCATION_STARTED.reason.input`. Anything but a string
/// (an object, a content-parts array, a number), or a string of only
/// whitespace, is rejected (400) before anything is journaled. Some
/// framework and provider pairs can't send an empty user message, so the
/// turn would fail in the runner.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct UserMessageInput(String);

/// [`UserMessageInput::new`]'s error.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum InvalidInput {
  #[error("input must be a string: the user's message text")]
  NotAString,
  #[error("input is empty: a user message needs some text")]
  Empty,
}

impl UserMessageInput {
  pub fn new(input: serde_json::Value) -> Result<Self, InvalidInput> {
    match input {
      serde_json::Value::String(text) => Self::try_from(text),
      _ => Err(InvalidInput::NotAString),
    }
  }

  pub fn as_str(&self) -> &str {
    &self.0
  }
}

/// The one place the non-blank rule lives (the schema's `"pattern": "\\S"`):
/// the API, the typed event and its deserializer all go through it.
impl TryFrom<String> for UserMessageInput {
  type Error = InvalidInput;

  fn try_from(text: String) -> Result<Self, InvalidInput> {
    if text.trim().is_empty() {
      Err(InvalidInput::Empty)
    } else {
      Ok(Self(text))
    }
  }
}

impl From<UserMessageInput> for String {
  fn from(input: UserMessageInput) -> String {
    input.0
  }
}

/// Why an invocation exists. `tool_result` is specified for post-MVP
/// externally-executed tools.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InvocationReason {
  /// The user's message text. Starts a turn.
  UserMessage {
    input: UserMessageInput,
    #[serde(flatten)]
    extra: Extra,
  },
  /// Resumes a turn suspended by `TOOL_APPROVAL_REQUIRED`.
  ApprovalDecision {
    decision: ApprovalDecision,
    #[serde(flatten)]
    extra: Extra,
  },
  /// Post-MVP: resumes with an externally-computed tool output.
  ToolResult {
    tool_call_id: String,
    result: Value,
    #[serde(flatten)]
    extra: Extra,
  },
}

/// The approval decision carried by an `approval_decision` reason.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalDecision {
  pub approved: bool,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub approver: Option<String>,
  #[serde(flatten)]
  pub extra: Extra,
}

// ---------------------------------------------------------------------------
// Payload enum + (type, payload) dispatch
// ---------------------------------------------------------------------------

/// The payload of any event, mirroring the `typedEvent` oneOf. Serialization
/// is the inner struct itself (`untagged`); deserialization happens via
/// [`EventPayload::from_parts`], dispatched on the envelope's `type` string,
/// which is what enforces the type ↔ payload binding.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum EventPayload {
  MessageStart(MessageStart),
  MessageDelta(MessageDelta),
  MessageEnd(MessageEnd),
  ToolCall(ToolCall),
  ToolApprovalRequired(ToolApprovalRequired),
  Error(Error),
  NoMoreActions(NoMoreActions),
  SessionCreated(SessionCreated),
  InvocationStarted(InvocationStarted),
  ToolResult(ToolResult),
  ToolApprovalGranted(ToolApprovalDecision),
  ToolApprovalDenied(ToolApprovalDecision),
  SessionError(SessionError),
  TranscriptCompacted(TranscriptCompacted),
}

impl EventPayload {
  /// The event type this payload belongs to.
  pub fn event_type(&self) -> EventType {
    match self {
      EventPayload::MessageStart(_) => EventType::MessageStart,
      EventPayload::MessageDelta(_) => EventType::MessageDelta,
      EventPayload::MessageEnd(_) => EventType::MessageEnd,
      EventPayload::ToolCall(_) => EventType::ToolCall,
      EventPayload::ToolApprovalRequired(_) => EventType::ToolApprovalRequired,
      EventPayload::Error(_) => EventType::Error,
      EventPayload::NoMoreActions(_) => EventType::NoMoreActions,
      EventPayload::SessionCreated(_) => EventType::SessionCreated,
      EventPayload::InvocationStarted(_) => EventType::InvocationStarted,
      EventPayload::ToolResult(_) => EventType::ToolResult,
      EventPayload::ToolApprovalGranted(_) => EventType::ToolApprovalGranted,
      EventPayload::ToolApprovalDenied(_) => EventType::ToolApprovalDenied,
      EventPayload::SessionError(_) => EventType::SessionError,
      EventPayload::TranscriptCompacted(_) => EventType::TranscriptCompacted,
    }
  }

  /// Build a payload from the envelope's `type` string and raw payload
  /// value, rejecting mismatches (`payload-mismatch` fixture).
  pub fn from_parts(event_type: EventType, payload: Value) -> Result<Self, String> {
    macro_rules! parse {
      ($variant:ident, $t:ident) => {
        if event_type == EventType::$t {
          return serde_json::from_value(payload)
            .map(EventPayload::$variant)
            .map_err(|e| format!("payload does not match {}: {e}", stringify!($t)));
        }
      };
    }
    parse!(MessageStart, MessageStart);
    parse!(MessageDelta, MessageDelta);
    parse!(MessageEnd, MessageEnd);
    parse!(ToolCall, ToolCall);
    parse!(ToolApprovalRequired, ToolApprovalRequired);
    parse!(Error, Error);
    parse!(NoMoreActions, NoMoreActions);
    parse!(SessionCreated, SessionCreated);
    parse!(InvocationStarted, InvocationStarted);
    parse!(ToolResult, ToolResult);
    parse!(ToolApprovalGranted, ToolApprovalGranted);
    parse!(ToolApprovalDenied, ToolApprovalDenied);
    parse!(SessionError, SessionError);
    parse!(TranscriptCompacted, TranscriptCompacted);
    unreachable!("every EventType variant is handled above")
  }
}

// ---------------------------------------------------------------------------
// Envelope
// ---------------------------------------------------------------------------

/// One journal row / one client SSE frame (`$defs` root envelope).
///
/// `id` must be a UUIDv7 (time-ordered; also the SSE frame's `id:` field for
/// `Last-Event-ID` resumption); `seq` is per-session, monotonic,
/// gap-free within a turn, assigned by the journal writer — never user code.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Event {
  pub id: Uuid,
  pub session_id: Uuid,
  pub deployment_id: Uuid,
  #[serde(deserialize_with = "de_nonneg_i64")]
  pub seq: i64,
  /// Derived from `payload` — kept denormalized for cheap matching.
  /// Serialized as the wire `type` field.
  #[serde(rename = "type", serialize_with = "ser_event_type")]
  pub event_type: EventType,
  pub payload: EventPayload,
  pub created_at: DateTime<Utc>,
  #[serde(flatten)]
  pub extra: Extra,
}

impl Event {
  /// Assemble an envelope around a payload (journal-writer path: the
  /// writer assigns id/seq/created_at, then calls this).
  pub fn new(
    id: Uuid,
    session_id: Uuid,
    deployment_id: Uuid,
    seq: i64,
    created_at: DateTime<Utc>,
    payload: EventPayload,
  ) -> Self {
    let event_type = payload.event_type();
    Event {
      id,
      session_id,
      deployment_id,
      seq,
      event_type,
      payload,
      created_at,
      extra: Extra::default(),
    }
  }

  pub fn from_json_str(s: &str) -> Result<Self, serde_json::Error> {
    serde_json::from_str(s)
  }

  pub fn from_json_value(v: Value) -> Result<Self, serde_json::Error> {
    serde_json::from_value(v)
  }

  pub fn to_json_string(&self) -> String {
    serde_json::to_string(self).expect("Event serialization is infallible")
  }
}

impl<'de> Deserialize<'de> for NewEvent {
  fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
    #[derive(Deserialize)]
    struct Raw {
      #[serde(deserialize_with = "de_uuidv7")]
      id: Uuid,
      #[serde(rename = "type")]
      event_type: EventType,
      payload: Value,
      #[serde(flatten)]
      extra: Extra,
    }

    let raw = Raw::deserialize(deserializer)?;
    let payload =
      EventPayload::from_parts(raw.event_type, raw.payload).map_err(serde::de::Error::custom)?;
    Ok(NewEvent {
      id: raw.id,
      event_type: raw.event_type,
      payload,
      extra: raw.extra,
    })
  }
}

impl<'de> Deserialize<'de> for Event {
  fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
    #[derive(Deserialize)]
    struct Raw {
      #[serde(deserialize_with = "de_uuidv7")]
      id: Uuid,
      session_id: Uuid,
      deployment_id: Uuid,
      #[serde(deserialize_with = "de_nonneg_i64")]
      seq: i64,
      #[serde(rename = "type")]
      event_type: EventType,
      payload: Value,
      created_at: DateTime<Utc>,
      #[serde(flatten)]
      extra: Extra,
    }

    let raw = Raw::deserialize(deserializer)?;
    let payload =
      EventPayload::from_parts(raw.event_type, raw.payload).map_err(serde::de::Error::custom)?;
    Ok(Event {
      id: raw.id,
      session_id: raw.session_id,
      deployment_id: raw.deployment_id,
      seq: raw.seq,
      event_type: raw.event_type,
      payload,
      created_at: raw.created_at,
      extra: raw.extra,
    })
  }
}

// ---------------------------------------------------------------------------
// Runner stream frame ($defs/runnerEvent)
// ---------------------------------------------------------------------------

/// One frame of the runner's SSE response stream in the invoke contract:
/// `{type, payload}` only. `id`, `seq`, `session_id`, `deployment_id` and
/// `created_at` are assigned by the platform (producer mints the id; the
/// journal writer stamps the rest on ingest).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RunnerEvent {
  /// Serialized as the wire `type` field.
  #[serde(rename = "type", serialize_with = "ser_runner_event_type")]
  pub event_type: RunnerEventType,
  pub payload: EventPayload,
  #[serde(flatten)]
  pub extra: Extra,
}

impl RunnerEvent {
  pub fn new(payload: EventPayload) -> Result<Self, String> {
    let event_type = payload.event_type().as_runner().ok_or_else(|| {
      format!(
        "{} is not a runner-emittable event type",
        payload.event_type()
      )
    })?;
    Ok(RunnerEvent {
      event_type,
      payload,
      extra: Extra::default(),
    })
  }

  pub fn from_json_str(s: &str) -> Result<Self, serde_json::Error> {
    serde_json::from_str(s)
  }

  pub fn to_json_string(&self) -> String {
    serde_json::to_string(self).expect("RunnerEvent serialization is infallible")
  }
}

impl<'de> Deserialize<'de> for RunnerEvent {
  fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
    #[derive(Deserialize)]
    struct Raw {
      #[serde(rename = "type")]
      event_type: RunnerEventType,
      payload: Value,
      #[serde(flatten)]
      extra: Extra,
    }

    let raw = Raw::deserialize(deserializer)?;
    let payload = EventPayload::from_parts(raw.event_type.widen(), raw.payload)
      .map_err(serde::de::Error::custom)?;
    Ok(RunnerEvent {
      event_type: raw.event_type,
      payload,
      extra: raw.extra,
    })
  }
}

// ---------------------------------------------------------------------------
// Invoke request (invocation.schema.json $defs/invocationRequest)
// ---------------------------------------------------------------------------

/// The only thing a user process must implement: the body the sidecar POSTs
/// to `http://localhost:$SECTR_PORT/invocations`. The transcript already
/// includes this invocation's `INVOCATION_STARTED`; the process performs no
/// journal/DB access.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InvocationRequest {
  pub session_id: Uuid,
  pub invocation_id: Uuid,
  /// Agent route being invoked, e.g. "/customer-support".
  #[serde(deserialize_with = "de_route")]
  pub route: String,
  pub transcript: Transcript,
  pub reason: InvocationReason,
  #[serde(flatten)]
  pub extra: Extra,
}

// ---------------------------------------------------------------------------
// NewEvent — what an EventSink appends (id minted by the producer;
// seq/created_at assigned by the journal writer on append)
// ---------------------------------------------------------------------------

/// A validated event with a producer-minted identity. `id` is a UUIDv7
/// minted by the platform's event producer (the engine — never user code);
/// the journal writer dedups on it (reconnect replay) and assigns `seq`
/// (per-session, prev + 1) and `created_at` on append.
///
/// Serde shape (`$defs/newEvent` in schema/sidecar.schema.json):
/// `{id, type, payload}` — the ingest-path wire form of an event.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NewEvent {
  pub id: Uuid,
  /// Serialized as the wire `type` field.
  #[serde(rename = "type")]
  pub event_type: EventType,
  pub payload: EventPayload,
  #[serde(flatten)]
  pub extra: Extra,
}

impl NewEvent {
  pub fn new(payload: EventPayload) -> Self {
    let event_type = payload.event_type();
    NewEvent {
      id: Uuid::now_v7(),
      event_type,
      payload,
      extra: Extra::default(),
    }
  }

  /// Override the minted id (e.g. deterministic ids in tests). Must be
  /// UUIDv7 — validated at append time.
  pub fn with_id(mut self, id: Uuid) -> Self {
    self.id = id;
    self
  }

  pub fn into_event(
    self,
    session_id: Uuid,
    deployment_id: Uuid,
    seq: i64,
    created_at: DateTime<Utc>,
  ) -> Event {
    Event {
      id: self.id,
      session_id,
      deployment_id,
      seq,
      event_type: self.event_type,
      payload: self.payload,
      created_at,
      extra: self.extra,
    }
  }
}

impl From<RunnerEvent> for NewEvent {
  fn from(runner: RunnerEvent) -> Self {
    NewEvent {
      id: Uuid::now_v7(),
      event_type: runner.event_type.widen(),
      payload: runner.payload,
      extra: runner.extra,
    }
  }
}

impl TryFrom<EventPayload> for RunnerEvent {
  type Error = String;

  fn try_from(payload: EventPayload) -> Result<Self, String> {
    RunnerEvent::new(payload)
  }
}

// ---------------------------------------------------------------------------
// Deserialize helpers
// ---------------------------------------------------------------------------

fn ser_event_type<S: serde::Serializer>(t: &EventType, serializer: S) -> Result<S::Ok, S::Error> {
  serializer.serialize_str(t.name())
}

fn ser_runner_event_type<S: serde::Serializer>(
  t: &RunnerEventType,
  serializer: S,
) -> Result<S::Ok, S::Error> {
  serializer.serialize_str(t.name())
}

/// Route paths are anchored at `/` (schema `pattern: "^/"` on every
/// `route` field) — `route-no-slash` fixture.
fn de_route<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
  let route = String::deserialize(deserializer)?;
  if route.starts_with('/') {
    Ok(route)
  } else {
    Err(serde::de::Error::custom(format!(
      "route must start with '/': {route}"
    )))
  }
}

/// UUIDv7 only (version nibble 7, RFC 9562) — `id-not-v7` fixture.
fn de_uuidv7<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Uuid, D::Error> {
  let uuid = Uuid::deserialize(deserializer)?;
  if uuid.get_version_num() != 7 {
    return Err(serde::de::Error::custom(format!("not a UUIDv7: {uuid}")));
  }
  Ok(uuid)
}

/// `seq` is a non-negative integer (`seq-negative` fixture); floats are
/// rejected by the i64 deserialization itself (`seq-float` fixture).
fn de_nonneg_i64<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<i64, D::Error> {
  let seq = i64::deserialize(deserializer)?;
  if seq < 0 {
    return Err(serde::de::Error::custom(format!(
      "seq must be >= 0, got {seq}"
    )));
  }
  Ok(seq)
}

// ---------------------------------------------------------------------------
// Golden tests: round-trip every schema fixture
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
  use super::*;
  use std::fs;
  use std::path::PathBuf;

  fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../schema/fixtures")
  }

  /// (file stem, file contents) for every *.json in `schema/fixtures/<dir>`.
  fn load(dir: &str) -> Vec<(String, String)> {
    let path = fixtures_dir().join(dir);
    let mut files: Vec<(String, String)> = fs::read_dir(&path)
      .unwrap_or_else(|e| panic!("read_dir {}: {e}", path.display()))
      .map(|entry| {
        let entry = entry.expect("dir entry");
        let name = entry
          .path()
          .file_stem()
          .unwrap()
          .to_string_lossy()
          .into_owned();
        let contents = fs::read_to_string(entry.path()).expect("read fixture");
        (name, contents)
      })
      .collect();
    files.sort();
    files
  }

  fn assert_round_trip(original: &str, reloaded: &str, name: &str) {
    let a: Value = serde_json::from_str(original).expect("fixture is valid JSON");
    let b: Value = serde_json::from_str(reloaded).expect("re-serialized to valid JSON");
    assert_eq!(a, b, "round-trip changed the document: {name}");
  }

  #[test]
  fn valid_event_fixtures_round_trip() {
    let fixtures = load("valid/event");
    assert!(fixtures.len() >= 16, "expected the full valid fixture set");
    for (name, contents) in &fixtures {
      let event = Event::from_json_str(contents)
        .unwrap_or_else(|e| panic!("valid/event/{name}.json rejected: {e}"));
      assert_round_trip(contents, &event.to_json_string(), name);
    }
  }

  #[test]
  fn invalid_event_fixtures_rejected() {
    for (name, contents) in &load("invalid/event") {
      assert!(
        Event::from_json_str(contents).is_err(),
        "invalid/event/{name}.json was accepted"
      );
    }
  }

  #[test]
  fn valid_runner_event_fixtures_round_trip() {
    for (name, contents) in &load("valid/runner-event") {
      let event = RunnerEvent::from_json_str(contents)
        .unwrap_or_else(|e| panic!("valid/runner-event/{name}.json rejected: {e}"));
      assert_round_trip(contents, &event.to_json_string(), name);
    }
  }

  #[test]
  fn invalid_runner_event_fixtures_rejected() {
    for (name, contents) in &load("invalid/runner-event") {
      assert!(
        RunnerEvent::from_json_str(contents).is_err(),
        "invalid/runner-event/{name}.json was accepted"
      );
    }
  }

  #[test]
  fn valid_invocation_fixtures_round_trip() {
    for (name, contents) in &load("valid/invocation") {
      let request: InvocationRequest = serde_json::from_str(contents)
        .unwrap_or_else(|e| panic!("valid/invocation/{name}.json rejected: {e}"));
      let reloaded = serde_json::to_string(&request).expect("InvocationRequest serialization");
      assert_round_trip(contents, &reloaded, name);
    }
  }

  #[test]
  fn invalid_invocation_fixtures_rejected() {
    for (name, contents) in &load("invalid/invocation") {
      let result: Result<InvocationRequest, _> = serde_json::from_str(contents);
      assert!(
        result.is_err(),
        "invalid/invocation/{name}.json was accepted"
      );
    }
  }

  // -- behavior tests beyond the fixtures --------------------------------

  #[test]
  fn event_type_matches_payload_after_deserialization() {
    let event = Event::from_json_str(
            r#"{
                "id": "0192f3a4-b600-7000-8000-000000000005",
                "session_id": "0192f3a4-b5c6-7e2f-8a9b-1c2d3e4f5a6b",
                "deployment_id": "0192f3a0-1111-7abc-9def-0123456789ab",
                "seq": 5,
                "type": "TOOL_CALL",
                "payload": {"tool_call_id": "call_01", "name": "lookup_order", "arguments": {"order_id": "A-1234"}},
                "created_at": "2026-01-15T12:00:05.123456Z"
            }"#,
        )
        .unwrap();
    assert_eq!(event.event_type, EventType::ToolCall);
    assert_eq!(event.payload.event_type(), EventType::ToolCall);
    match event.payload {
      EventPayload::ToolCall(tc) => {
        assert_eq!(tc.name, "lookup_order");
        assert_eq!(tc.arguments["order_id"], "A-1234");
      }
      other => panic!("wrong variant: {other:?}"),
    }
  }

  #[test]
  fn platform_types_are_not_runner_emittable() {
    assert!(EventType::SessionCreated.as_runner().is_none());
    assert!(EventType::InvocationStarted.as_runner().is_none());
    assert!(EventType::ToolApprovalGranted.as_runner().is_none());
    assert!(EventType::ToolApprovalDenied.as_runner().is_none());
    assert!(EventType::SessionError.as_runner().is_none());
    assert!(EventType::MessageStart.as_runner().is_some());
  }

  #[test]
  fn runner_event_of_platform_type_is_rejected_via_constructor() {
    let payload = EventPayload::SessionError(SessionError {
      message: "boom".into(),
      extra: Extra::default(),
    });
    assert!(RunnerEvent::new(payload).is_err());
  }

  #[test]
  fn runner_event_widens_into_new_event_and_then_a_full_envelope() {
    let runner = RunnerEvent::from_json_str(
            r#"{"type": "MESSAGE_DELTA", "payload": {"message_id": "m1", "delta": {"type": "text", "text": "hi"}}}"#,
        )
        .unwrap();
    let new_event = NewEvent::from(runner);
    assert_eq!(new_event.event_type, EventType::MessageDelta);
    assert_eq!(new_event.id.get_version_num(), 7);

    let event = new_event.into_event(Uuid::now_v7(), Uuid::now_v7(), 7, Utc::now());
    assert_eq!(event.seq, 7);
    assert!(event.to_json_string().contains(r#""type":"MESSAGE_DELTA""#));
  }

  #[test]
  fn unknown_envelope_fields_are_preserved() {
    let event = Event::from_json_str(
      r#"{
                "id": "0192f3a4-b600-7000-8000-00000000000e",
                "session_id": "0192f3a4-b5c6-7e2f-8a9b-1c2d3e4f5a6b",
                "deployment_id": "0192f3a0-1111-7abc-9def-0123456789ab",
                "seq": 14,
                "type": "NO_MORE_ACTIONS",
                "payload": {},
                "created_at": "2026-01-15T12:00:14.123456Z",
                "future_envelope_field": {"v": 1}
            }"#,
    )
    .unwrap();
    assert_eq!(event.extra.map["future_envelope_field"]["v"], 1);
    let out: Value = serde_json::from_str(&event.to_json_string()).unwrap();
    assert_eq!(out["future_envelope_field"]["v"], 1);
  }

  #[test]
  fn delta_shapes() {
    // rejected: bare strings, scalars, arrays, unknown kinds, missing text
    for bad in [
      r#""hi""#,
      "42",
      "true",
      "null",
      "[1]",
      r#"{"type": "image"}"#,
      r#"{"type": "text"}"#,
    ] {
      let v: Value = serde_json::from_str(bad).unwrap();
      assert!(
        serde_json::from_value::<TypedDelta>(v).is_err(),
        "delta {bad} should be rejected"
      );
    }
    // typed text round-trips (extra fields preserved)
    let d: TypedDelta =
      serde_json::from_str(r#"{"type": "text", "text": "hi", "future": 1}"#).unwrap();
    assert!(matches!(d, TypedDelta::Text { ref text, .. } if text == "hi"));
    let out = serde_json::to_value(&d).unwrap();
    assert_eq!(out["type"], "text");
    assert_eq!(out["future"], 1);
    // typed reasoning round-trips (extra fields preserved)
    let d: TypedDelta =
      serde_json::from_str(r#"{"type": "reasoning", "text": "hmm", "future": 1}"#).unwrap();
    assert!(matches!(d, TypedDelta::Reasoning { ref text, .. } if text == "hmm"));
    let out = serde_json::to_value(&d).unwrap();
    assert_eq!(out["type"], "reasoning");
    assert_eq!(out["future"], 1);
    // constructors
    assert!(matches!(TypedDelta::text("x"), TypedDelta::Text { .. }));
    assert!(matches!(
      TypedDelta::reasoning("y"),
      TypedDelta::Reasoning { .. }
    ));
  }

  #[test]
  fn uuidv7_check_rejects_other_versions() {
    let v4 = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
    assert_ne!(v4.get_version_num(), 7);
    let v7 = Uuid::now_v7();
    assert_eq!(v7.get_version_num(), 7);
  }
}
