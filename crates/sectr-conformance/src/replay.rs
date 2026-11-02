//! The replay check: an invocation's first model request must start with
//! exactly what the previous invocation's last request sent, namely the
//! offered tools, the system prompt and the conversation so far. Providers
//! cache a request's prefix, so a later turn (or an approval resume) that
//! sends earlier history differently misses the cache from that point on
//! (see `schema/README.md`). The adapter rebuilds that
//! history from the journal, so this is where its rebuild shows.
//!
//! What counts as different is calibrated against the real APIs
//! (`calibrate/caching.py`, 2026-10-12: the cached tokens of a request
//! replayed with one difference). A cache miss: a tool call's arguments in
//! another key order (Chat Completions, Responses, and Anthropic's
//! `tool_use.input`), an assistant message's `name` dropped, an assistant
//! message's text and tool calls split in two. No miss, so ignored here:
//! the arguments' spacing, text sent as text parts or as a string (a tool
//! result, Responses' `output_text`), Responses items' `id` and `status`,
//! and a Responses message's `type` left to its default.
//! Other object keys' order isn't compared: the provider parses the body.

use std::fmt::Write as _;

use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};

use crate::endpoint::Endpoint;

/// What a request sends ahead of its new messages, normalized (see the
/// module docs).
#[derive(Debug, Clone, PartialEq)]
pub struct Prefix {
  tools: Value,
  system: Value,
  messages: Vec<Value>,
}

impl Prefix {
  /// The prefix of the request whose body is `text`.
  pub fn of(endpoint: Endpoint, text: &str) -> Prefix {
    let raw = serde_json::from_str::<Ordered>(text).map_or(Value::Null, |o| normalized(&o));
    let messages = match endpoint {
      Endpoint::OpenAiResponses => &raw["input"],
      _ => &raw["messages"],
    };
    let mut messages = match messages {
      Value::Array(items) => items.clone(),
      // A bare string `input`: the Responses API's single user message.
      other => vec![other.clone()],
    };
    if endpoint == Endpoint::OpenAiResponses {
      // An item with a `role` and no `type` is a message.
      for item in &mut messages {
        if let Some(item) = item.as_object_mut()
          && item.contains_key("role")
        {
          item
            .entry("type")
            .or_insert_with(|| Value::String("message".into()));
        }
      }
    }
    let system = match endpoint {
      Endpoint::OpenAiResponses => raw["instructions"].clone(),
      Endpoint::Anthropic => raw["system"].clone(),
      // In `messages`.
      Endpoint::OpenAiChat | Endpoint::OpenRouter => Value::Null,
    };
    Prefix {
      tools: raw["tools"].clone(),
      system,
      messages,
    }
  }
}

impl Prefix {
  /// This prefix with the reasoning its assistant messages send back left
  /// out: Chat Completions' `reasoning`, `reasoning_details` and
  /// `reasoning_content`, Responses' `reasoning` items, and Anthropic's
  /// `thinking` and `redacted_thinking` blocks.
  fn without_reasoning(&self) -> Prefix {
    const FIELDS: [&str; 3] = ["reasoning", "reasoning_details", "reasoning_content"];
    const BLOCKS: [&str; 3] = ["reasoning", "thinking", "redacted_thinking"];
    let is_reasoning = |v: &Value| v["type"].as_str().is_some_and(|t| BLOCKS.contains(&t));
    let messages = self
      .messages
      .iter()
      .filter(|m| !is_reasoning(m))
      .map(|m| {
        let mut m = m.clone();
        if let Some(fields) = m.as_object_mut() {
          fields.retain(|k, _| !FIELDS.contains(&k.as_str()));
          if let Some(Value::Array(blocks)) = fields.get_mut("content") {
            blocks.retain(|b| !is_reasoning(b));
          }
        }
        m
      })
      .collect();
    Prefix {
      messages,
      ..self.clone()
    }
  }
}

/// Whether `next` fails to start with `prev` only because it leaves out
/// reasoning `prev` sent back.
pub fn drops_only_reasoning(prev: &Prefix, next: &Prefix) -> bool {
  !compare(prev, next).is_empty()
    && compare(&prev.without_reasoning(), &next.without_reasoning()).is_empty()
}

/// How `next` fails to start with `prev`, in order: the tools, the system
/// prompt, then the first message that differs (every later one would
/// too once one shifts, and the cache misses from there anyway).
pub fn compare(prev: &Prefix, next: &Prefix) -> Vec<String> {
  let mut out = Vec::new();
  let differ = |what: &str, before: &Value, after: &Value| {
    format!(
      "{what} sent differently\n  before: {}\n  after:  {}",
      clip(before),
      clip(after)
    )
  };
  if prev.tools != next.tools {
    out.push(differ("the tools", &prev.tools, &next.tools));
  }
  if prev.system != next.system {
    out.push(differ("the system prompt", &prev.system, &next.system));
  }
  let last = prev.messages.len().saturating_sub(1);
  for (i, before) in prev.messages.iter().enumerate() {
    match next.messages.get(i) {
      Some(after) if after == before => {}
      // The previous request's last message, which this one only extends
      // (a user message after a failed turn, merged into the unanswered
      // one): nothing before it changes, so no cache hit is lost.
      Some(after) if i == last && extends(before, after) => {}
      Some(after) => {
        out.push(differ(&format!("message {i}"), before, after));
        break;
      }
      None => {
        out.push(format!(
          "messages {i}.. dropped (it sends {} of {})\n  first dropped: {}",
          next.messages.len(),
          prev.messages.len(),
          clip(before)
        ));
        break;
      }
    }
  }
  out
}

/// Whether message `after` is `before` with more text appended: the same
/// fields but `content`, a string (text parts are joined) that starts with
/// `before`'s.
fn extends(before: &Value, after: &Value) -> bool {
  let (Some(b), Some(a)) = (before.as_object(), after.as_object()) else {
    return false;
  };
  let others = |m: &Map<String, Value>| {
    m.iter()
      .filter(|(k, _)| *k != "content")
      .map(|(k, v)| (k.clone(), v.clone()))
      .collect::<Map<_, _>>()
  };
  match (b.get("content"), a.get("content")) {
    (Some(Value::String(b_text)), Some(Value::String(a_text))) => {
      others(b) == others(a) && a_text.len() > b_text.len() && a_text.starts_with(b_text.as_str())
    }
    _ => false,
  }
}

/// A JSON value that keeps its objects' key order (`serde_json::Value`
/// sorts keys).
#[derive(Debug, Clone)]
enum Ordered {
  Null,
  Bool(bool),
  Number(Number),
  String(String),
  Array(Vec<Ordered>),
  Object(Vec<(String, Ordered)>),
}

impl<'de> Deserialize<'de> for Ordered {
  fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
    struct V;
    impl<'de> Visitor<'de> for V {
      type Value = Ordered;
      fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("JSON")
      }
      fn visit_unit<E>(self) -> Result<Ordered, E> {
        Ok(Ordered::Null)
      }
      fn visit_bool<E>(self, b: bool) -> Result<Ordered, E> {
        Ok(Ordered::Bool(b))
      }
      fn visit_i64<E>(self, n: i64) -> Result<Ordered, E> {
        Ok(Ordered::Number(n.into()))
      }
      fn visit_u64<E>(self, n: u64) -> Result<Ordered, E> {
        Ok(Ordered::Number(n.into()))
      }
      fn visit_f64<E>(self, n: f64) -> Result<Ordered, E> {
        Ok(Number::from_f64(n).map_or(Ordered::Null, Ordered::Number))
      }
      fn visit_str<E>(self, s: &str) -> Result<Ordered, E> {
        Ok(Ordered::String(s.into()))
      }
      fn visit_string<E>(self, s: String) -> Result<Ordered, E> {
        Ok(Ordered::String(s))
      }
      fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Ordered, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element()? {
          items.push(item);
        }
        Ok(Ordered::Array(items))
      }
      fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Ordered, A::Error> {
        let mut pairs = Vec::new();
        while let Some(pair) = map.next_entry()? {
          pairs.push(pair);
        }
        Ok(Ordered::Object(pairs))
      }
    }
    deserializer.deserialize_any(V)
  }
}

/// `value` as compared (see the module docs): Anthropic's `tool_use.input`
/// as its compact JSON in key order; a call's `arguments` without the
/// spacing; Responses items' `id` and `status` dropped; a list of text
/// parts as their joined text.
fn normalized(value: &Ordered) -> Value {
  match value {
    Ordered::Null => Value::Null,
    Ordered::Bool(b) => Value::Bool(*b),
    Ordered::Number(n) => Value::Number(n.clone()),
    Ordered::String(s) => Value::String(s.clone()),
    Ordered::Array(items) => {
      let items: Vec<Value> = items.iter().map(normalized).collect();
      match joined(&items) {
        Some(text) => Value::String(text),
        None => Value::Array(items),
      }
    }
    Ordered::Object(pairs) => {
      let ty = pairs.iter().find_map(|(k, v)| match (k.as_str(), v) {
        ("type", Ordered::String(t)) => Some(t.as_str()),
        _ => None,
      });
      let item = matches!(ty, Some("message" | "function_call"));
      let mut map = Map::new();
      for (k, v) in pairs {
        let v = match (k.as_str(), v) {
          ("id" | "status", _) if item => continue,
          ("input", _) if ty == Some("tool_use") => Value::String(compact(v)),
          ("arguments", Ordered::String(s)) => Value::String(unspaced(s)),
          _ => normalized(v),
        };
        map.insert(k.clone(), v);
      }
      Value::Object(map)
    }
  }
}

/// The joined text of `items` if they're all text parts: `{type: "text" |
/// "input_text" | "output_text", text}`, with at most empty `annotations`
/// and `logprobs`.
fn joined(items: &[Value]) -> Option<String> {
  if items.is_empty() {
    return None;
  }
  items
    .iter()
    .map(|item| {
      let part = item.as_object()?;
      let text_type = matches!(
        part.get("type")?.as_str()?,
        "text" | "input_text" | "output_text"
      );
      let extras_empty = part.iter().all(|(k, v)| match k.as_str() {
        "type" | "text" => true,
        "annotations" | "logprobs" => v.as_array().is_some_and(Vec::is_empty),
        _ => false,
      });
      (text_type && extras_empty).then_some(())?;
      part.get("text")?.as_str()
    })
    .collect()
}

/// `value`'s compact JSON, its objects' keys in order.
fn compact(value: &Ordered) -> String {
  let mut out = String::new();
  write_compact(value, &mut out);
  out
}

fn write_compact(value: &Ordered, out: &mut String) {
  match value {
    Ordered::Null => out.push_str("null"),
    Ordered::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
    Ordered::Number(n) => {
      let _ = write!(out, "{n}");
    }
    Ordered::String(s) => out.push_str(&Value::String(s.clone()).to_string()),
    Ordered::Array(items) => {
      out.push('[');
      for (i, item) in items.iter().enumerate() {
        if i > 0 {
          out.push(',');
        }
        write_compact(item, out);
      }
      out.push(']');
    }
    Ordered::Object(pairs) => {
      out.push('{');
      for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
          out.push(',');
        }
        out.push_str(&Value::String(k.clone()).to_string());
        out.push(':');
        write_compact(v, out);
      }
      out.push('}');
    }
  }
}

/// JSON text `s` without whitespace outside its strings (text that isn't
/// JSON stays as is).
fn unspaced(s: &str) -> String {
  match serde_json::from_str::<Ordered>(s) {
    Ok(value) => compact(&value),
    Err(_) => s.into(),
  }
}

fn clip(value: &Value) -> String {
  let s = value.to_string();
  match s.char_indices().nth(400) {
    Some((at, _)) => format!("{}…", &s[..at]),
    None => s,
  }
}

#[cfg(test)]
mod tests {
  use serde_json::json;

  use super::*;

  fn of(endpoint: Endpoint, body: &str) -> Prefix {
    Prefix::of(endpoint, body)
  }

  fn chat(messages: Value) -> Prefix {
    of(
      Endpoint::OpenAiChat,
      &json!({"tools": [{"name": "t"}], "messages": messages}).to_string(),
    )
  }

  fn args(arguments: &str) -> Prefix {
    chat(json!([{"role": "assistant", "tool_calls": [{"function": {"arguments": arguments}}]}]))
  }

  #[test]
  fn dropped_reasoning_is_told_apart_from_other_changes() {
    let call = json!({"role": "assistant", "tool_calls": [{"function": {"arguments": "{}"}}]});
    let mut with = call.clone();
    with["reasoning_details"] =
      json!([{"type": "reasoning.text", "text": "Hm.", "signature": "s"}]);
    let (prev, next) = (chat(json!([with])), chat(json!([call])));
    assert!(drops_only_reasoning(&prev, &next));
    assert!(!drops_only_reasoning(&prev, &prev));
    let other = chat(json!([{"role": "assistant", "content": "Hi"}]));
    assert!(!drops_only_reasoning(&prev, &other));

    let responses = |input: Value| {
      of(
        Endpoint::OpenAiResponses,
        &json!({"input": input}).to_string(),
      )
    };
    let item = json!({"type": "function_call", "call_id": "c1", "arguments": "{}"});
    let reasoning = json!({"type": "reasoning", "encrypted_content": "e", "summary": []});
    assert!(drops_only_reasoning(
      &responses(json!([reasoning, item])),
      &responses(json!([item]))
    ));

    let anthropic = |content: Value| {
      of(
        Endpoint::Anthropic,
        &json!({"messages": [{"role": "assistant", "content": content}]}).to_string(),
      )
    };
    let tool_use = json!({"type": "tool_use", "id": "c1", "input": {}});
    let thinking = json!({"type": "thinking", "thinking": "Hm.", "signature": "s"});
    assert!(drops_only_reasoning(
      &anthropic(json!([thinking, tool_use])),
      &anthropic(json!([tool_use]))
    ));
  }

  #[test]
  fn a_request_that_extends_the_last_one_matches() {
    let prev = chat(json!([{"role": "user", "content": "Hi"}]));
    let next = chat(json!([
      {"role": "user", "content": "Hi"},
      {"role": "assistant", "content": "Hello"},
      {"role": "user", "content": "Bye"},
    ]));
    assert_eq!(compare(&prev, &next), Vec::<String>::new());
  }

  #[test]
  fn text_parts_and_a_string_match() {
    let prev = chat(json!([{"role": "tool", "content": [
      {"type": "text", "text": "Shipped."}, {"type": "text", "text": " Friday."}
    ]}]));
    let next = chat(json!([{"role": "tool", "content": "Shipped. Friday."}]));
    assert_eq!(compare(&prev, &next), Vec::<String>::new());
  }

  #[test]
  fn arguments_spacing_is_ignored_but_not_key_order() {
    let compact = args(r#"{"reason":"late","order_id":"42"}"#);
    assert_eq!(
      compare(&compact, &args(r#"{"reason": "late", "order_id": "42"}"#)).len(),
      0
    );
    assert_eq!(
      compare(&compact, &args(r#"{"order_id": "42", "reason": "late"}"#)).len(),
      1
    );
  }

  #[test]
  fn anthropic_tool_use_input_key_order_counts() {
    let body = |input: &str| {
      format!(
        r#"{{"messages": [{{"role": "assistant", "content": [{{"type": "tool_use", "id": "t1", "input": {input}}}]}}]}}"#
      )
    };
    let prev = of(
      Endpoint::Anthropic,
      &body(r#"{"reason": "late", "order_id": "42"}"#),
    );
    let same = of(
      Endpoint::Anthropic,
      &body(r#"{"reason":"late","order_id":"42"}"#),
    );
    let sorted = of(
      Endpoint::Anthropic,
      &body(r#"{"order_id":"42","reason":"late"}"#),
    );
    assert_eq!(compare(&prev, &same).len(), 0);
    assert_eq!(compare(&prev, &sorted).len(), 1);
  }

  #[test]
  fn other_object_key_order_is_not_compared() {
    let prev = of(
      Endpoint::OpenAiChat,
      r#"{"messages": [{"content": "x", "role": "user"}]}"#,
    );
    let next = of(
      Endpoint::OpenAiChat,
      r#"{"messages": [{"role": "user", "content": "x"}]}"#,
    );
    assert_eq!(compare(&prev, &next), Vec::<String>::new());
  }

  #[test]
  fn responses_item_ids_and_output_text_parts_are_ignored() {
    let prev = of(
      Endpoint::OpenAiResponses,
      &json!({"input": [
        {"type": "message", "id": "msg_1", "status": "completed", "role": "assistant",
         "content": [{"type": "output_text", "text": "Hi", "annotations": [], "logprobs": []}]},
        {"type": "function_call", "id": "fc_1", "status": "completed", "call_id": "c1", "arguments": "{}"},
      ]})
      .to_string(),
    );
    let next = of(
      Endpoint::OpenAiResponses,
      &json!({"input": [
        {"role": "assistant", "content": "Hi"},
        {"type": "function_call", "call_id": "c1", "arguments": "{}"},
      ]})
      .to_string(),
    );
    assert_eq!(compare(&prev, &next), Vec::<String>::new());
  }

  #[test]
  fn a_split_message_or_a_dropped_name_differs_once() {
    let prev = chat(json!([
      {"role": "assistant", "content": "A", "tool_calls": [{"id": "c1"}]},
      {"role": "tool", "content": "x"},
    ]));
    let split = chat(json!([
      {"role": "assistant", "content": "A"},
      {"role": "assistant", "tool_calls": [{"id": "c1"}]},
      {"role": "tool", "content": "x"},
    ]));
    assert_eq!(compare(&prev, &split).len(), 1);
    let named = chat(json!([{"role": "assistant", "name": "agent", "content": "A"}]));
    let unnamed = chat(json!([{"role": "assistant", "content": "A"}]));
    assert_eq!(compare(&named, &unnamed).len(), 1);
  }

  #[test]
  fn an_extended_last_message_matches() {
    let body =
      |content: Value| json!({"messages": [{"role": "user", "content": content}]}).to_string();
    let prev = of(
      Endpoint::Anthropic,
      &body(json!([{"type": "text", "text": "Hi"}])),
    );
    let next = of(
      Endpoint::Anthropic,
      &body(json!([{"type": "text", "text": "Hi"}, {"type": "text", "text": " Still there?"}])),
    );
    assert_eq!(compare(&prev, &next), Vec::<String>::new());
    // Not the last message: the history after it moved.
    let earlier =
      chat(json!([{"role": "user", "content": "Hi"}, {"role": "assistant", "content": "Yes"}]));
    let changed = chat(json!([
      {"role": "user", "content": "Hi again"},
      {"role": "assistant", "content": "Yes"},
    ]));
    assert_eq!(compare(&earlier, &changed).len(), 1);
  }

  #[test]
  fn dropped_messages_tools_and_system_differ() {
    let prev =
      chat(json!([{"role": "user", "content": "Hi"}, {"role": "user", "content": "Again"}]));
    let next = chat(json!([{"role": "user", "content": "Hi"}]));
    assert_eq!(compare(&prev, &next).len(), 1);
    let anthropic = |system: &str, tool: &str| {
      of(
        Endpoint::Anthropic,
        &json!({"system": system, "tools": [{"name": tool}], "messages": []}).to_string(),
      )
    };
    assert_eq!(compare(&anthropic("A", "t"), &anthropic("B", "u")).len(), 2);
  }

  #[test]
  fn a_bare_string_input_is_one_message() {
    let prev = of(Endpoint::OpenAiResponses, r#"{"input": "Hi"}"#);
    let next = of(
      Endpoint::OpenAiResponses,
      r#"{"input": [{"role": "user", "content": "Hi"}]}"#,
    );
    assert_eq!(compare(&prev, &next).len(), 1);
  }
}
