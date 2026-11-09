//! The journal traits — the only access path to the event log, split into a
//! write half ([`EventSink`], consumed by the engine in both dev and prod)
//! and a read half ([`JournalReader`], SSE tails + transcript fetches,
//! store-side only). Physical stores ([`Journal`], Postgres + SQLite) are
//! independent of the relational model (`Registry`) so the two can sit on
//! different stacks in future cloud iterations; the sidecar's remote sink is
//! a WebSocket client (Contract 4), not a store.

mod pg;
mod sqlite;

pub use pg::PgJournal;
pub use sqlite::SqliteJournal;

/// NOTIFY channel the Postgres journal posts appends to (payload = the
/// session uuid, so tails filter to their own session). The sqlite journal
/// polls instead; this channel is PgJournal's wake mechanism.
pub const PG_NOTIFY_CHANNEL: &str = "sectr_events";

use crate::events::{Event, EventPayload, NewEvent};
use chrono::{DateTime, Utc};
use std::pin::Pin;
use tokio_stream::Stream;
use uuid::Uuid;

/// A session id — the journal is per-session scoped (seq, tail).
pub type SessionId = Uuid;

/// A deployment id — callers (the sidecar) know the session's pinned
/// deployment and stamp it on append; the journal never looks it up
/// relationally.
pub type DeploymentId = Uuid;

/// Live tail of a session's journal: items in `seq` order, never completes
/// on its own (ends when dropped, on error, or when the underlying
/// implementation's connection fails).
pub type EventStream = Pin<Box<dyn Stream<Item = Result<Event>> + Send>>;

/// The approval a session is suspended on: its last event, when that's a
/// `TOOL_APPROVAL_REQUIRED` (a suspended turn ends there). The one rule for
/// accepting an approval decision, in dev and prod, over the sessions API
/// and AG-UI.
///
/// A decision journals `TOOL_APPROVAL_GRANTED`/`DENIED` and the resume's
/// `INVOCATION_STARTED` after it, so the call is decided exactly once: a
/// second decision gets a 409, even when the resume crashed before the
/// tool's `TOOL_RESULT` (the tool may have run). The next user turn's fold
/// answers such a call with its placeholder.
pub fn awaiting_approval(events: &[crate::events::Event]) -> Option<&crate::events::Event> {
  events.last().filter(|e| {
    matches!(
      e.payload,
      crate::events::EventPayload::ToolApprovalRequired(_)
    )
  })
}

/// The unanswered tool call in a transcript: the LAST `TOOL_CALL` /
/// `TOOL_APPROVAL_REQUIRED` envelope with no subsequent `TOOL_RESULT` for
/// the same `tool_call_id`. A later `user_message` invocation abandons the
/// call: the SDK's canonical fold answers it as interrupted. Mirrors the
/// SDK's `SessionContext.pending_tool_call`, the runner's view: on an
/// approval resume, the call is still unanswered, and the runner executes
/// it. Whether a decision may be accepted is [`awaiting_approval`], not
/// this: a call stays unanswered here after its resume crashed.
pub fn pending_tool_call(events: &[crate::events::Event]) -> Option<&crate::events::Event> {
  use crate::events::{EventPayload, InvocationReason};
  let mut pending: Option<&crate::events::Event> = None;
  for event in events {
    match &event.payload {
      EventPayload::ToolCall(_) | EventPayload::ToolApprovalRequired(_) => {
        pending = Some(event);
      }
      EventPayload::ToolResult(result) => {
        let answers_pending = match pending.map(|p| &p.payload) {
          Some(EventPayload::ToolCall(c)) => c.tool_call_id == result.tool_call_id,
          Some(EventPayload::ToolApprovalRequired(a)) => a.tool_call_id == result.tool_call_id,
          _ => false,
        };
        if answers_pending {
          pending = None;
        }
      }
      EventPayload::InvocationStarted(started)
        if matches!(started.reason, InvocationReason::UserMessage { .. }) =>
      {
        pending = None;
      }
      _ => {}
    }
  }
  pending
}

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
  /// Seq-race, FK violation (unknown session), or other append conflict.
  #[error("journal conflict: {0}")]
  Conflict(String),

  /// A row that violates the event contract (corrupt journal).
  #[error("corrupt journal row: {0}")]
  Corrupt(String),

  #[error("serialization failure: {0}")]
  Serde(#[from] serde_json::Error),

  #[error(transparent)]
  Sql(#[from] sqlx::Error),
}

pub type Result<T> = std::result::Result<T, JournalError>;

#[async_trait::async_trait]
pub trait EventSink: Send + Sync {
  /// Append events to a session's journal. `NewEvent.id` is minted by the
  /// producer (UUIDv7 — platform code, never user code) and is the dedup
  /// key: replaying an already-journaled event returns its originally
  /// stamped row instead of a duplicate (exactly-once reconnect replay).
  /// The journal writer assigns `seq` (per-session, prev + 1, gap-free —
  /// enforced by the UNIQUE(session_id, seq) constraint) and `created_at`.
  /// Returns the stamped events in append order.
  async fn append(
    &self,
    session: SessionId,
    deployment_id: DeploymentId,
    events: Vec<NewEvent>,
  ) -> Result<Vec<Event>>;

  /// Append a single platform-emitted event (INVOCATION_STARTED,
  /// TOOL_APPROVAL_GRANTED/DENIED, SESSION_ERROR, ...). Convenience over
  /// [`EventSink::append`] for one event.
  async fn append_platform_event(
    &self,
    session: SessionId,
    deployment_id: DeploymentId,
    payload: EventPayload,
  ) -> Result<Event> {
    let mut stamped = self
      .append(session, deployment_id, vec![NewEvent::new(payload)])
      .await?;
    stamped
      .pop()
      .ok_or_else(|| JournalError::Conflict("append returned no events".into()))
  }
}

/// Record the stamped write's seq range onto the `journal.append` span
///. Shared by both store impls' `append` spans.
pub(crate) fn record_stamped(span: &tracing::Span, stamped: &[Event]) {
  if let Some(first) = stamped.first() {
    span.record("first_seq", first.seq);
    if let Some(last) = stamped.last() {
      span.record("last_seq", last.seq);
    }
  }
}

/// The read half — SSE tails and transcript fetches. Meaningful only for
/// physical stores: the remote event sink (sidecar) has no read path by
/// design (the assignment carries the transcript; clients tail via the
/// public API).
#[async_trait::async_trait]
pub trait JournalReader: Send + Sync {
  /// Fetch a session's events with `seq > after_seq` (from the beginning —
  /// seq 0 — if `None`), ordered by `seq`.
  async fn fetch(&self, session: SessionId, after_seq: Option<i64>) -> Result<Vec<Event>>;

  /// Live tail: all events with `seq > after_seq` (from the beginning if
  /// `None`), then newly appended ones as they land. The SQLite
  /// implementation polls; Postgres would LISTEN/NOTIFY. The stream never
  /// completes on its own — consumers terminate it by dropping.
  ///
  /// This is the SSE primitive: `Last-Event-ID` reconnect is
  /// `subscribe(session, Some(last_seq))`.
  async fn subscribe(&self, session: SessionId, after_seq: Option<i64>) -> Result<EventStream>;

  /// A deployment's recent turns, for its wakes summary: its `limit` newest
  /// `INVOCATION_STARTED` events, and every `INVOCATION_STARTED` and
  /// `SESSION_ERROR` of that deployment since the oldest of them, newest
  /// first. Empty when the deployment has no turns.
  async fn deployment_turn_starts(
    &self,
    deployment: DeploymentId,
    limit: i64,
  ) -> Result<Vec<TurnRow>>;
}

/// One row of [`JournalReader::deployment_turn_starts`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnRow {
  pub session_id: SessionId,
  pub seq: i64,
  /// `None` for an `INVOCATION_STARTED`; the message of a `SESSION_ERROR`.
  pub error: Option<String>,
  pub created_at: DateTime<Utc>,
}

/// The rows' shape in both stores: session, seq, type, payload, created_at.
pub(crate) type TurnRowParts = (Uuid, i64, String, serde_json::Value, DateTime<Utc>);

pub(crate) fn turn_row(
  (session_id, seq, event_type, payload, created_at): TurnRowParts,
) -> TurnRow {
  let error = (event_type == "SESSION_ERROR").then(|| {
    payload
      .get("message")
      .and_then(|m| m.as_str())
      .unwrap_or_default()
      .to_owned()
  });
  TurnRow {
    session_id,
    seq,
    error,
    created_at,
  }
}

/// A physical journal store: both halves. Implemented by `PgJournal` (prod)
/// and `SqliteJournal` (dev). The engine consumes only `dyn EventSink`; the
/// remote sink is the sidecar's WebSocket client (Contract 4).
pub trait Journal: EventSink + JournalReader {}

impl<T: ?Sized + EventSink + JournalReader> Journal for T {}

/// Shared timestamp helper — one `now()` per append batch keeps a batch
/// monotonically consistent. Truncated to microseconds: Postgres
/// `timestamptz` stores microseconds, so a nanosecond-precision stamp would
/// not survive a write/read round trip (stamped rows must equal fetched
/// rows).
pub(crate) fn now() -> DateTime<Utc> {
  use chrono::SubsecRound;
  Utc::now().trunc_subsecs(6)
}

/// What both stores' `deployment_turn_starts` must return for the same
/// journal: `a` and `b` are two sessions the store has rows for.
#[cfg(test)]
pub(crate) async fn turn_starts_contract(journal: &dyn Journal, a: SessionId, b: SessionId) {
  use crate::events::{Extra, InvocationReason, InvocationStarted, SessionError};
  let deployment = Uuid::now_v7();
  let other = Uuid::now_v7();
  let mut stamped = Vec::new();
  let mut append = async |session: SessionId, deployment_id: DeploymentId, error: Option<&str>| {
    let payload = match error {
      None => EventPayload::InvocationStarted(InvocationStarted {
        invocation_id: Uuid::now_v7(),
        route: "/test".into(),
        deployment_id,
        reason: InvocationReason::UserMessage {
          input: "hi".to_string().try_into().unwrap(),
          extra: Extra::default(),
        },
        extra: Extra::default(),
      }),
      Some(message) => EventPayload::SessionError(SessionError {
        message: message.into(),
        extra: Extra::default(),
      }),
    };
    let event = journal
      .append_platform_event(session, deployment_id, payload)
      .await
      .unwrap();
    // Distinct timestamps, so the order below is the append order.
    tokio::time::sleep(std::time::Duration::from_millis(3)).await;
    stamped.push(TurnRow {
      session_id: session,
      seq: event.seq,
      error: error.map(str::to_owned),
      created_at: event.created_at,
    });
  };
  append(a, deployment, None).await; // 0: a's first turn
  append(a, deployment, Some("boom")).await; // 1
  append(b, deployment, None).await; // 2: b's turn
  append(b, deployment, Some("endpoint wake failed: x")).await; // 3
  append(a, other, None).await; // 4: another deployment
  append(a, deployment, None).await; // 5: a's second turn

  let rows = |idx: &[usize]| idx.iter().map(|&i| stamped[i].clone()).collect::<Vec<_>>();
  assert_eq!(
    journal.deployment_turn_starts(deployment, 2).await.unwrap(),
    rows(&[5, 3, 2])
  );
  assert_eq!(
    journal
      .deployment_turn_starts(deployment, 10)
      .await
      .unwrap(),
    rows(&[5, 3, 2, 1, 0])
  );
  assert_eq!(
    journal.deployment_turn_starts(other, 10).await.unwrap(),
    rows(&[4])
  );
  assert!(
    journal
      .deployment_turn_starts(Uuid::now_v7(), 10)
      .await
      .unwrap()
      .is_empty()
  );
}

#[cfg(test)]
mod tests {
  use super::{awaiting_approval, pending_tool_call};
  use crate::events::{
    ApprovalDecision, Event, EventPayload, InvocationReason, InvocationStarted, NoMoreActions,
    SessionError, ToolApprovalDecision, ToolApprovalRequired, ToolCall, ToolResult,
  };
  use serde_json::json;
  use uuid::Uuid;

  fn event(seq: i64, payload: EventPayload) -> Event {
    Event::new(
      Uuid::now_v7(),
      Uuid::nil(),
      Uuid::nil(),
      seq,
      chrono::Utc::now(),
      payload,
    )
  }

  fn call(seq: i64, id: &str) -> Event {
    event(
      seq,
      EventPayload::ToolCall(ToolCall {
        tool_call_id: id.into(),
        name: "refund".into(),
        arguments: json!({}),
        extra: Default::default(),
      }),
    )
  }

  fn result(seq: i64, id: &str) -> Event {
    event(
      seq,
      EventPayload::ToolResult(ToolResult {
        tool_call_id: id.into(),
        result: json!("ok"),
        extra: Default::default(),
      }),
    )
  }

  fn started(seq: i64, reason: InvocationReason) -> Event {
    event(
      seq,
      EventPayload::InvocationStarted(InvocationStarted {
        invocation_id: Uuid::now_v7(),
        route: "/chat".into(),
        deployment_id: Uuid::nil(),
        reason,
        extra: Default::default(),
      }),
    )
  }

  fn user_message(seq: i64) -> Event {
    started(
      seq,
      InvocationReason::UserMessage {
        input: "hi".to_string().try_into().unwrap(),
        extra: Default::default(),
      },
    )
  }

  fn tool_call_id(event: &Event) -> &str {
    match &event.payload {
      EventPayload::ToolCall(c) => &c.tool_call_id,
      EventPayload::ToolApprovalRequired(a) => &a.tool_call_id,
      other => panic!("not a tool call: {other:?}"),
    }
  }

  #[test]
  fn an_unanswered_call_is_pending() {
    let events = [user_message(0), call(1, "c1")];
    assert_eq!(pending_tool_call(&events).map(tool_call_id), Some("c1"));
  }

  #[test]
  fn an_answered_call_is_not_pending() {
    let events = [user_message(0), call(1, "c1"), result(2, "c1")];
    assert!(pending_tool_call(&events).is_none());
  }

  #[test]
  fn a_later_user_message_abandons_an_unanswered_call() {
    // e.g. the turn crashed after journaling the call, then the user wrote
    // again: the fold answers c1 as interrupted, so no decision may resume it
    let events = [user_message(0), call(1, "c1"), user_message(2)];
    assert!(pending_tool_call(&events).is_none());
  }

  #[test]
  fn an_approval_decision_invocation_keeps_the_call_pending() {
    let decision = InvocationReason::ApprovalDecision {
      decision: ApprovalDecision {
        approved: true,
        approver: None,
        extra: Default::default(),
      },
      extra: Default::default(),
    };
    let events = [user_message(0), call(1, "c1"), started(2, decision)];
    assert_eq!(pending_tool_call(&events).map(tool_call_id), Some("c1"));
  }

  fn approval_required(seq: i64, id: &str) -> Event {
    event(
      seq,
      EventPayload::ToolApprovalRequired(ToolApprovalRequired {
        tool_call_id: id.into(),
        name: "refund".into(),
        arguments: json!({}),
        reason: None,
        extra: Default::default(),
      }),
    )
  }

  fn granted(seq: i64, id: &str) -> Event {
    event(
      seq,
      EventPayload::ToolApprovalGranted(ToolApprovalDecision {
        tool_call_id: id.into(),
        approver: None,
        extra: Default::default(),
      }),
    )
  }

  fn approval_decision(seq: i64) -> Event {
    started(
      seq,
      InvocationReason::ApprovalDecision {
        decision: ApprovalDecision {
          approved: true,
          approver: None,
          extra: Default::default(),
        },
        extra: Default::default(),
      },
    )
  }

  fn turn_end(seq: i64) -> Vec<Event> {
    vec![
      event(
        seq,
        EventPayload::SessionError(SessionError {
          message: "runner exited".into(),
          extra: Default::default(),
        }),
      ),
      event(
        seq + 1,
        EventPayload::NoMoreActions(NoMoreActions {
          extra: Default::default(),
        }),
      ),
    ]
  }

  #[test]
  fn a_suspended_turn_awaits_its_approval() {
    let events = [user_message(0), call(1, "c1"), approval_required(2, "c1")];
    assert_eq!(awaiting_approval(&events).map(|e| e.seq), Some(2));
  }

  #[test]
  fn a_decided_approval_is_not_awaited() {
    // the resume is running: a second decision must not start another
    let events = [
      user_message(0),
      call(1, "c1"),
      approval_required(2, "c1"),
      granted(3, "c1"),
      approval_decision(4),
    ];
    assert!(awaiting_approval(&events).is_none());
  }

  #[test]
  fn a_crashed_resume_does_not_reopen_the_approval() {
    // the runner died inside the approved tool: no TOOL_RESULT, so the
    // runner's view still has the call unanswered, but a second decision
    // would run the tool again
    let mut events = vec![
      user_message(0),
      call(1, "c1"),
      approval_required(2, "c1"),
      granted(3, "c1"),
      approval_decision(4),
    ];
    events.extend(turn_end(5));
    assert!(awaiting_approval(&events).is_none());
    assert_eq!(pending_tool_call(&events).map(tool_call_id), Some("c1"));
  }

  #[test]
  fn an_ungated_call_left_by_a_crash_is_not_awaited() {
    // the turn crashed with a call journaled that never asked for approval
    let mut events = vec![user_message(0), call(1, "c1")];
    events.extend(turn_end(2));
    assert!(awaiting_approval(&events).is_none());
  }
}
