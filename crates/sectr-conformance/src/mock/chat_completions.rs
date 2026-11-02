//! Chat Completions: [`Endpoint::OpenAiChat`], and OpenRouter's dialect of
//! it, [`Endpoint::OpenRouter`], which streams reasoning with signed
//! `reasoning_details` (the layout copies a real capture). Streamed, or as
//! one `chat.completion` when the request doesn't stream.

use axum::response::Response;
use serde_json::{Value, json};

use super::{End, ModelRequest, Reply, sse};
use crate::endpoint::Endpoint;
use crate::msg::Msg;

/// The `format` OpenRouter gives Claude's thinking.
pub(super) const CLAUDE_FORMAT: &str = "anthropic-claude-v1";

/// `issued`: the reasoning signatures this run's replies carried, for the
/// validator.
pub(super) fn parse(
  raw: Value,
  endpoint: Endpoint,
  issued: &[String],
) -> Result<ModelRequest, String> {
  let messages = raw
    .get("messages")
    .and_then(Value::as_array)
    .ok_or("request has no `messages` array")?;
  super::validate::messages(messages, endpoint, issued)?;
  let mut system = Vec::new();
  let mut history = Vec::new();
  for (i, m) in messages.iter().enumerate() {
    let role = m.get("role").and_then(Value::as_str).unwrap_or_default();
    let text = content_text(m.get("content")).map_err(|e| format!("messages[{i}]: {e}"))?;
    match role {
      "system" | "developer" => system.push(text),
      "user" => history.push(Msg::User(text)),
      "assistant" => {
        if let Some(reasoning) = replayed_reasoning(m) {
          history.push(reasoning);
        }
        if !text.is_empty() {
          history.push(Msg::Assistant(text));
        }
        let calls = m.get("tool_calls").and_then(Value::as_array);
        for (j, c) in calls.into_iter().flatten().enumerate() {
          let field = |path: &str| {
            c.pointer(path)
              .and_then(Value::as_str)
              .ok_or_else(|| format!("messages[{i}].tool_calls[{j}]: no `{path}`"))
          };
          let arguments = field("/function/arguments")?;
          history.push(Msg::ToolCall {
            id: field("/id")?.into(),
            name: field("/function/name")?.into(),
            args: serde_json::from_str(arguments).unwrap_or_else(|_| json!(arguments)),
          });
        }
      }
      "tool" => history.push(Msg::ToolResult {
        id: m
          .get("tool_call_id")
          .and_then(Value::as_str)
          .ok_or_else(|| format!("messages[{i}]: tool message without `tool_call_id`"))?
          .into(),
        content: text,
      }),
      other => {
        return Err(format!(
          "messages[{i}]: role `{other}` not supported by the mock yet"
        ));
      }
    }
  }
  let tools = raw
    .get("tools")
    .and_then(Value::as_array)
    .into_iter()
    .flatten()
    .filter_map(|t| Some(t.pointer("/function/name")?.as_str()?.to_owned()))
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

/// The reasoning an assistant message sends back: its `reasoning` text, or
/// (when that's empty) the text of its `reasoning_details`; signed if any
/// detail carries a signature.
fn replayed_reasoning(message: &Value) -> Option<Msg> {
  let details: Vec<&Value> = message
    .get("reasoning_details")
    .and_then(Value::as_array)
    .into_iter()
    .flatten()
    .collect();
  let mut text = message
    .get("reasoning")
    .and_then(Value::as_str)
    .unwrap_or_default()
    .to_owned();
  if text.is_empty() {
    text = details.iter().filter_map(|d| d["text"].as_str()).collect();
  }
  let signed = details.iter().any(|d| d["signature"].is_string());
  (!text.is_empty() || signed).then_some(Msg::Reasoning { text, signed })
}

/// A message's content as text: a string, or the concatenated text parts.
fn content_text(content: Option<&Value>) -> Result<String, String> {
  match content {
    None | Some(Value::Null) => Ok(String::new()),
    Some(Value::String(s)) => Ok(s.clone()),
    Some(Value::Array(parts)) => parts
      .iter()
      .enumerate()
      .map(|(j, p)| match p.get("type").and_then(Value::as_str) {
        // Every API rejects a bare (non-object) part, e.g. a tool's raw
        // list result: "content[0] must be an object".
        _ if !p.is_object() => Err(format!("content[{j}] must be an object, got {p}")),
        Some("text") => p
          .get("text")
          .and_then(Value::as_str)
          .map(str::to_owned)
          .ok_or_else(|| "text part without `text`".to_string()),
        other => Err(format!("content part type {other:?} not supported")),
      })
      .collect(),
    Some(other) => Err(format!("unexpected content {other}")),
  }
}

/// The reply as Chat Completions SSE, for `endpoint`:
///
/// - OpenAI: a role chunk, one chunk per text element, the tool calls (a
///   header chunk per call, then its arguments in two fragments), a
///   `finish_reason` chunk, a usage chunk if asked for, `[DONE]`. A
///   calls-only reply has `content: null`, as OpenAI sends it.
/// - OpenRouter: a `: OPENROUTER PROCESSING` comment, then the same parts
///   with the role on every delta (`content: ""`, or `null` beside tool
///   calls). Each reasoning element is a chunk with `reasoning` and the
///   matching `reasoning_details` fragment, followed by one fragment with
///   just `signature`; usage rides on the finish chunk.
///
/// A fault ([`End`] other than `Finish`) cuts the stream after the
/// content: no finish chunk, usage or `[DONE]`; an error chunk (OpenAI's
/// `{"error": ...}`, or OpenRouter's documented chunk with `error` and
/// `finish_reason: "error"`) or a connection reset where asked.
///
/// `signature` is required for an OpenRouter reply with reasoning.
pub(super) fn stream(
  reply: &Reply,
  request: &Value,
  endpoint: Endpoint,
  signature: Option<&str>,
  end: End,
  n: usize,
) -> Response {
  let id = format!("chatcmpl-mock-{n}");
  let openrouter = match endpoint {
    Endpoint::OpenAiChat => false,
    Endpoint::OpenRouter => true,
    Endpoint::OpenAiResponses | Endpoint::Anthropic => {
      unreachable!("not a Chat Completions endpoint")
    }
  };
  let model = request.get("model").cloned().unwrap_or(json!("mock"));
  let include_usage = request
    .pointer("/stream_options/include_usage")
    .and_then(Value::as_bool)
    .unwrap_or(false);
  let usage = json!({"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0});
  let chunk = |mut delta: Value, finish: Value| {
    if openrouter {
      let d = delta.as_object_mut().expect("deltas are objects");
      let content = if d.contains_key("tool_calls") {
        Value::Null
      } else {
        json!("")
      };
      d.entry("content").or_insert(content);
      d.entry("role").or_insert(json!("assistant"));
    }
    let mut chunk = json!({
      "id": id,
      "object": "chat.completion.chunk",
      "created": 0,
      "model": model,
      "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    });
    if openrouter && !finish.is_null() {
      chunk["usage"] = usage.clone();
    }
    chunk
  };
  let mut events = Vec::new();
  if !openrouter {
    let content = if reply.text.is_empty() && !reply.calls.is_empty() {
      Value::Null
    } else {
      json!("")
    };
    events.push(chunk(
      json!({"role": "assistant", "content": content}),
      Value::Null,
    ));
  }
  for r in &reply.reasoning {
    events.push(chunk(
      json!({
        "reasoning": r,
        "reasoning_details": [
          {"type": "reasoning.text", "text": r, "format": CLAUDE_FORMAT, "index": 0},
        ],
      }),
      Value::Null,
    ));
  }
  if !reply.reasoning.is_empty() {
    let signature = signature.expect("an OpenRouter reply with reasoning has a signature");
    events.push(chunk(
      json!({"reasoning_details": [
        {"type": "reasoning.text", "signature": signature, "format": CLAUDE_FORMAT, "index": 0},
      ]}),
      Value::Null,
    ));
  }
  for t in &reply.text {
    events.push(chunk(json!({"content": t}), Value::Null));
  }
  for (index, c) in reply.calls.iter().enumerate() {
    events.push(chunk(
      json!({"tool_calls": [{
        "index": index,
        "id": c.id,
        "type": "function",
        "function": {"name": c.name, "arguments": ""},
      }]}),
      Value::Null,
    ));
    let arguments = c.arguments();
    let (a, b) = arguments.split_at(arguments.floor_char_boundary(arguments.len() / 2));
    for fragment in [a, b] {
      events.push(chunk(
        json!({"tool_calls": [{"index": index, "function": {"arguments": fragment}}]}),
        Value::Null,
      ));
    }
  }
  let finish = if reply.calls.is_empty() {
    "stop"
  } else {
    "tool_calls"
  };
  match end {
    End::Finish => events.push(chunk(json!({}), json!(finish))),
    End::ErrorChunk if openrouter => {
      let mut c = chunk(json!({}), json!("error"));
      c["error"] = json!({"code": "server_error", "message": "Provider disconnected unexpectedly"});
      c.as_object_mut()
        .expect("chunks are objects")
        .remove("usage");
      events.push(c);
    }
    End::ErrorChunk => events.push(json!({"error": {
      "message": "The server had an error while processing your request.",
      "type": "server_error",
    }})),
    End::Truncate | End::Reset => {}
  }
  if end == End::Finish && include_usage && !openrouter {
    events.push(json!({
      "id": id,
      "object": "chat.completion.chunk",
      "created": 0,
      "model": model,
      "choices": [],
      "usage": usage,
    }));
  }
  let mut frames: Vec<String> = Vec::new();
  if openrouter {
    frames.push(": OPENROUTER PROCESSING\n\n".into());
  }
  frames.extend(events.iter().map(|e| format!("data: {e}\n\n")));
  if end == End::Finish {
    frames.push("data: [DONE]\n\n".into());
  }
  sse(frames, end == End::Reset)
}

/// The reply as one `chat.completion`, for a request that doesn't stream.
/// `signature` is required for a reply with reasoning (OpenRouter).
pub(super) fn complete(
  reply: &Reply,
  request: &Value,
  endpoint: Endpoint,
  signature: Option<&str>,
  n: usize,
) -> Value {
  let id = format!("chatcmpl-mock-{n}");
  let model = request.get("model").cloned().unwrap_or(json!("mock"));
  let text = reply.text.concat();
  let mut message = json!({
    "role": "assistant",
    "content": if text.is_empty() && !reply.calls.is_empty() { Value::Null } else { json!(text) },
  });
  if !reply.reasoning.is_empty() {
    let reasoning = reply.reasoning.concat();
    let signature = signature.expect("a reply with reasoning has a signature");
    message["reasoning"] = json!(reasoning);
    message["reasoning_details"] = json!([{"type": "reasoning.text", "text": reasoning,
      "signature": signature, "format": CLAUDE_FORMAT, "index": 0}]);
  }
  if !reply.calls.is_empty() {
    let calls: Vec<Value> = reply
      .calls
      .iter()
      .map(|c| {
        json!({"id": c.id, "type": "function",
          "function": {"name": c.name, "arguments": c.arguments()}})
      })
      .collect();
    message["tool_calls"] = json!(calls);
  }
  let finish = if reply.calls.is_empty() {
    "stop"
  } else {
    "tool_calls"
  };
  let mut completion = json!({
    "id": id,
    "object": "chat.completion",
    "created": 0,
    "model": model,
    "choices": [{"index": 0, "message": message, "finish_reason": finish}],
    "system_fingerprint": null,
    "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0},
  });
  if endpoint == Endpoint::OpenRouter {
    completion["provider"] = json!("Anthropic");
  }
  completion
}
