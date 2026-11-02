//! AG-UI 1.0 projection over the journal.
//!
//! The AG-UI endpoint is another VIEW over the journal — the same move as
//! platform SSE, transcripts and the dashboard. This module is the pure
//! half of it: journal events in, AG-UI events out, no I/O, no auth. The
//! normative mapping table lives in `schema/agui-mapping.md`; the golden
//! fixtures under `schema/fixtures/*/agui/` pin it.
//!
//! - [`Projector`] — stateful per-run mapper (one run = one invocation's
//!   journal events), enforcing AG-UI stream discipline (everything a run
//!   opened is closed before `RUN_FINISHED`/`RUN_ERROR`).
//! - [`fold_messages`] — the journal folded into AG-UI messages, for
//!   `MESSAGES_SNAPSHOT` (mirrors the SDK's canonical `ctx.messages` fold,
//!   in AG-UI message shapes).
//! - [`RunAgentInput`] — the request body (`threadId`/`runId`/`messages`…).
//! - [`connect_context`] — what a connect re-streams: the
//!   thread's latest invocation, projected like a run (snapshot + its events).
//!
//! Field names follow the pinned upstream schema
//! (`spec/1.0/schema.json`), not our prose: `TOOL_CALL_START.toolCallName`,
//! `TOOL_CALL_RESULT.messageId` (required), `REASONING_MESSAGE_START.role`.

use std::collections::VecDeque;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::events::{Event, EventPayload, InvocationReason, ToolApprovalRequired, TypedDelta};

/// The AG-UI protocol version this endpoint speaks (`RUN_STARTED.protocolVersion`
/// declares it; requests are checked by [`supports_protocol_version`]).
pub const PROTOCOL_VERSION: &str = "1.0";

/// Does this endpoint accept a request declaring `version`? Any `1.x`
/// (same major as [`PROTOCOL_VERSION`]): minor versions are additive, so a
/// client upgrade to 1.1 must not break every run. Other majors — and
/// anything that is not a dotted numeric version — are refused.
pub fn supports_protocol_version(version: &str) -> bool {
  let mut parts = version.split('.');
  parts.next() == Some("1") && parts.all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// `INVOCATION_STARTED.extra` namespace for everything the AG-UI endpoint
/// journals: ONE `ag_ui` object (`extra.ag_ui.run_id`, …), so protocol-
/// specific fields never collide with core payload fields or with another
/// integration's extras. The key constants below are keys INSIDE it.
pub const EXTRA_NAMESPACE: &str = "ag_ui";

/// The AG-UI `runId` of the run that started the invocation (every AG-UI run
/// starts exactly one invocation). The invocation id stays server-minted;
/// this records the client's id for the same run, so a client-side `runId`
/// (CopilotKit logs, an error report) can be looked up in the journal.
pub const RUN_ID_KEY: &str = "run_id";

/// The AG-UI client's own id for the user message that started the turn.
/// The journal has no user-message ids (it records `input`), but
/// `MESSAGES_SNAPSHOT` reconciles with the client's message list BY ID —
/// without the client's id every snapshot would duplicate the user's
/// messages.
pub const CLIENT_MESSAGE_ID_KEY: &str = "client_message_id";

/// The AG-UI request's `forwardedProps` and `context` (passed through to the
/// runner via its transcript; the invoke contract's `input` stays the bare
/// message content).
pub const FORWARDED_PROPS_KEY: &str = "forwarded_props";
pub const CONTEXT_KEY: &str = "context";

/// The `INVOCATION_STARTED.extra` an AG-UI run journals, under
/// [`EXTRA_NAMESPACE`]: always its `runId`; for a user-message run also the
/// client's message id, `forwardedProps` and `context` when non-empty (a
/// resume run passes `None` for those).
pub fn invocation_extra(
  run_id: &str,
  client_message_id: Option<String>,
  forwarded_props: Option<Value>,
  context: Option<Vec<Value>>,
) -> crate::events::Extra {
  let mut ns = Map::new();
  ns.insert(RUN_ID_KEY.into(), Value::String(run_id.to_owned()));
  if let Some(id) = client_message_id {
    ns.insert(CLIENT_MESSAGE_ID_KEY.into(), Value::String(id));
  }
  // Clients send `forwardedProps: {}` by default (`HttpAgent`): an empty
  // object carries nothing, so skip it, like an empty `context`.
  if let Some(props) = forwarded_props.filter(|p| !p.as_object().is_some_and(|o| o.is_empty())) {
    ns.insert(FORWARDED_PROPS_KEY.into(), props);
  }
  if let Some(context) = context.filter(|c| !c.is_empty()) {
    ns.insert(CONTEXT_KEY.into(), Value::Array(context));
  }
  let mut extra = crate::events::Extra::default();
  extra.map.insert(EXTRA_NAMESPACE.into(), Value::Object(ns));
  extra
}

/// The [`EXTRA_NAMESPACE`] object of an `INVOCATION_STARTED`'s extras —
/// `None` for invocations not started by an AG-UI run.
pub fn invocation_namespace(extra: &crate::events::Extra) -> Option<&Map<String, Value>> {
  extra.map.get(EXTRA_NAMESPACE).and_then(Value::as_object)
}

/// `Interrupt.reason` for a tool-approval interrupt.
pub const INTERRUPT_REASON_TOOL_APPROVAL: &str = "tool_approval";

// ---------------------------------------------------------------------------
// Request: RunAgentInput
// ---------------------------------------------------------------------------

/// `RunAgentInput` (`#RunAgentInput` in the upstream schema). Unknown
/// top-level fields are dropped (the schema is closed; we echo only what it
/// declares). `messages`/`tools`/`context` items are kept as raw JSON: they
/// are echoed verbatim in `RUN_STARTED.input` and otherwise opaque to us.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunAgentInput {
  pub thread_id: String,
  pub run_id: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub protocol_version: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub parent_run_id: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub state: Option<Value>,
  pub messages: Vec<Value>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub tools: Option<Vec<Value>>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub context: Option<Vec<Value>>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub forwarded_props: Option<Value>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub resume: Option<Vec<ResumeEntry>>,
}

/// An answer to one interrupt, sent on the run that continues from it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumeEntry {
  pub interrupt_id: String,
  pub status: ResumeStatus,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub payload: Option<Value>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub metadata: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResumeStatus {
  Resolved,
  Cancelled,
}

/// The user input of a user-message run, derived from the request's trailing
/// message — see [`RunAgentInput::trailing_user_input`].
#[derive(Debug, Clone, PartialEq)]
pub struct UserInput {
  /// The client's id for the message (echoed back in snapshots).
  pub message_id: Option<String>,
  /// Journaled as `INVOCATION_STARTED.reason.input`: the message `content`
  /// verbatim (a string; content parts are not supported).
  pub input: crate::events::UserMessageInput,
  /// The request carries conversation history before the trailing message
  /// (any prior message other than `system`/`developer`). The journal is
  /// the thread's history, so this is ignored on an existing thread, but a
  /// NEW thread cannot import it: that run is refused (no history import
  /// yet), rather than silently starting the agent cold.
  pub has_history: bool,
}

impl RunAgentInput {
  /// Does the request carry resume entries (a resume run)?
  pub fn is_resume(&self) -> bool {
    self.resume.as_ref().is_some_and(|r| !r.is_empty())
  }

  /// The user input of a user-message run: the LAST message must be a
  /// `user` message with string `content`.
  pub fn trailing_user_input(&self) -> Result<UserInput, String> {
    let last = self
      .messages
      .last()
      .ok_or_else(|| "messages is empty: a run needs a trailing user message".to_string())?;
    if last.get("role").and_then(Value::as_str) != Some("user") {
      return Err(
        "the last message must be a user message (tool-result runs for frontend tools are not supported yet)"
          .into(),
      );
    }
    let input = match last.get("content") {
      Some(content @ Value::String(_)) => content.clone(),
      Some(Value::Array(_)) => {
        return Err(
          "content parts are not supported: send the trailing user message's `content` as a string"
            .into(),
        );
      }
      _ => return Err("the trailing user message needs string `content`".into()),
    };
    let input = crate::events::UserMessageInput::new(input)
      .map_err(|_| "the trailing user message is empty: it needs some text".to_string())?;
    let prior = &self.messages[..self.messages.len() - 1];
    let has_history = prior.iter().any(|m| {
      !matches!(
        m.get("role").and_then(Value::as_str),
        Some("system" | "developer")
      )
    });
    Ok(UserInput {
      message_id: last.get("id").and_then(Value::as_str).map(str::to_owned),
      input,
      has_history,
    })
  }
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// One AG-UI event: the typed body plus the `BaseEvent` fields we set
/// (`timestamp` ms, open `metadata`). Serializes to the wire JSON.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AgUiEvent {
  #[serde(flatten)]
  pub body: AgUiBody,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub timestamp: Option<i64>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub metadata: Option<Map<String, Value>>,
}

impl AgUiEvent {
  /// The wire JSON (one SSE `data:` frame).
  pub fn to_json(&self) -> String {
    serde_json::to_string(self).expect("AgUiEvent serialization is infallible")
  }

  fn bare(body: AgUiBody) -> Self {
    AgUiEvent {
      body,
      timestamp: None,
      metadata: None,
    }
  }
}

/// The AG-UI 1.0 events this endpoint emits (a subset of the taxonomy —
/// see the non-goals in `schema/agui-mapping.md`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(
  tag = "type",
  rename_all = "SCREAMING_SNAKE_CASE",
  rename_all_fields = "camelCase"
)]
pub enum AgUiBody {
  RunStarted {
    thread_id: String,
    run_id: String,
    protocol_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    input: Option<Value>,
  },
  RunFinished {
    thread_id: String,
    run_id: String,
    outcome: RunOutcome,
  },
  RunError {
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<String>,
  },
  TextMessageStart {
    message_id: String,
    role: String,
  },
  TextMessageContent {
    message_id: String,
    delta: String,
  },
  TextMessageEnd {
    message_id: String,
  },
  ToolCallStart {
    tool_call_id: String,
    tool_call_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_message_id: Option<String>,
  },
  ToolCallArgs {
    tool_call_id: String,
    delta: String,
  },
  ToolCallEnd {
    tool_call_id: String,
  },
  ToolCallResult {
    message_id: String,
    tool_call_id: String,
    content: String,
    role: String,
  },
  ReasoningStart {
    message_id: String,
  },
  ReasoningMessageStart {
    message_id: String,
    role: String,
  },
  ReasoningMessageContent {
    message_id: String,
    delta: String,
  },
  ReasoningMessageEnd {
    message_id: String,
  },
  ReasoningEnd {
    message_id: String,
  },
  MessagesSnapshot {
    messages: Vec<Value>,
  },
}

/// `RUN_FINISHED.outcome` — the closed union (`success` | `interrupt`;
/// `cancelled` is part of the protocol but we never cancel runs).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(
  tag = "type",
  rename_all = "snake_case",
  rename_all_fields = "camelCase"
)]
pub enum RunOutcome {
  Success,
  Interrupt { interrupts: Vec<Interrupt> },
}

/// `Interrupt` — what a suspended run waits for. Ours is always a tool
/// approval (`responseSchema: {"type": "boolean"}`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Interrupt {
  pub id: String,
  pub reason: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub message: Option<String>,
  pub tool_call_id: String,
  pub response_schema: Value,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub metadata: Option<Map<String, Value>>,
}

// ---------------------------------------------------------------------------
// Id derivations (stable across live streaming and journal folds)
// ---------------------------------------------------------------------------

/// The reasoning message id for an assistant message's reasoning deltas.
/// Deliberately NOT the assistant message's own id: AG-UI clients key their
/// message list by id, and a reasoning message under the assistant's id
/// would stream reasoning text into the assistant's visible content.
pub fn reasoning_message_id(message_id: &str) -> String {
  format!("{message_id}:reasoning")
}

/// The id of the tool message minted by a tool result (the journal has no
/// message ids for results; the call id is unique per call).
pub fn tool_message_id(tool_call_id: &str) -> String {
  format!("tool-result-{tool_call_id}")
}

fn user_message_id(event: &Event, extra_id: Option<&str>) -> String {
  extra_id
    .map(str::to_owned)
    .unwrap_or_else(|| format!("user-{}", event.id))
}

/// Arguments/results are opaque journal JSON; AG-UI carries text. Strings
/// pass through verbatim, anything else is JSON-encoded (the SDK fold's
/// rule).
fn stringify(value: &Value) -> String {
  match value {
    Value::String(s) => s.clone(),
    other => other.to_string(),
  }
}

fn millis(at: DateTime<Utc>) -> i64 {
  at.timestamp_millis()
}

// ---------------------------------------------------------------------------
// Projector
// ---------------------------------------------------------------------------

/// Everything a run's projection needs besides the journal events.
#[derive(Debug, Clone)]
pub struct RunContext {
  pub thread_id: String,
  pub run_id: String,
  /// Echoed as `RUN_STARTED.input` (the request, as received).
  pub input: Option<Value>,
  /// When `Some`, emitted as `MESSAGES_SNAPSHOT` right after `RUN_STARTED`
  /// (every run on an EXISTING thread; see agui-mapping.md "History").
  pub snapshot: Option<Vec<Value>>,
}

/// Maps ONE invocation's journal events to AG-UI events. Feed it the
/// invocation's events in `seq` order starting at its `INVOCATION_STARTED`;
/// it reports [`Projector::is_done`] after the terminal event (sentinel,
/// approval request, error) — the run stream ends there, NOT at the session
/// tail.
#[derive(Debug)]
pub struct Projector {
  ctx: RunContext,
  started: bool,
  done: bool,
  /// Text messages with a `TEXT_MESSAGE_START` and no `TEXT_MESSAGE_END`.
  open_text: Vec<String>,
  /// The reasoning message currently bracketed (REASONING_START … END).
  open_reasoning: Option<String>,
  /// The latest assistant message started in this run: `TOOL_CALL`s attach
  /// to it (the SDK fold's `current` rule), via `parentMessageId`.
  current_message: Option<String>,
  /// Seq of the last event fed, stamped on synthesized terminal events.
  last_seq: Option<i64>,
  last_at: Option<DateTime<Utc>>,
}

impl Projector {
  pub fn new(ctx: RunContext) -> Self {
    Projector {
      ctx,
      started: false,
      done: false,
      open_text: Vec::new(),
      open_reasoning: None,
      current_message: None,
      last_seq: None,
      last_at: None,
    }
  }

  /// True once the run's terminal event (`RUN_FINISHED` / `RUN_ERROR`) has
  /// been produced; further events are ignored.
  pub fn is_done(&self) -> bool {
    self.done
  }

  /// Project one journal event of this run.
  pub fn push(&mut self, event: &Event) -> Vec<AgUiEvent> {
    if self.done {
      return Vec::new();
    }
    self.last_seq = Some(event.seq);
    self.last_at = Some(event.created_at);
    let mut out = Out::new(event);

    if !self.started {
      // The run begins at its INVOCATION_STARTED; anything before it (not
      // fed by a well-formed caller) is not part of the run.
      if let EventPayload::InvocationStarted(_) = &event.payload {
        self.started = true;
        out.push(AgUiBody::RunStarted {
          thread_id: self.ctx.thread_id.clone(),
          run_id: self.ctx.run_id.clone(),
          protocol_version: PROTOCOL_VERSION.into(),
          input: self.ctx.input.clone(),
        });
        if let Some(messages) = self.ctx.snapshot.take() {
          out.push(AgUiBody::MessagesSnapshot { messages });
        }
      }
      return out.finish();
    }

    match &event.payload {
      EventPayload::InvocationStarted(_) => {
        // A newer invocation began before this run's terminal event: the
        // run was superseded (crash recovery re-invoked the session).
        self.close_all(&mut out);
        out.push(AgUiBody::RunError {
          message: "run superseded by a newer invocation of this thread".into(),
          code: Some("superseded".into()),
        });
        self.done = true;
      }
      EventPayload::MessageStart(m) => {
        self.close_reasoning(&mut out);
        self.open_text_message(&mut out, &m.message_id);
        self.current_message = Some(m.message_id.clone());
      }
      EventPayload::MessageDelta(d) => match &d.delta {
        TypedDelta::Text { text, .. } => {
          self.close_reasoning(&mut out);
          // Defensive: a delta without a START (the sidecar validates the
          // stream shape, but not message-id pairing) still yields a
          // well-formed AG-UI message.
          self.open_text_message(&mut out, &d.message_id);
          out.push(AgUiBody::TextMessageContent {
            message_id: d.message_id.clone(),
            delta: text.clone(),
          });
        }
        TypedDelta::Reasoning { text, .. } => {
          let rid = reasoning_message_id(&d.message_id);
          if self.open_reasoning.as_deref() != Some(rid.as_str()) {
            self.close_reasoning(&mut out);
            out.push(AgUiBody::ReasoningStart {
              message_id: rid.clone(),
            });
            out.push(AgUiBody::ReasoningMessageStart {
              message_id: rid.clone(),
              role: "reasoning".into(),
            });
            self.open_reasoning = Some(rid.clone());
          }
          out.push(AgUiBody::ReasoningMessageContent {
            message_id: rid,
            delta: text.clone(),
          });
        }
      },
      EventPayload::MessageEnd(m) => {
        self.close_reasoning(&mut out);
        if let Some(pos) = self.open_text.iter().position(|id| *id == m.message_id) {
          self.open_text.remove(pos);
          out.push(AgUiBody::TextMessageEnd {
            message_id: m.message_id.clone(),
          });
        }
      }
      EventPayload::ToolCall(c) => {
        self.close_reasoning(&mut out);
        out.push(AgUiBody::ToolCallStart {
          tool_call_id: c.tool_call_id.clone(),
          tool_call_name: c.name.clone(),
          parent_message_id: self.current_message.clone(),
        });
        // One ARGS chunk: the journal holds complete arguments (approval
        // resume requires that anyway).
        out.push(AgUiBody::ToolCallArgs {
          tool_call_id: c.tool_call_id.clone(),
          delta: stringify(&c.arguments),
        });
        out.push(AgUiBody::ToolCallEnd {
          tool_call_id: c.tool_call_id.clone(),
        });
      }
      EventPayload::ToolResult(r) => {
        self.close_reasoning(&mut out);
        out.push(AgUiBody::ToolCallResult {
          message_id: tool_message_id(&r.tool_call_id),
          tool_call_id: r.tool_call_id.clone(),
          content: stringify(&r.result),
          role: "tool".into(),
        });
      }
      EventPayload::ToolApprovalRequired(a) => {
        self.close_all(&mut out);
        out.push(AgUiBody::RunFinished {
          thread_id: self.ctx.thread_id.clone(),
          run_id: self.ctx.run_id.clone(),
          outcome: RunOutcome::Interrupt {
            interrupts: vec![interrupt_for(event, a)],
          },
        });
        self.done = true;
      }
      EventPayload::NoMoreActions(_) => {
        self.close_all(&mut out);
        out.push(AgUiBody::RunFinished {
          thread_id: self.ctx.thread_id.clone(),
          run_id: self.ctx.run_id.clone(),
          outcome: RunOutcome::Success,
        });
        self.done = true;
      }
      // Runner `ERROR` ends the turn (the sidecar journals the sentinel
      // after it) — a failed run, not a successful one.
      EventPayload::Error(e) => {
        self.close_all(&mut out);
        out.push(AgUiBody::RunError {
          message: e.message.clone(),
          code: e.code.clone(),
        });
        self.done = true;
      }
      EventPayload::SessionError(e) => {
        self.close_all(&mut out);
        out.push(AgUiBody::RunError {
          message: e.message.clone(),
          code: None,
        });
        self.done = true;
      }
      // Journaled, not UI-relevant.
      EventPayload::SessionCreated(_)
      | EventPayload::ToolApprovalGranted(_)
      | EventPayload::ToolApprovalDenied(_)
      | EventPayload::TranscriptCompacted(_) => {}
    }
    out.finish()
  }

  /// The journal stream ended (or failed) before a terminal event — token
  /// expiry, a tail error. Closes whatever is open and ends the run with a
  /// `RUN_ERROR` so every stream terminates with a terminal event.
  pub fn abort(&mut self, message: &str, code: &str) -> Vec<AgUiEvent> {
    if self.done {
      return Vec::new();
    }
    let mut out = Out::synthetic(self.last_seq, self.last_at);
    if !self.started {
      self.started = true;
      out.push(AgUiBody::RunStarted {
        thread_id: self.ctx.thread_id.clone(),
        run_id: self.ctx.run_id.clone(),
        protocol_version: PROTOCOL_VERSION.into(),
        input: self.ctx.input.clone(),
      });
    }
    self.close_all(&mut out);
    out.push(AgUiBody::RunError {
      message: message.into(),
      code: Some(code.into()),
    });
    self.done = true;
    out.finish()
  }

  fn open_text_message(&mut self, out: &mut Out, message_id: &str) {
    if !self.open_text.iter().any(|id| id == message_id) {
      self.open_text.push(message_id.to_owned());
      out.push(AgUiBody::TextMessageStart {
        message_id: message_id.to_owned(),
        role: "assistant".into(),
      });
    }
  }

  fn close_reasoning(&mut self, out: &mut Out) {
    if let Some(rid) = self.open_reasoning.take() {
      out.push(AgUiBody::ReasoningMessageEnd {
        message_id: rid.clone(),
      });
      out.push(AgUiBody::ReasoningEnd { message_id: rid });
    }
  }

  /// Close every open bracket (before a terminal event: a crashed turn can
  /// leave a message with no `MESSAGE_END`).
  fn close_all(&mut self, out: &mut Out) {
    self.close_reasoning(out);
    for id in std::mem::take(&mut self.open_text) {
      out.push(AgUiBody::TextMessageEnd { message_id: id });
    }
  }
}

fn interrupt_for(event: &Event, approval: &ToolApprovalRequired) -> Interrupt {
  // The AG-UI Interrupt carries no tool name/arguments, and the approval
  // request is not a TOOL_CALL (the runner may journal only the approval
  // event), so the client would otherwise not know WHAT is being approved.
  let mut metadata = Map::new();
  metadata.insert("toolName".into(), json!(approval.name));
  metadata.insert("arguments".into(), approval.arguments.clone());
  Interrupt {
    id: event.id.to_string(),
    reason: INTERRUPT_REASON_TOOL_APPROVAL.into(),
    message: approval.reason.clone(),
    tool_call_id: approval.tool_call_id.clone(),
    response_schema: json!({"type": "boolean"}),
    metadata: Some(metadata),
  }
}

/// Collects the AG-UI events one journal event produces, stamping each with
/// the journal event's `timestamp` (ms from `created_at`) and
/// `metadata: {seq}`.
struct Out {
  seq: Option<i64>,
  at: Option<DateTime<Utc>>,
  events: VecDeque<AgUiEvent>,
}

impl Out {
  fn new(event: &Event) -> Self {
    Out {
      seq: Some(event.seq),
      at: Some(event.created_at),
      events: VecDeque::new(),
    }
  }

  fn synthetic(seq: Option<i64>, at: Option<DateTime<Utc>>) -> Self {
    Out {
      seq,
      at,
      events: VecDeque::new(),
    }
  }

  fn push(&mut self, body: AgUiBody) {
    let mut ev = AgUiEvent::bare(body);
    ev.timestamp = self.at.map(millis);
    if let Some(seq) = self.seq {
      let mut metadata = Map::new();
      metadata.insert("seq".into(), json!(seq));
      ev.metadata = Some(metadata);
    }
    self.events.push_back(ev);
  }

  fn finish(self) -> Vec<AgUiEvent> {
    self.events.into()
  }
}

// ---------------------------------------------------------------------------
// Journal → AG-UI messages (MESSAGES_SNAPSHOT)
// ---------------------------------------------------------------------------

/// Fold journal events into AG-UI messages — the `MESSAGES_SNAPSHOT`
/// payload. Mirrors the SDK's canonical fold (`SessionContext.messages`),
/// emitting AG-UI message shapes and ids that MATCH what live streaming
/// produced (so a client reconciling by id sees no duplicates):
///
/// - user turns: `INVOCATION_STARTED` with a `user_message` reason (id =
///   the client's own message id when journaled, else `user-<event id>`)
/// - assistant messages: `MESSAGE_START/DELTA/END` (`text` deltas → `content`;
///   `reasoning` deltas → a separate `reasoning` message, [`reasoning_message_id`])
/// - `TOOL_CALL` attaches to the latest assistant message OF THE SAME
///   INVOCATION as `toolCalls` (a call with no message to attach to mints
///   one, id = the call id — exactly what an AG-UI client does for a
///   parent-less call)
/// - `TOOL_RESULT` → `tool` message ([`tool_message_id`]), placed right after
///   the assistant message that issued the call (the AG-UI reducer's rule —
///   and the provider contract that a call is followed by its result)
///
/// `TRANSCRIPT_COMPACTED` and the crash-continuation stitch (a synthesized
/// user prompt) are LLM-CONTEXT management, not conversation history: the
/// journal is never rewritten, so the fold shows the full original
/// conversation to the user.
pub fn fold_messages(events: &[Event]) -> Vec<Value> {
  let mut msgs: Vec<Value> = Vec::new();
  // message_id → index in `msgs` (assistant), reasoning id → index.
  let mut text_idx: std::collections::HashMap<String, usize> = Default::default();
  let mut reasoning_idx: std::collections::HashMap<String, usize> = Default::default();
  let mut current: Option<usize> = None;

  for event in events {
    match &event.payload {
      EventPayload::InvocationStarted(inv) => {
        // Every invocation starts with no current assistant message, as the
        // live Projector does per run: a tool-only response opening a later
        // turn (or a resumed one) must not attach to an earlier turn's
        // message (the SDK fold has the same rule).
        current = None;
        if let InvocationReason::UserMessage { input, .. } = &inv.reason {
          let extra_id = invocation_namespace(&inv.extra)
            .and_then(|ns| ns.get(CLIENT_MESSAGE_ID_KEY))
            .and_then(Value::as_str);
          msgs.push(json!({
            "id": user_message_id(event, extra_id),
            "role": "user",
            "content": input,
          }));
        }
      }
      EventPayload::MessageStart(m) => {
        msgs.push(json!({"id": m.message_id, "role": "assistant"}));
        let idx = msgs.len() - 1;
        text_idx.insert(m.message_id.clone(), idx);
        current = Some(idx);
      }
      EventPayload::MessageDelta(d) => match &d.delta {
        TypedDelta::Text { text, .. } => {
          let idx = match text_idx.get(&d.message_id) {
            Some(idx) => *idx,
            None => {
              // Delta without a START (defensive; mirrors the Projector).
              msgs.push(json!({"id": d.message_id, "role": "assistant"}));
              text_idx.insert(d.message_id.clone(), msgs.len() - 1);
              current = Some(msgs.len() - 1);
              msgs.len() - 1
            }
          };
          append_string(&mut msgs[idx], "content", text);
        }
        TypedDelta::Reasoning { text, .. } => {
          let rid = reasoning_message_id(&d.message_id);
          let idx = match reasoning_idx.get(&rid) {
            Some(idx) => *idx,
            None => {
              msgs.push(json!({"id": rid, "role": "reasoning", "content": ""}));
              reasoning_idx.insert(rid, msgs.len() - 1);
              msgs.len() - 1
            }
          };
          append_string(&mut msgs[idx], "content", text);
        }
      },
      EventPayload::ToolCall(c) => {
        attach_tool_call(
          &mut msgs,
          &mut current,
          &c.tool_call_id,
          &c.name,
          &c.arguments,
        );
      }
      // The runner may journal only the approval request (no TOOL_CALL) —
      // fold it as the pending, unanswered call so the resumed run's
      // TOOL_CALL_RESULT has a call to answer.
      EventPayload::ToolApprovalRequired(a) => {
        let known = msgs.iter().any(|m| {
          m.get("toolCalls")
            .and_then(Value::as_array)
            .is_some_and(|calls| calls.iter().any(|c| c["id"] == a.tool_call_id.as_str()))
        });
        if !known {
          attach_tool_call(
            &mut msgs,
            &mut current,
            &a.tool_call_id,
            &a.name,
            &a.arguments,
          );
        }
      }
      EventPayload::ToolResult(r) => {
        let tool = json!({
          "id": tool_message_id(&r.tool_call_id),
          "role": "tool",
          "toolCallId": r.tool_call_id,
          "content": stringify(&r.result),
        });
        // Right after the assistant message that issued the call (past any
        // results already recorded for it), else at the end.
        let owner = msgs.iter().position(|m| {
          m["role"] == "assistant"
            && m
              .get("toolCalls")
              .and_then(Value::as_array)
              .is_some_and(|calls| calls.iter().any(|c| c["id"] == r.tool_call_id.as_str()))
        });
        match owner {
          Some(owner) => {
            let mut at = owner + 1;
            while at < msgs.len() && msgs[at]["role"] == "tool" {
              at += 1;
            }
            msgs.insert(at, tool);
            // Indices recorded for later deltas shift past the insertion.
            for idx in text_idx.values_mut().chain(reasoning_idx.values_mut()) {
              if *idx >= at {
                *idx += 1;
              }
            }
            if let Some(cur) = current.as_mut()
              && *cur >= at
            {
              *cur += 1;
            }
          }
          None => msgs.push(tool),
        }
      }
      _ => {}
    }
  }
  msgs
}

/// Attach a tool call to the latest assistant message (minting one, id =
/// the call id, when there is none — what an AG-UI client does for a
/// parent-less `TOOL_CALL_START`).
fn attach_tool_call(
  msgs: &mut Vec<Value>,
  current: &mut Option<usize>,
  tool_call_id: &str,
  name: &str,
  arguments: &Value,
) {
  let idx = match *current {
    Some(idx) => idx,
    None => {
      msgs.push(json!({"id": tool_call_id, "role": "assistant"}));
      *current = Some(msgs.len() - 1);
      msgs.len() - 1
    }
  };
  let call = json!({
    "id": tool_call_id,
    "type": "function",
    "function": {"name": name, "arguments": stringify(arguments)},
  });
  let calls = msgs[idx]
    .as_object_mut()
    .expect("messages are objects")
    .entry("toolCalls")
    .or_insert_with(|| Value::Array(Vec::new()));
  if let Value::Array(calls) = calls {
    calls.push(call);
  }
}

fn append_string(message: &mut Value, key: &str, text: &str) {
  let obj = message.as_object_mut().expect("messages are objects");
  let current = obj.get(key).and_then(Value::as_str).unwrap_or("");
  obj.insert(key.into(), Value::String(format!("{current}{text}")));
}

// ---------------------------------------------------------------------------
// Connect: re-stream a thread's latest run
// ---------------------------------------------------------------------------

/// Index of the thread's latest `INVOCATION_STARTED`, whatever its reason
/// (user message or approval resume): the run a connect re-streams. `None`
/// when no turn has started (a session with only `SESSION_CREATED`).
pub fn latest_invocation(events: &[Event]) -> Option<usize> {
  events
    .iter()
    .rposition(|e| matches!(e.payload, EventPayload::InvocationStarted(_)))
}

/// The `runId` of the run behind an `INVOCATION_STARTED`: the AG-UI
/// client's own id (`ag_ui.run_id`), else the invocation id (a turn started
/// outside AG-UI, e.g. through the sessions API). `None` for any other event.
pub fn invocation_run_id(event: &Event) -> Option<String> {
  let EventPayload::InvocationStarted(inv) = &event.payload else {
    return None;
  };
  let client = invocation_namespace(&inv.extra)
    .and_then(|ns| ns.get(RUN_ID_KEY))
    .and_then(Value::as_str);
  Some(client.map_or_else(|| inv.invocation_id.to_string(), str::to_owned))
}

/// What a connect replays (`schema/agui-mapping.md` "Connect"): the latest
/// invocation's index and the projection context for it. Same run id as the
/// original run, no `input` (the original request is not journaled), and
/// always a snapshot (a reloaded client holds nothing). `None`: no turn yet,
/// so the connect stream is empty.
pub fn connect_context(thread_id: &str, events: &[Event]) -> Option<(usize, RunContext)> {
  let idx = latest_invocation(events)?;
  let run_id = invocation_run_id(&events[idx])?;
  Some((
    idx,
    RunContext {
      thread_id: thread_id.to_owned(),
      run_id,
      input: None,
      snapshot: Some(fold_messages(&events[..=idx])),
    },
  ))
}

#[cfg(test)]
mod tests;
