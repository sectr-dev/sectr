//! [`Endpoint::Anthropic`](crate::endpoint::Endpoint::Anthropic): the
//! Anthropic Messages API. Streamed, the event layout copies a real
//! `claude-haiku-5-5` capture (`calibrate/anthropic.py`): `message_start`,
//! then per content block `content_block_start` … `content_block_stop`
//! (thinking with its `thinking_delta`s and one `signature_delta`, text
//! with its `text_delta`s, tool use with its `input_json_delta`s), a `ping`
//! after the first block opens, then `message_delta` with the stop reason
//! and usage, and `message_stop`. A request that doesn't stream gets the
//! one message.

use axum::response::Response;
use serde_json::{Value, json};

use super::{End, ModelRequest, Reply, sse};
use crate::msg::Msg;

pub(super) fn parse(raw: Value, issued: &[String]) -> Result<ModelRequest, String> {
  super::validate::anthropic(&raw, issued)?;
  let system = match raw.get("system") {
    None | Some(Value::Null) => Vec::new(),
    Some(Value::String(s)) => vec![s.clone()],
    Some(Value::Array(blocks)) => vec![
      blocks
        .iter()
        .filter_map(|b| b["text"].as_str())
        .collect::<String>(),
    ],
    Some(other) => return Err(format!("unexpected `system` {other}")),
  };
  let messages = raw["messages"].as_array().expect("validated");
  let mut history = Vec::new();
  for (i, m) in messages.iter().enumerate() {
    let at = |e: String| format!("messages[{i}]: {e}");
    let assistant = m["role"] == "assistant";
    let blocks = match &m["content"] {
      Value::String(s) => vec![json!({"type": "text", "text": s})],
      content => content.as_array().expect("validated").clone(),
    };
    // One message per text block: Anthropic merges consecutive messages of
    // a role (calibrated), so two user messages in a row and one with two
    // text blocks are the same request.
    for (j, b) in blocks.iter().enumerate() {
      let at = |e: String| at(format!("content[{j}]: {e}"));
      let text = || b["text"].as_str().unwrap_or_default().to_owned();
      match b["type"].as_str().unwrap_or_default() {
        "text" if !assistant => history.push(Msg::User(text())),
        "text" if !text().is_empty() => history.push(Msg::Assistant(text())),
        "text" => {}
        "thinking" if assistant => history.push(Msg::Reasoning {
          text: b["thinking"].as_str().unwrap_or_default().into(),
          signed: true,
        }),
        "tool_use" if assistant => history.push(Msg::ToolCall {
          id: field(b, "id").map_err(at)?,
          name: field(b, "name").map_err(at)?,
          args: b["input"].clone(),
        }),
        "tool_result" if !assistant => history.push(Msg::ToolResult {
          id: field(b, "tool_use_id").map_err(at)?,
          content: content_text(&b["content"]).map_err(at)?,
        }),
        other => {
          return Err(at(format!(
            "block type `{other}` in a {} message not supported by the mock yet",
            m["role"]
          )));
        }
      }
    }
  }
  let tools = raw
    .get("tools")
    .and_then(Value::as_array)
    .into_iter()
    .flatten()
    .filter_map(|t| Some(t.get("name")?.as_str()?.to_owned()))
    .collect();
  Ok(ModelRequest {
    system,
    history,
    tools,
    stream: raw.get("stream") == Some(&Value::Bool(true)),
    raw,
    text: String::new(),
  })
}

fn field(block: &Value, key: &str) -> Result<String, String> {
  block[key]
    .as_str()
    .map(str::to_owned)
    .ok_or_else(|| format!("{} block without `{key}`", block["type"]))
}

/// A `tool_result`'s content as text: a string, or its text blocks
/// concatenated (as the other endpoints' parsers flatten content parts).
fn content_text(content: &Value) -> Result<String, String> {
  match content {
    Value::Null => Ok(String::new()),
    Value::String(s) => Ok(s.clone()),
    Value::Array(blocks) => blocks
      .iter()
      .map(|b| match b["type"].as_str() {
        Some("text") => Ok(b["text"].as_str().unwrap_or_default().to_owned()),
        other => Err(format!(
          "tool_result content block type {other:?} not supported by the mock yet"
        )),
      })
      .collect(),
    other => unreachable!("validated: {other}"),
  }
}

/// The reply as Messages API SSE (see the module docs). `signature` is the
/// thinking block's, required for a reply with reasoning. A fault cuts the
/// stream inside the last block: no `content_block_stop`, `message_delta`
/// or `message_stop`; an `error` event or a reset where asked.
pub(super) fn stream(
  reply: &Reply,
  request: &Value,
  signature: Option<&str>,
  end: End,
  n: usize,
) -> Response {
  let id = format!("msg_mock_{n}");
  let model = request.get("model").cloned().unwrap_or(json!("mock"));
  let usage = |output: u32| {
    json!({"input_tokens": 0, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0,
      "output_tokens": output})
  };
  let mut events: Vec<Value> = vec![json!({"type": "message_start", "message": {
    "model": model, "id": id, "type": "message", "role": "assistant", "content": [],
    "stop_reason": null, "stop_sequence": null, "usage": usage(1)}})];
  // Each block: its start, its deltas.
  let mut blocks: Vec<(Value, Vec<Value>)> = Vec::new();
  if !reply.reasoning.is_empty() {
    let signature = signature.expect("a reply with reasoning has a signature");
    let mut deltas: Vec<Value> = reply
      .reasoning
      .iter()
      .map(|r| json!({"type": "thinking_delta", "thinking": r}))
      .collect();
    deltas.push(json!({"type": "signature_delta", "signature": signature}));
    blocks.push((
      json!({"type": "thinking", "thinking": "", "signature": ""}),
      deltas,
    ));
  }
  if !reply.text.is_empty() {
    let deltas = reply
      .text
      .iter()
      .map(|t| json!({"type": "text_delta", "text": t}))
      .collect();
    blocks.push((json!({"type": "text", "text": ""}), deltas));
  }
  for c in &reply.calls {
    let input = c.arguments();
    let (a, b) = input.split_at(input.floor_char_boundary(input.len() / 2));
    let deltas = ["", a, b]
      .iter()
      .map(|p| json!({"type": "input_json_delta", "partial_json": p}))
      .collect();
    blocks.push((
      json!({"type": "tool_use", "id": c.id, "name": c.name, "input": {}}),
      deltas,
    ));
  }
  let last = blocks.len().saturating_sub(1);
  for (index, (start, deltas)) in blocks.into_iter().enumerate() {
    events.push(json!({"type": "content_block_start", "index": index, "content_block": start}));
    if index == 0 {
      events.push(json!({"type": "ping"}));
    }
    for delta in deltas {
      events.push(json!({"type": "content_block_delta", "index": index, "delta": delta}));
    }
    if index < last || end == End::Finish {
      events.push(json!({"type": "content_block_stop", "index": index}));
    }
  }
  match end {
    End::Finish => {
      let stop = if reply.calls.is_empty() {
        "end_turn"
      } else {
        "tool_use"
      };
      events.push(json!({"type": "message_delta",
        "delta": {"stop_reason": stop, "stop_sequence": null}, "usage": usage(1)}));
      events.push(json!({"type": "message_stop"}));
    }
    End::ErrorChunk => events.push(json!({"type": "error",
      "error": {"type": "overloaded_error", "message": "Overloaded"}})),
    End::Truncate | End::Reset => {}
  }
  let frames = events
    .into_iter()
    .map(|e| {
      format!(
        "event: {}\ndata: {e}\n\n",
        e["type"].as_str().expect("events have a type")
      )
    })
    .collect();
  sse(frames, end == End::Reset)
}

/// The reply as one message, for a request that doesn't stream.
/// `signature` is the thinking block's, required for a reply with
/// reasoning.
pub(super) fn complete(reply: &Reply, request: &Value, signature: Option<&str>, n: usize) -> Value {
  let id = format!("msg_mock_{n}");
  let model = request.get("model").cloned().unwrap_or(json!("mock"));
  let mut content: Vec<Value> = Vec::new();
  if !reply.reasoning.is_empty() {
    let signature = signature.expect("a reply with reasoning has a signature");
    content.push(
      json!({"type": "thinking", "thinking": reply.reasoning.concat(),
      "signature": signature}),
    );
  }
  if !reply.text.is_empty() {
    content.push(json!({"type": "text", "text": reply.text.concat()}));
  }
  for c in &reply.calls {
    content.push(json!({"type": "tool_use", "id": c.id, "name": c.name, "input": c.args}));
  }
  let stop = if reply.calls.is_empty() {
    "end_turn"
  } else {
    "tool_use"
  };
  json!({"model": model, "id": id, "type": "message", "role": "assistant",
    "content": content, "stop_reason": stop, "stop_sequence": null,
    "usage": {"input_tokens": 0, "cache_creation_input_tokens": 0,
      "cache_read_input_tokens": 0, "output_tokens": 1}})
}
