//! Journal prefixes a case seeds before its first turn
//! ([`Case::seed`](crate::case::Case::seed)): a prior conversation exactly
//! as the journal holds it, typically cut where a crash would cut it. Seeded
//! events come from no adapter, so a case tests the fold on any journal the
//! contract allows ("every prefix of a valid journal is a valid journal",
//! `schema/README.md`), not only on what the adapter under test writes.

use serde_json::Value;

#[derive(Debug, Clone)]
pub enum Seed {
  /// `INVOCATION_STARTED` with reason `user_message`.
  User(String),
  /// An assistant message: `MESSAGE_START`, a reasoning delta (if any), a
  /// text delta (if any), and `MESSAGE_END` unless `closed` is false.
  Assistant {
    reasoning: String,
    text: String,
    closed: bool,
  },
  ToolCall {
    id: String,
    name: String,
    args: Value,
  },
  ToolResult {
    id: String,
    result: Value,
  },
  ApprovalRequired {
    id: String,
    name: String,
    args: Value,
  },
  /// A runner `ERROR`.
  Error(String),
  /// A platform `SESSION_ERROR` (the runner died).
  SessionError(String),
  /// `NO_MORE_ACTIONS`.
  TurnEnd,
}

pub fn user(text: &str) -> Seed {
  Seed::User(text.into())
}

/// A closed assistant message.
pub fn assistant(text: &str) -> Seed {
  Seed::Assistant {
    reasoning: String::new(),
    text: text.into(),
    closed: true,
  }
}

/// An assistant message cut off after `text`: no `MESSAGE_END`.
pub fn partial(text: &str) -> Seed {
  Seed::Assistant {
    reasoning: String::new(),
    text: text.into(),
    closed: false,
  }
}

/// An assistant message cut off after reasoning, before any text.
pub fn partial_reasoning(reasoning: &str) -> Seed {
  Seed::Assistant {
    reasoning: reasoning.into(),
    text: String::new(),
    closed: false,
  }
}

pub fn tool_call(id: &str, name: &str, args: Value) -> Seed {
  Seed::ToolCall {
    id: id.into(),
    name: name.into(),
    args,
  }
}

pub fn tool_result(id: &str, result: Value) -> Seed {
  Seed::ToolResult {
    id: id.into(),
    result,
  }
}

pub fn approval_required(id: &str, name: &str, args: Value) -> Seed {
  Seed::ApprovalRequired {
    id: id.into(),
    name: name.into(),
    args,
  }
}

pub fn error(message: &str) -> Seed {
  Seed::Error(message.into())
}

pub fn session_error(message: &str) -> Seed {
  Seed::SessionError(message.into())
}

pub fn turn_end() -> Seed {
  Seed::TurnEnd
}
