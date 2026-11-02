//! Patterns over journal events, matched in order against what the adapter
//! writes. Matching is exact on event order and loose where adapters
//! legitimately differ: text deltas are compared concatenated per message
//! (an empty delta is a warning, [`crate::warning`]), and message ids are
//! checked for consistency, not value. Tool results are
//! journaled in completion order, so a case lists them in the order its
//! [`Case::tools`](crate::case::Case::tools) answers them (see the crate
//! docs, "Ordering and strictness").
//!
//! Each model response is one assistant bracket followed by its tool calls.
//! A tool-only response's bracket is empty, `ev::assistant("")`: the AG-UI
//! projection hangs the calls off it (`parentMessageId`).

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ev {
  /// `MESSAGE_START` (assistant), `MESSAGE_DELTA`s, `MESSAGE_END`, all with
  /// one message id. The reasoning deltas concatenate to `reasoning` and
  /// the text deltas to `text` (no deltas of a kind if it's empty).
  Assistant { reasoning: String, text: String },
  /// `MESSAGE_START` (assistant) and text `MESSAGE_DELTA`s concatenating to
  /// `text`, never closed: the response failed mid-stream.
  Partial { text: String },
  /// `TOOL_CALL` with the model's call id, verbatim.
  ToolCall {
    id: String,
    name: String,
    args: Value,
  },
  /// `TOOL_RESULT` for the call `id`.
  ToolResult { id: String, result: Value },
  /// `ERROR` whose message contains this. The turn's `TurnEnd` then expects
  /// an errored turn.
  Error { contains: String },
  /// `ERROR` with this `code`, like [`Ev::Error`] otherwise.
  ErrorCode { code: String },
  /// `SESSION_ERROR` whose message contains this: the platform ended the
  /// turn because the runner failed (e.g. died mid-tool). The turn's
  /// `TurnEnd` then expects a failed turn.
  SessionError { contains: String },
  /// `TOOL_APPROVAL_REQUIRED` for the call `id`, suspending the turn. It
  /// must be the turn's last event.
  ApprovalRequired {
    id: String,
    name: String,
    args: Value,
  },
  /// `NO_MORE_ACTIONS`, ending the turn: cleanly, or as errored if the
  /// turn's output matched an `Error`.
  TurnEnd,
  /// Exactly one event, matching any of these: the framework decides which
  /// (e.g. which of two gated calls pauses first).
  OneOf(Vec<Ev>),
}

impl Ev {
  /// Whether this pattern matches exactly one event.
  pub fn is_single(&self) -> bool {
    matches!(
      self,
      Ev::ToolCall { .. }
        | Ev::ToolResult { .. }
        | Ev::Error { .. }
        | Ev::ErrorCode { .. }
        | Ev::SessionError { .. }
        | Ev::ApprovalRequired { .. }
    )
  }
}

impl std::fmt::Display for Ev {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    let list = |f: &mut std::fmt::Formatter<'_>, evs: &[Ev]| {
      let items: Vec<String> = evs.iter().map(Ev::to_string).collect();
      write!(f, "[{}]", items.join(", "))
    };
    match self {
      Ev::Assistant { reasoning, text } if reasoning.is_empty() => {
        write!(f, "assistant message {text:?}")
      }
      Ev::Assistant { reasoning, text } => {
        write!(f, "assistant message {text:?} with reasoning {reasoning:?}")
      }
      Ev::Partial { text } => write!(f, "unclosed assistant message {text:?}"),
      Ev::ToolCall { id, name, args } => write!(f, "TOOL_CALL {id} {name}({args})"),
      Ev::ToolResult { id, result } => write!(f, "TOOL_RESULT {id} {result}"),
      Ev::Error { contains } => write!(f, "ERROR containing {contains:?}"),
      Ev::ErrorCode { code } => write!(f, "ERROR with code {code:?}"),
      Ev::SessionError { contains } => write!(f, "SESSION_ERROR containing {contains:?}"),
      Ev::ApprovalRequired { id, name, args } => {
        write!(f, "TOOL_APPROVAL_REQUIRED {id} {name}({args})")
      }
      Ev::TurnEnd => write!(f, "NO_MORE_ACTIONS"),
      Ev::OneOf(evs) => {
        write!(f, "one of ")?;
        list(f, evs)
      }
    }
  }
}

/// `""` matches an empty bracket (no deltas).
pub fn assistant(text: &str) -> Ev {
  message("", text)
}

/// An assistant message cut off after `text`, with no `MESSAGE_END`.
pub fn partial(text: &str) -> Ev {
  Ev::Partial { text: text.into() }
}

/// An assistant message with reasoning, and text (`""` for none).
pub fn message(reasoning: &str, text: &str) -> Ev {
  Ev::Assistant {
    reasoning: reasoning.into(),
    text: text.into(),
  }
}

pub fn tool_call(id: &str, name: &str, args: Value) -> Ev {
  Ev::ToolCall {
    id: id.into(),
    name: name.into(),
    args,
  }
}

pub fn tool_result(id: &str, result: Value) -> Ev {
  Ev::ToolResult {
    id: id.into(),
    result,
  }
}

pub fn error_containing(contains: &str) -> Ev {
  Ev::Error {
    contains: contains.into(),
  }
}

pub fn error_code(code: &str) -> Ev {
  Ev::ErrorCode { code: code.into() }
}

pub fn session_error_containing(contains: &str) -> Ev {
  Ev::SessionError {
    contains: contains.into(),
  }
}

pub fn approval_required(id: &str, name: &str, args: Value) -> Ev {
  Ev::ApprovalRequired {
    id: id.into(),
    name: name.into(),
    args,
  }
}

pub fn turn_end() -> Ev {
  Ev::TurnEnd
}

/// One event matching any of `evs` (each a single event).
pub fn one_of(evs: &[Ev]) -> Ev {
  assert!(evs.iter().all(Ev::is_single), "one_of takes single events");
  Ev::OneOf(evs.to_vec())
}
