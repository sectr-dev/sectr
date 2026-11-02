//! The turn-stream validator: consumes decoded [`StreamItem`]s from the
//! runner's invoke response and enforces the turn contract's finality rules
//! — the consuming mirror of the SDK's `sectr.drive` driver (which produces
//! streams under the same rules; see sdk-python/src/sectr/drive.py).
//!
//! The rules:
//!
//! - `NO_MORE_ACTIONS` ends the turn. `Ended { errored: false }`.
//! - An `ERROR` event ends the turn: the engine journals the ERROR, then
//!   synthesizes the sentinel (SDK runners emit their own sentinel after
//!   ERROR; the validator has already concluded by then, so the journal sees
//!   exactly one).
//! - `TOOL_APPROVAL_REQUIRED` suspends the turn: it must be the FINAL frame
//!   (no sentinel — the gated tool is a pending action). Anything after it
//!   is a contract violation, as is a second approval in one turn.
//! - A stream that ends with an approval pending is a clean suspension.
//! - A stream that ends without a sentinel and without a pending approval
//!   violated the contract (raw runners MUST end with `NO_MORE_ACTIONS`).
//! - Any invalid frame (bad JSON, non-runner type, schema-violating
//!   payload) fails the turn — see the policy in the module doc of
//!   [`super`].
//! - Transport failure (connection reset, HTTP error, timeout) fails the
//!   turn with the same synthesized `SESSION_ERROR` + sentinel.
//! - A `MESSAGE_END`'s `framework_state` over [`FRAMEWORK_STATE_LIMIT`] is
//!   dropped from it (with a warning); the event is journaled without it.
//!   The SDK applies the same limit before sending, so only a runner that
//!   doesn't use it gets here. The adapter then rebuilds that message
//!   without it, as a framework that never kept it would.
//!
//! Pure state machine — no I/O. The caller (turn lifecycle) journals the
//! directed events via its `dyn EventSink` and stops feeding on
//! [`Directive::Conclude`].

use sectr_common::events::{
  EventPayload, FRAMEWORK_STATE_LIMIT, NewEvent, NoMoreActions, RunnerEvent, SessionError,
};

/// One item from the runner's turn stream (post-SSE-decode).
#[derive(Debug)]
pub enum StreamItem {
  /// One `data:` payload, deserialized and schema-validated against
  /// `$defs/runnerEvent` (`FrameError` = unparseable or contract-invalid).
  Frame(Result<RunnerEvent, FrameError>),
  /// The runner closed the stream cleanly.
  Ended,
  /// The transport broke before finality (HTTP error, connection reset,
  /// read timeout). `detail` is human-readable.
  Failed(Failure),
}

/// A rejected frame (see [`StreamItem::Frame`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameError(pub String);

/// Why a turn failed. `SESSION_ERROR`'s journal payload is a `message`
/// derived from this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
  Transport {
    detail: String,
  },
  ContractViolation {
    detail: String,
  },
  /// The runner didn't start; `message` says why, ending with its stderr
  /// (`startup_failure_message`).
  Startup {
    message: String,
  },
}

impl Failure {
  /// The `SESSION_ERROR` message journaled for this failure.
  pub fn message(&self) -> String {
    match self {
      Failure::Transport { detail } => format!("runner stream failed: {detail}"),
      Failure::ContractViolation { detail } => {
        format!("runner violated the stream contract: {detail}")
      }
      Failure::Startup { message } => message.clone(),
    }
  }
}

/// How a turn concluded. The turn-lifecycle state machine (next milestone)
/// maps these to lease release + process teardown + server completion
/// reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnOutcome {
  /// `NO_MORE_ACTIONS` was journaled — the turn is over. `errored` is true
  /// when it followed a runner `ERROR` (sentinel synthesized by us).
  Ended { errored: bool },
  /// `TOOL_APPROVAL_REQUIRED` is the final journaled event; the turn is
  /// suspended awaiting an approval decision (resume = fresh invocation).
  Suspended,
  /// The turn failed; a synthesized `SESSION_ERROR` + `NO_MORE_ACTIONS`
  /// must be journaled.
  Failed(Failure),
}

/// What the validator tells the caller to do after each item.
#[derive(Debug, PartialEq)]
pub enum Directive {
  /// Append this runner event to the journal (id minted here — platform
  /// code; the journal writer stamps seq/created_at).
  Append(NewEvent),
  /// The turn has concluded: journal the given events in order (empty for
  /// a clean suspension or the runner's own sentinel), record the
  /// outcome, and STOP READING THE STREAM. Feeding more items afterwards
  /// is a caller bug ([`FeedError::Concluded`]).
  Conclude(TurnOutcome, Vec<NewEvent>),
}

/// Feeding a [`StreamValidator`] after it concluded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("validator already concluded the turn; the stream must not be read further")]
pub struct FeedError(pub TurnOutcome);

pub struct StreamValidator {
  /// A `TOOL_APPROVAL_REQUIRED` is the last accepted frame — the turn is
  /// suspended unless something (invalidly) follows.
  awaiting_approval: bool,
  /// Set once the turn concluded; further items are a caller bug.
  concluded: Option<TurnOutcome>,
}

impl Default for StreamValidator {
  fn default() -> Self {
    Self::new()
  }
}

impl StreamValidator {
  pub fn new() -> Self {
    StreamValidator {
      awaiting_approval: false,
      concluded: None,
    }
  }

  /// Feed the next stream item.
  pub fn feed(&mut self, item: StreamItem) -> Result<Directive, FeedError> {
    if let Some(outcome) = &self.concluded {
      return Err(FeedError(outcome.clone()));
    }
    let directive = match item {
      StreamItem::Frame(frame) => self.accept(frame),
      StreamItem::Ended => {
        if self.awaiting_approval {
          // The stream just ends on a suspension: the turn stays
          // suspended until the decision's invocation.
          Directive::Conclude(TurnOutcome::Suspended, vec![])
        } else {
          // Raw runners MUST end with NO_MORE_ACTIONS; an SDK
          // runner's driver always emits it. Ending without it
          // means the runner never reached finality.
          failed(Failure::ContractViolation {
            detail: "stream ended without NO_MORE_ACTIONS or an approval pause".into(),
          })
        }
      }
      StreamItem::Failed(failure) => failed(failure),
    };
    if let Directive::Conclude(outcome, _) = &directive {
      self.concluded = Some(outcome.clone());
    }
    Ok(directive)
  }

  /// Accept one deserialized frame and decide what it means.
  fn accept(&mut self, frame: Result<RunnerEvent, FrameError>) -> Directive {
    let event = match frame {
      Ok(event) => within_limits(event),
      Err(FrameError(detail)) => {
        return failed(Failure::ContractViolation { detail });
      }
    };

    use sectr_common::events::RunnerEventType as T;
    // Exhaustive on purpose: a new RunnerEventType variant must be
    // explicitly classified here (journalable? finality-affecting?) or
    // the engine does not compile — a silent `_` arm would append it.
    match event.event_type {
      T::NoMoreActions => {
        // The runner's own sentinel — journal as-is.
        Directive::Conclude(
          TurnOutcome::Ended { errored: false },
          vec![new_event(event)],
        )
      }
      T::Error => {
        // ERROR is terminal (MVP): journal it, synthesize the
        // sentinel, end the turn. (An SDK runner emits its own
        // sentinel right after; it is never read.)
        let error = new_event(event);
        Directive::Conclude(
          TurnOutcome::Ended { errored: true },
          vec![error, sentinel()],
        )
      }
      T::ToolApprovalRequired => {
        if self.awaiting_approval {
          return failed(Failure::ContractViolation {
            detail: "second TOOL_APPROVAL_REQUIRED in one turn".into(),
          });
        }
        self.awaiting_approval = true;
        Directive::Append(new_event(event))
      }
      // Stream-content types: journalable when the turn is live; any
      // of them after TOOL_APPROVAL_REQUIRED violates finality (the
      // turn is suspended there — nothing may follow).
      T::MessageStart
      | T::MessageDelta
      | T::MessageEnd
      | T::ToolCall
      | T::ToolResult
      | T::TranscriptCompacted => {
        if self.awaiting_approval {
          return failed(Failure::ContractViolation {
            detail: format!(
              "frame after TOOL_APPROVAL_REQUIRED (the turn is suspended there): {}",
              event.event_type
            ),
          });
        }
        Directive::Append(new_event(event))
      }
    }
  }
}

/// `event`, without a `MESSAGE_END.framework_state` over
/// [`FRAMEWORK_STATE_LIMIT`].
fn within_limits(mut event: RunnerEvent) -> RunnerEvent {
  if let EventPayload::MessageEnd(end) = &mut event.payload
    && let Some(state) = &end.framework_state
    && state.serialized_len() > FRAMEWORK_STATE_LIMIT
  {
    tracing::warn!(
      message_id = %end.message_id,
      adapter = %state.adapter,
      bytes = state.serialized_len(),
      limit = FRAMEWORK_STATE_LIMIT,
      "MESSAGE_END framework_state over the limit: dropped"
    );
    end.framework_state = None;
  }
  event
}

fn new_event(event: RunnerEvent) -> NewEvent {
  NewEvent {
    id: uuid::Uuid::now_v7(),
    event_type: event.event_type.widen(),
    payload: event.payload,
    extra: event.extra,
  }
}

/// The synthesized events that realize a failed turn: `SESSION_ERROR`
/// (sidecar-attributed platform failure) + the sentinel. Journaled in this
/// order; nothing from the runner after the failure point.
pub fn failed(failure: Failure) -> Directive {
  Directive::Conclude(
    TurnOutcome::Failed(failure.clone()),
    vec![
      NewEvent::new(EventPayload::SessionError(SessionError {
        message: failure.message(),
        extra: Default::default(),
      })),
      sentinel(),
    ],
  )
}

fn sentinel() -> NewEvent {
  NewEvent::new(EventPayload::NoMoreActions(NoMoreActions::default()))
}

#[cfg(test)]
mod tests {
  use super::*;
  use sectr_common::events::{
    EventPayload, EventType, FrameworkState, MessageDelta, MessageEnd, MessageStart, RunnerEvent,
    ToolApprovalRequired, TypedDelta,
  };

  fn fed(v: &mut StreamValidator, event: EventPayload) -> Directive {
    let json = RunnerEvent::new(event).unwrap().to_json_string();
    let frame: RunnerEvent = serde_json::from_str(&json).unwrap();
    v.feed(StreamItem::Frame(Ok(frame))).unwrap()
  }

  fn start() -> EventPayload {
    EventPayload::MessageStart(MessageStart {
      role: "assistant".into(),
      message_id: "m1".into(),
      extra: Default::default(),
    })
  }

  fn delta(text: &str) -> EventPayload {
    EventPayload::MessageDelta(MessageDelta {
      message_id: "m1".into(),
      delta: TypedDelta::Text {
        text: text.into(),
        extra: Default::default(),
      },
      extra: Default::default(),
    })
  }

  fn approval() -> EventPayload {
    EventPayload::ToolApprovalRequired(ToolApprovalRequired {
      tool_call_id: "call_1".into(),
      name: "issue_refund".into(),
      arguments: serde_json::json!({}),
      reason: None,
      extra: Default::default(),
    })
  }

  fn conclude(d: Directive) -> (TurnOutcome, Vec<NewEvent>) {
    match d {
      Directive::Conclude(outcome, events) => (outcome, events),
      other => panic!("expected conclude, got {other:?}"),
    }
  }

  fn expect_failed(d: Directive) -> (Failure, Vec<NewEvent>) {
    match conclude(d) {
      (TurnOutcome::Failed(f), events) => (f, events),
      other => panic!("expected failure, got {other:?}"),
    }
  }

  #[test]
  fn happy_path_ends_on_sentinel() {
    let mut v = StreamValidator::new();
    assert!(matches!(fed(&mut v, start()), Directive::Append(_)));
    assert!(matches!(fed(&mut v, delta("hi")), Directive::Append(_)));
    let (outcome, events) = conclude(fed(
      &mut v,
      EventPayload::NoMoreActions(NoMoreActions::default()),
    ));
    assert_eq!(outcome, TurnOutcome::Ended { errored: false });
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_type, EventType::NoMoreActions);
    // The runner's own sentinel is journaled as-is (payload preserved).
    assert_eq!(
      events[0].payload,
      EventPayload::NoMoreActions(NoMoreActions::default())
    );
    // Producer-minted UUIDv7 ids.
    assert_eq!(events[0].id.get_version_num(), 7);
  }

  #[test]
  fn approval_suspends_without_sentinel() {
    let mut v = StreamValidator::new();
    assert!(matches!(fed(&mut v, approval()), Directive::Append(_)));
    let d = v.feed(StreamItem::Ended).unwrap();
    assert_eq!(d, Directive::Conclude(TurnOutcome::Suspended, vec![]));
  }

  #[test]
  fn frame_after_approval_is_a_violation() {
    let mut v = StreamValidator::new();
    assert!(matches!(fed(&mut v, approval()), Directive::Append(_)));
    let (failure, events) = expect_failed(fed(&mut v, delta("late")));
    assert!(matches!(failure, Failure::ContractViolation { .. }));
    // The violation journals SESSION_ERROR + sentinel, NOT the late frame.
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].event_type, EventType::SessionError);
    assert_eq!(events[1].event_type, EventType::NoMoreActions);
  }

  #[test]
  fn second_approval_is_a_violation() {
    let mut v = StreamValidator::new();
    assert!(matches!(fed(&mut v, approval()), Directive::Append(_)));
    let (failure, _) = expect_failed(fed(&mut v, approval()));
    assert!(matches!(failure, Failure::ContractViolation { .. }));
  }

  #[test]
  fn error_event_gets_synthesized_sentinel() {
    let mut v = StreamValidator::new();
    let d = fed(
      &mut v,
      EventPayload::Error(sectr_common::events::Error {
        message: "boom".into(),
        code: None,
        transient: None,
        extra: Default::default(),
      }),
    );
    let (outcome, events) = conclude(d);
    assert_eq!(outcome, TurnOutcome::Ended { errored: true });
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].event_type, EventType::Error);
    assert_eq!(events[1].event_type, EventType::NoMoreActions);
    assert_eq!(events[0].event_type, EventType::Error);
    assert_eq!(events[1].event_type, EventType::NoMoreActions);
  }

  #[test]
  fn invalid_frame_fails_the_turn() {
    let mut v = StreamValidator::new();
    let (failure, events) = expect_failed(
      v.feed(StreamItem::Frame(Err(FrameError("not json".into()))))
        .unwrap(),
    );
    assert!(matches!(failure, Failure::ContractViolation { .. }));
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].event_type, EventType::SessionError);
  }

  fn end_with_state(bytes: usize) -> EventPayload {
    let state = serde_json::json!({"adapter": "x", "v": 1, "data": {"blob": "a".repeat(bytes)}});
    EventPayload::MessageEnd(MessageEnd {
      message_id: "m1".into(),
      finish_reason: None,
      framework_state: Some(serde_json::from_value(state).unwrap()),
      extra: Default::default(),
    })
  }

  fn journaled_state(d: Directive) -> Option<FrameworkState> {
    match d {
      Directive::Append(NewEvent {
        payload: EventPayload::MessageEnd(end),
        ..
      }) => end.framework_state,
      other => panic!("expected an appended MESSAGE_END, got {other:?}"),
    }
  }

  #[test]
  fn framework_state_within_the_limit_is_journaled() {
    let mut v = StreamValidator::new();
    fed(&mut v, start());
    let state = journaled_state(fed(&mut v, end_with_state(1024))).unwrap();
    assert_eq!((state.adapter.as_str(), state.v), ("x", 1));
    assert_eq!(state.data["blob"].as_str().unwrap().len(), 1024);
  }

  #[test]
  fn framework_state_over_the_limit_is_dropped_but_the_end_journaled() {
    let mut v = StreamValidator::new();
    fed(&mut v, start());
    let d = fed(&mut v, end_with_state(FRAMEWORK_STATE_LIMIT));
    assert_eq!(journaled_state(d), None);
  }

  #[test]
  fn untagged_framework_state_is_rejected_at_decode() {
    let raw = r#"{"type":"MESSAGE_END","payload":{"message_id":"m1","framework_state":{"data":{"blob":1}}}}"#;
    let frame: Result<RunnerEvent, _> = serde_json::from_str(raw);
    assert!(frame.is_err());
  }

  #[test]
  fn non_runner_event_type_is_rejected_at_decode() {
    // SESSION_CREATED is platform-emitted — $defs/runnerEvent rejects it,
    // so it arrives as a FrameError, not a frame.
    let raw = r#"{"type":"SESSION_CREATED","payload":{}}"#;
    let frame: Result<RunnerEvent, _> = serde_json::from_str(raw);
    assert!(frame.is_err());
  }

  #[test]
  fn schema_violating_payload_is_rejected_at_decode() {
    // Bare-string delta shorthand was removed (closed TypedDelta
    // vocabulary) — a frame with a string payload must not decode.
    let raw = r#"{"type":"MESSAGE_DELTA","payload":"hi"}"#;
    let frame: Result<RunnerEvent, _> = serde_json::from_str(raw);
    assert!(frame.is_err());
  }

  #[test]
  fn end_without_sentinel_or_suspension_is_a_violation() {
    let mut v = StreamValidator::new();
    let (failure, events) = expect_failed(v.feed(StreamItem::Ended).unwrap());
    match failure {
      Failure::ContractViolation { detail } => {
        assert!(detail.contains("without NO_MORE_ACTIONS"));
      }
      other => panic!("expected contract violation, got {other:?}"),
    }
    assert_eq!(events.len(), 2);
  }

  #[test]
  fn transport_failure_fails_the_turn() {
    let mut v = StreamValidator::new();
    let d = v
      .feed(StreamItem::Failed(Failure::Transport {
        detail: "connection reset".into(),
      }))
      .unwrap();
    let (failure, events) = expect_failed(d);
    assert_eq!(
      failure,
      Failure::Transport {
        detail: "connection reset".into()
      }
    );
    assert_eq!(events[0].event_type, EventType::SessionError);
    let EventPayload::SessionError(se) = &events[0].payload else {
      unreachable!("first synthesized event is SESSION_ERROR")
    };
    assert_eq!(se.message, "runner stream failed: connection reset");
  }

  #[test]
  fn feeding_after_conclude_is_an_error() {
    let mut v = StreamValidator::new();
    let d = fed(
      &mut v,
      EventPayload::NoMoreActions(NoMoreActions::default()),
    );
    assert!(matches!(d, Directive::Conclude(_, _)));
    let err = v.feed(StreamItem::Ended).unwrap_err();
    assert_eq!(err.0, TurnOutcome::Ended { errored: false });
  }

  /// Golden fixtures: every runner-event fixture decodes, and a scripted
  /// sequence journals the same event types in order with minted v7 ids.
  #[test]
  fn golden_fixture_sequence() {
    let dir = concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/../../schema/fixtures/valid/runner-event"
    );
    let names = [
      "MESSAGE_START",
      "MESSAGE_DELTA",
      "MESSAGE_END",
      "TOOL_CALL",
      "TOOL_RESULT",
      "TRANSCRIPT_COMPACTED",
      "NO_MORE_ACTIONS",
    ];
    let mut v = StreamValidator::new();
    let mut journaled = Vec::new();
    for name in names {
      let raw = std::fs::read_to_string(format!("{dir}/{name}.json")).unwrap();
      let event: RunnerEvent = serde_json::from_str(&raw).unwrap();
      match v.feed(StreamItem::Frame(Ok(event))).unwrap() {
        Directive::Append(e) => journaled.push(e),
        Directive::Conclude(outcome, events) => {
          assert_eq!(
            outcome,
            TurnOutcome::Ended { errored: false },
            "unexpected conclusion at {name}"
          );
          journaled.extend(events);
          break;
        }
      }
    }
    let expected = [
      EventType::MessageStart,
      EventType::MessageDelta,
      EventType::MessageEnd,
      EventType::ToolCall,
      EventType::ToolResult,
      EventType::TranscriptCompacted,
      EventType::NoMoreActions,
    ];
    assert_eq!(journaled.len(), expected.len());
    for (e, t) in journaled.iter().zip(expected) {
      assert_eq!(e.event_type, t);
      assert_eq!(e.id.get_version_num(), 7, "producer-minted v7 id");
    }
  }
}
