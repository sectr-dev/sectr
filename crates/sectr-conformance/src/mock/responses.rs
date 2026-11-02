//! [`Endpoint::OpenAiResponses`](crate::endpoint::Endpoint::OpenAiResponses): the OpenAI Responses API. Streamed, the
//! event layout copies a real `gpt-5.4-nano` capture
//! (`calibrate/responses.py`): `response.created`, `response.in_progress`,
//! then per output item `response.output_item.added` … `.done` (reasoning
//! with its summary deltas and `encrypted_content`, a message with its
//! `output_text` deltas, function calls with their argument deltas), then
//! `response.completed` carrying every item. A request that doesn't stream
//! gets that completed response alone.

use axum::response::Response;
use serde_json::{Value, json};

use super::{End, ModelRequest, Reply, sse};
use crate::msg::Msg;

pub(super) fn parse(raw: Value, issued: &[String]) -> Result<ModelRequest, String> {
  let items: Vec<Value> = match raw.get("input") {
    Some(Value::String(s)) => vec![json!({"role": "user", "content": s})],
    Some(Value::Array(items)) => items.clone(),
    _ => return Err("request has no `input`".into()),
  };
  super::validate::responses(&raw, &items, issued)?;
  let mut system: Vec<String> = raw
    .get("instructions")
    .and_then(Value::as_str)
    .map(str::to_owned)
    .into_iter()
    .collect();
  let mut history = Vec::new();
  for (i, item) in items.iter().enumerate() {
    let at = |e: String| format!("input[{i}]: {e}");
    let kind = item
      .get("type")
      .and_then(Value::as_str)
      .unwrap_or("message");
    match kind {
      "message" => {
        let text = content_text(item.get("content")).map_err(at)?;
        match item.get("role").and_then(Value::as_str).unwrap_or_default() {
          "system" | "developer" => system.push(text),
          "user" => history.push(Msg::User(text)),
          "assistant" if !text.is_empty() => history.push(Msg::Assistant(text)),
          "assistant" => {}
          other => return Err(at(format!("role `{other}` not supported by the mock yet"))),
        }
      }
      "reasoning" => {
        let text: String = item
          .get("summary")
          .and_then(Value::as_array)
          .into_iter()
          .flatten()
          .filter_map(|s| s.get("text").and_then(Value::as_str))
          .collect();
        let signed = item.get("encrypted_content").is_some_and(Value::is_string);
        if !text.is_empty() || signed {
          history.push(Msg::Reasoning { text, signed });
        }
      }
      "function_call" => {
        let field = |k: &str| {
          item
            .get(k)
            .and_then(Value::as_str)
            .ok_or_else(|| at(format!("function_call without `{k}`")))
        };
        let arguments = field("arguments")?;
        history.push(Msg::ToolCall {
          id: field("call_id")?.into(),
          name: field("name")?.into(),
          args: serde_json::from_str(arguments).unwrap_or_else(|_| json!(arguments)),
        });
      }
      "function_call_output" => history.push(Msg::ToolResult {
        id: item
          .get("call_id")
          .and_then(Value::as_str)
          .ok_or_else(|| at("function_call_output without `call_id`".into()))?
          .into(),
        content: content_text(item.get("output")).map_err(at)?,
      }),
      other => {
        return Err(at(format!(
          "item type `{other}` not supported by the mock yet"
        )));
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

/// A message's content or a call's output as text: a string, or the
/// concatenated text parts (`input_text`, `output_text`).
fn content_text(content: Option<&Value>) -> Result<String, String> {
  match content {
    None | Some(Value::Null) => Ok(String::new()),
    Some(Value::String(s)) => Ok(s.clone()),
    Some(Value::Array(parts)) => parts
      .iter()
      .enumerate()
      .map(|(j, p)| match p.get("type").and_then(Value::as_str) {
        _ if !p.is_object() => Err(format!("part [{j}] must be an object, got {p}")),
        Some("input_text" | "output_text") => p
          .get("text")
          .and_then(Value::as_str)
          .map(str::to_owned)
          .ok_or_else(|| "text part without `text`".to_string()),
        other => Err(format!("content part type {other:?} not supported")),
      })
      .collect(),
    // OpenAI: "expected one of a string or array of objects".
    Some(other) => Err(format!(
      "invalid type: expected one of a string or array of objects, got {other}"
    )),
  }
}

/// The reply as Responses SSE (see the module docs). `encrypted` is the
/// reasoning item's `encrypted_content`, required for a reply with
/// reasoning. A fault cuts the stream after the content: no
/// `response.completed`; an `error` event or a reset where asked.
pub(super) fn stream(
  reply: &Reply,
  request: &Value,
  encrypted: Option<&str>,
  end: End,
  n: usize,
) -> Response {
  let frames = events(reply, request, encrypted, end, n)
    .into_iter()
    .enumerate()
    .map(|(n, mut e)| {
      e["sequence_number"] = json!(n);
      format!(
        "event: {}\ndata: {e}\n\n",
        e["type"].as_str().expect("events have a type")
      )
    })
    .collect();
  sse(frames, end == End::Reset)
}

/// The reply as one completed response, for a request that doesn't stream:
/// what `response.completed` carries.
pub(super) fn complete(reply: &Reply, request: &Value, encrypted: Option<&str>, n: usize) -> Value {
  let mut events = events(reply, request, encrypted, End::Finish, n);
  let completed = events
    .pop()
    .expect("a finished reply ends in response.completed");
  completed["response"].clone()
}

/// The reply's SSE events, unnumbered.
fn events(
  reply: &Reply,
  request: &Value,
  encrypted: Option<&str>,
  end: End,
  n: usize,
) -> Vec<Value> {
  let model = request.get("model").cloned().unwrap_or(json!("mock"));
  let mut events: Vec<Value> = Vec::new();
  let response = |status: &str, output: &[Value]| {
    json!({
      "id": format!("resp_mock_{n}"),
      "object": "response",
      "created_at": 0,
      "status": status,
      "background": false,
      "error": null,
      "incomplete_details": null,
      "instructions": request.get("instructions").cloned().unwrap_or(Value::Null),
      "max_output_tokens": null,
      "metadata": {},
      "model": model,
      "output": output,
      "parallel_tool_calls": true,
      "previous_response_id": null,
      "reasoning": {"effort": null, "summary": null},
      "store": false,
      "temperature": 1.0,
      "text": {"format": {"type": "text"}},
      "tool_choice": request.get("tool_choice").cloned().unwrap_or(json!("auto")),
      "tools": request.get("tools").cloned().unwrap_or(json!([])),
      "top_p": 1.0,
      "truncation": "disabled",
      "usage": if status == "completed" {
        json!({
          "input_tokens": 0,
          "input_tokens_details": {"cached_tokens": 0},
          "output_tokens": 0,
          "output_tokens_details": {"reasoning_tokens": 0},
          "total_tokens": 0,
        })
      } else {
        Value::Null
      },
      "user": null,
    })
  };
  events.push(json!({"type": "response.created", "response": response("in_progress", &[])}));
  events.push(json!({"type": "response.in_progress", "response": response("in_progress", &[])}));
  let mut output: Vec<Value> = Vec::new();

  if !reply.reasoning.is_empty() {
    let index = output.len();
    let id = format!("rs_mock_{n}_{index}");
    let encrypted = encrypted.expect("a reply with reasoning has encrypted content");
    let text = reply.reasoning.concat();
    let item = |summary: Value| json!({"id": id, "type": "reasoning", "content": [], "encrypted_content": encrypted, "summary": summary});
    events.push(
      json!({"type": "response.output_item.added", "item": item(json!([])), "output_index": index}),
    );
    events.push(
      json!({"type": "response.reasoning_summary_part.added", "item_id": id,
      "output_index": index, "part": {"type": "summary_text", "text": ""}, "summary_index": 0}),
    );
    for r in &reply.reasoning {
      events.push(
        json!({"type": "response.reasoning_summary_text.delta", "delta": r,
        "item_id": id, "output_index": index, "summary_index": 0}),
      );
    }
    events.push(
      json!({"type": "response.reasoning_summary_text.done", "item_id": id,
      "output_index": index, "summary_index": 0, "text": text}),
    );
    events.push(
      json!({"type": "response.reasoning_summary_part.done", "item_id": id,
      "output_index": index, "part": {"type": "summary_text", "text": text}, "summary_index": 0}),
    );
    let done = item(json!([{"type": "summary_text", "text": text}]));
    events.push(json!({"type": "response.output_item.done", "item": done, "output_index": index}));
    output.push(done);
  }

  if !reply.text.is_empty() {
    let index = output.len();
    let id = format!("msg_mock_{n}_{index}");
    let text = reply.text.concat();
    let part =
      |text: &str| json!({"type": "output_text", "annotations": [], "logprobs": [], "text": text});
    let item = |status: &str, content: Value| json!({"id": id, "type": "message", "status": status, "content": content, "role": "assistant"});
    events.push(
      json!({"type": "response.output_item.added", "item": item("in_progress", json!([])),
      "output_index": index}),
    );
    events.push(
      json!({"type": "response.content_part.added", "item_id": id, "output_index": index,
      "content_index": 0, "part": part("")}),
    );
    for t in &reply.text {
      events.push(
        json!({"type": "response.output_text.delta", "delta": t, "item_id": id,
        "output_index": index, "content_index": 0, "logprobs": []}),
      );
    }
    events.push(
      json!({"type": "response.output_text.done", "text": text, "item_id": id,
      "output_index": index, "content_index": 0, "logprobs": []}),
    );
    events.push(
      json!({"type": "response.content_part.done", "item_id": id, "output_index": index,
      "content_index": 0, "part": part(&text)}),
    );
    let done = item("completed", json!([part(&text)]));
    events.push(json!({"type": "response.output_item.done", "item": done, "output_index": index}));
    output.push(done);
  }

  for c in &reply.calls {
    let index = output.len();
    let id = format!("fc_mock_{n}_{index}");
    let arguments = c.arguments();
    let item = |status: &str, arguments: &str| {
      json!({"id": id, "type": "function_call", "status": status, "arguments": arguments,
        "call_id": c.id, "name": c.name})
    };
    events.push(
      json!({"type": "response.output_item.added", "item": item("in_progress", ""),
      "output_index": index}),
    );
    let (a, b) = arguments.split_at(arguments.floor_char_boundary(arguments.len() / 2));
    for fragment in [a, b] {
      events.push(
        json!({"type": "response.function_call_arguments.delta", "delta": fragment,
        "item_id": id, "output_index": index}),
      );
    }
    events.push(
      json!({"type": "response.function_call_arguments.done", "arguments": arguments,
      "item_id": id, "output_index": index}),
    );
    let done = item("completed", &arguments);
    events.push(json!({"type": "response.output_item.done", "item": done, "output_index": index}));
    output.push(done);
  }

  match end {
    End::Finish => {
      events.push(json!({"type": "response.completed", "response": response("completed", &output)}))
    }
    End::ErrorChunk => events.push(json!({"type": "error", "code": "server_error",
      "message": "The server had an error while processing your request.", "param": null})),
    End::Truncate | End::Reset => {}
  }
  events
}
