//! A model request's conversation, normalized across SDKs: the system prompt
//! is pulled out (SDKs place it differently, and the harness checks it on
//! every request), content parts are flattened to text, and an assistant
//! message is flattened to the reasoning it replays (if any), its text (if
//! any), then its tool calls.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
  User(String),
  /// Reasoning an assistant message sends back to the model: the text, and
  /// whether it carries the provider's signature (so the provider can use
  /// the original thinking, not just its text).
  Reasoning {
    text: String,
    signed: bool,
  },
  Assistant(String),
  /// One entry of an assistant message's `tool_calls`. `args` is the parsed
  /// `arguments` string (the raw string if it isn't JSON).
  ToolCall {
    id: String,
    name: String,
    args: Value,
  },
  /// A `tool` message: the result of the call `id`, as text.
  ToolResult {
    id: String,
    content: String,
  },
}

pub fn user(text: &str) -> Msg {
  Msg::User(text.into())
}

/// Reasoning sent back with an assistant message, signed or not.
pub fn thought(text: &str, signed: bool) -> Msg {
  Msg::Reasoning {
    text: text.into(),
    signed,
  }
}

pub fn assistant(text: &str) -> Msg {
  Msg::Assistant(text.into())
}

pub fn tool_call(id: &str, name: &str, args: Value) -> Msg {
  Msg::ToolCall {
    id: id.into(),
    name: name.into(),
    args,
  }
}

pub fn tool_result(id: &str, content: &str) -> Msg {
  Msg::ToolResult {
    id: id.into(),
    content: content.into(),
  }
}

/// `history` with each run of tool calls split into one call per assistant
/// message, each followed by its results: the layout openai-agents'
/// LiteLLM model sends Anthropic (`_fix_tool_message_ordering`).
pub fn one_call_per_message(history: &[Msg]) -> Vec<Msg> {
  let mut out = Vec::with_capacity(history.len());
  let mut i = 0;
  while i < history.len() {
    if !matches!(history[i], Msg::ToolCall { .. }) {
      out.push(history[i].clone());
      i += 1;
      continue;
    }
    let calls = run(history, &mut i, |m| matches!(m, Msg::ToolCall { .. }));
    let mut results = run(history, &mut i, |m| matches!(m, Msg::ToolResult { .. }));
    for call in calls {
      let Msg::ToolCall { id, .. } = call else {
        unreachable!()
      };
      out.push(call.clone());
      let answered = |r: &Msg| matches!(r, Msg::ToolResult { id: r, .. } if r == id);
      out.extend(results.iter().filter(|r| answered(r)).map(|r| (*r).clone()));
      results.retain(|r| !answered(r));
    }
    out.extend(results.into_iter().cloned());
  }
  out
}

/// The messages from `i` on that match `is`, advancing `i` past them.
fn run<'a>(history: &'a [Msg], i: &mut usize, is: impl Fn(&Msg) -> bool) -> Vec<&'a Msg> {
  let start = *i;
  while *i < history.len() && is(&history[*i]) {
    *i += 1;
  }
  history[start..*i].iter().collect()
}

#[cfg(test)]
mod tests {
  use serde_json::json;

  use super::*;

  #[test]
  fn one_call_per_message_pairs_each_call_with_its_results() {
    let (c1, c2) = (
      tool_call("c1", "f", json!({})),
      tool_call("c2", "f", json!({})),
    );
    let (r1, r2) = (tool_result("c1", "a"), tool_result("c2", "b"));
    let history = [
      user("hi"),
      assistant("Checking."),
      c1.clone(),
      c2.clone(),
      r2.clone(),
      r1.clone(),
      user("thanks"),
    ];
    assert_eq!(
      one_call_per_message(&history),
      [
        user("hi"),
        assistant("Checking."),
        c1,
        r1,
        c2,
        r2,
        user("thanks")
      ]
    );
  }
}
