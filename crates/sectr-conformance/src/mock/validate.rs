//! The request validator: Chat Completions `messages` the real APIs reject,
//! per endpoint, rejected here too (a 400 that fails the case). Only rules
//! a real API was seen enforcing (calibrated 2026-10-10 against OpenAI
//! `gpt-5.4-nano` for [`Endpoint::OpenAiChat`], OpenRouter routed to
//! `anthropic/claude-haiku-5.5` for [`Endpoint::OpenRouter`], by
//! `calibrate/validator.py`, which re-runs it). What both
//! accepted stays accepted: tool results out of call order, two user
//! messages in a row, `content: ""`, arguments that aren't JSON.
//!
//! OpenRouter routed to Claude repairs several shapes OpenAI rejects (an
//! unanswered call followed by more conversation, a result for an unknown
//! call), so pairing is checked strictly only on `openai_chat`; every case
//! runs on both.
//!
//! On `openai_responses` (calibrated by `calibrate/responses.py` against
//! `gpt-5.4-nano`), see [`responses`]; on `anthropic` (calibrated by
//! `calibrate/anthropic.py` against `claude-haiku-5-5`), see [`anthropic`].
//!
//! On `openrouter`, reasoning sent back is checked too (calibrated by
//! `calibrate/reasoning.py`): a signature must be one this run's replies
//! carried, unmodified.

use std::collections::HashSet;

use serde_json::{Value, json};

use crate::endpoint::Endpoint;

/// `issued`: the reasoning signatures this run's replies carried.
pub(super) fn messages(
  messages: &[Value],
  endpoint: Endpoint,
  issued: &[String],
) -> Result<(), String> {
  for (i, m) in messages.iter().enumerate() {
    let at = |e: String| format!("messages[{i}]: {e}");
    if m["role"] == "assistant" {
      unique_call_ids(m).map_err(at)?;
      match endpoint {
        Endpoint::OpenAiChat => content_present(m).map_err(at)?,
        Endpoint::OpenRouter => {
          object_arguments(m).map_err(at)?;
          intact_signatures(m, issued).map_err(at)?;
        }
        Endpoint::OpenAiResponses | Endpoint::Anthropic => {
          unreachable!("not a Chat Completions endpoint")
        }
      }
    }
  }
  match endpoint {
    Endpoint::OpenAiChat => tool_pairing(messages),
    Endpoint::OpenRouter => ends_with_input(messages),
    Endpoint::OpenAiResponses | Endpoint::Anthropic => {
      unreachable!("not a Chat Completions endpoint")
    }
  }
}

/// The Responses API's `input` (OpenAI rejects these):
///
/// - a `function_call` with no `function_call_output` for its `call_id`
///   anywhere ("No tool output found for function call ..."), or an output
///   with no call ("No tool call found for function call output ...");
///   order and duplicates were accepted;
/// - a reasoning item whose `encrypted_content` isn't one this run's
///   replies carried, unmodified ("The encrypted content ... could not be
///   verified"). Dropping the item, or its `encrypted_content`, was
///   accepted.
///
/// And Sectr's own rule: the runner is stateless, so a request carries the
/// whole conversation, with no `previous_response_id` or `conversation`.
pub(super) fn responses(raw: &Value, items: &[Value], issued: &[String]) -> Result<(), String> {
  for key in ["previous_response_id", "conversation"] {
    if raw.get(key).is_some_and(|v| !v.is_null()) {
      return Err(format!(
        "`{key}`: the runner is stateless; a request must carry the whole conversation"
      ));
    }
  }
  let ids = |kind: &str| -> Vec<&str> {
    items
      .iter()
      .filter(|i| i["type"] == kind)
      .filter_map(|i| i["call_id"].as_str())
      .collect()
  };
  let (calls, outputs) = (ids("function_call"), ids("function_call_output"));
  if let Some(id) = calls.iter().find(|id| !outputs.contains(id)) {
    return Err(format!("No tool output found for function call {id}."));
  }
  if let Some(id) = outputs.iter().find(|id| !calls.contains(id)) {
    return Err(format!(
      "No tool call found for function call output with call_id {id}."
    ));
  }
  for (i, item) in items.iter().enumerate() {
    if item["type"] == "reasoning"
      && let Some(encrypted) = item["encrypted_content"].as_str()
      && !issued.iter().any(|s| s == encrypted)
    {
      return Err(format!(
        "input[{i}]: The encrypted content for item {} could not be verified (not one this \
         run's replies carried, or modified)",
        item["id"]
      ));
    }
  }
  Ok(())
}

/// The Messages API request. Anthropic rejected these (calibrated
/// 2026-10-10 against `claude-haiku-5-5`, twice, with the same results):
///
/// - `thinking.type: "enabled"` (the model takes only `adaptive`);
/// - a role other than `user` or `assistant` (the system prompt is the
///   top-level `system`);
/// - content that is neither a string nor an array (null), or a block that
///   isn't an object;
/// - an empty text block in a message's content ("text content blocks must
///   be non-empty"; empty string content was accepted, and so was an empty
///   text block inside a `tool_result`, as pydantic-ai sends a `""` result);
/// - a thinking block without a signature, or whose signature isn't one
///   this run's replies carried, unmodified ("Invalid `signature` in
///   `thinking` block"), in any turn. Dropping the thinking, rewriting or
///   emptying its text, or moving it after the call were accepted, with
///   thinking enabled or not;
/// - `tool_use` input that isn't an object, or ids repeated within a
///   message;
/// - with consecutive same-role messages merged (two in a row were
///   accepted, for either role): a `tool_use` whose `tool_result` isn't
///   among the blocks opening the next message ("`tool_use` ids were found
///   without `tool_result` blocks immediately after"; text before it, or
///   between two results, counts as not immediately after), or a
///   `tool_result` with no `tool_use` in the previous message;
/// - `tool_result` content that is neither a string nor an array of
///   objects;
/// - a conversation ending with an assistant message ("This model does
///   not support assistant message prefill").
///
/// Accepted besides: results out of call order, text after the results, a
/// call id reused in a later turn, a conversation opening with the
/// assistant.
pub(super) fn anthropic(raw: &Value, issued: &[String]) -> Result<(), String> {
  if raw.pointer("/thinking/type") == Some(&json!("enabled")) {
    return Err(
      "\"thinking.type.enabled\" is not supported for this model. Use \"thinking.type.adaptive\" \
       and \"output_config.effort\" to control thinking behavior."
        .into(),
    );
  }
  let messages = raw
    .get("messages")
    .and_then(Value::as_array)
    .ok_or("request has no `messages` array")?;
  // (role, index of its first message, blocks), consecutive roles merged.
  let mut merged: Vec<(&str, usize, Vec<Value>)> = Vec::new();
  for (i, m) in messages.iter().enumerate() {
    let at = |e: String| format!("messages[{i}]: {e}");
    let role = m["role"].as_str().unwrap_or_default();
    if role != "user" && role != "assistant" {
      return Err(at(format!(
        "role {role:?}: use the top-level 'system' parameter for the system prompt"
      )));
    }
    let blocks = match &m["content"] {
      Value::String(s) => vec![json!({"type": "text", "text": s})],
      Value::Array(blocks) => {
        for (j, b) in blocks.iter().enumerate() {
          anthropic_block(b, issued).map_err(|e| at(format!("content[{j}]: {e}")))?;
        }
        blocks.clone()
      }
      other => {
        return Err(at(format!(
          "content: Input should be a valid array, got {other}"
        )));
      }
    };
    let mut seen = HashSet::new();
    for id in block_ids(&blocks, "tool_use", "id") {
      if !seen.insert(id) {
        return Err(at(format!("`tool_use` ids must be unique: {id:?}")));
      }
    }
    match merged.last_mut() {
      Some((last, _, previous)) if *last == role => previous.extend(blocks),
      _ => merged.push((role, i, blocks)),
    }
  }
  for (k, (role, i, blocks)) in merged.iter().enumerate() {
    let called: Vec<&str> = match k.checked_sub(1).map(|p| &merged[p]) {
      Some(("assistant", _, previous)) => block_ids(previous, "tool_use", "id").collect(),
      _ => Vec::new(),
    };
    if let Some(id) =
      block_ids(blocks, "tool_result", "tool_use_id").find(|id| !called.contains(id))
    {
      return Err(format!(
        "messages[{i}]: unexpected `tool_use_id` found in `tool_result` blocks: {id}. Each \
         `tool_result` block must have a corresponding `tool_use` block in the previous message."
      ));
    }
    if *role == "assistant" {
      let answered: Vec<&str> = merged
        .get(k + 1)
        .into_iter()
        .flat_map(|(_, _, next)| next.iter())
        .take_while(|b| b["type"] == "tool_result")
        .filter_map(|b| b["tool_use_id"].as_str())
        .collect();
      let missing: Vec<&str> = block_ids(blocks, "tool_use", "id")
        .filter(|id| !answered.contains(id))
        .collect();
      if !missing.is_empty() {
        return Err(format!(
          "messages[{i}]: `tool_use` ids were found without `tool_result` blocks immediately \
           after: {}. Each `tool_use` block must have a corresponding `tool_result` block in the \
           next message.",
          missing.join(", ")
        ));
      }
    }
  }
  match merged.last() {
    Some(("assistant", _, _)) => Err(
      "This model does not support assistant message prefill. The conversation must end with a \
       user message."
        .into(),
    ),
    _ => Ok(()),
  }
}

/// One Messages API content block, on its own (see [`anthropic`]).
fn anthropic_block(b: &Value, issued: &[String]) -> Result<(), String> {
  if !b.is_object() {
    return Err(format!("Input should be an object, got {b}"));
  }
  match b["type"].as_str().unwrap_or_default() {
    "text" if b["text"].as_str().is_none_or(str::is_empty) => {
      Err("text content blocks must be non-empty".into())
    }
    "thinking" => match b["signature"].as_str() {
      None => Err("thinking.signature: Field required".into()),
      Some(signature) if !issued.iter().any(|s| s == signature) => Err(format!(
        "Invalid `signature` in `thinking` block (not one this run's replies carried, or \
         modified): {signature:?}"
      )),
      Some(_) => Ok(()),
    },
    "tool_use" if !b["input"].is_object() => Err(format!(
      "tool_use.input: Input should be an object, got {}",
      b["input"]
    )),
    "tool_result" => match &b["content"] {
      Value::Null | Value::String(_) => Ok(()),
      Value::Array(parts) => match parts.iter().position(|p| !p.is_object()) {
        Some(j) => Err(format!(
          "tool_result.content[{j}]: Input should be an object"
        )),
        None => Ok(()),
      },
      other => Err(format!(
        "tool_result content must either be a string or a list of content blocks, got {other}"
      )),
    },
    _ => Ok(()),
  }
}

/// The `field` of each `kind` block.
fn block_ids<'a>(
  blocks: &'a [Value],
  kind: &'a str,
  field: &'a str,
) -> impl Iterator<Item = &'a str> {
  blocks
    .iter()
    .filter(move |b| b["type"] == kind)
    .filter_map(move |b| b[field].as_str())
}

/// Both: an assistant message's call ids are unique. (OpenAI: "messages
/// with role 'tool' must be a response to a preceeding message with
/// 'tool_calls'"; Anthropic: "`tool_use` ids must be unique".)
fn unique_call_ids(m: &Value) -> Result<(), String> {
  let mut seen = HashSet::new();
  for c in calls(m) {
    let id = c["id"].as_str().unwrap_or_default();
    if !seen.insert(id) {
      return Err(format!("duplicate tool call id {id:?}"));
    }
  }
  Ok(())
}

/// OpenAI: an assistant message without tool calls has string content
/// ("Invalid value for 'content': expected a string, got null").
fn content_present(m: &Value) -> Result<(), String> {
  if calls(m).next().is_none() && m.get("content").is_none_or(Value::is_null) {
    return Err("Invalid value for 'content': expected a string, got null".into());
  }
  Ok(())
}

/// Anthropic (via OpenRouter): arguments that parse as JSON are an object
/// ("tool_use.input: Input should be a valid dictionary"). Unparseable
/// arguments were accepted.
fn object_arguments(m: &Value) -> Result<(), String> {
  for (j, c) in calls(m).enumerate() {
    let arguments = c.pointer("/function/arguments").and_then(Value::as_str);
    if let Some(Ok(parsed)) = arguments.map(serde_json::from_str::<Value>)
      && !parsed.is_object()
    {
      return Err(format!(
        "tool_calls[{j}]: arguments {parsed} is not an object (Input should be a valid \
         dictionary)"
      ));
    }
  }
  Ok(())
}

/// Anthropic (via OpenRouter): a reasoning signature sent back is one the
/// model issued, unmodified ("Invalid `signature` in `thinking` block").
/// Leaving reasoning out, sending it unsigned, or rewriting its text with
/// the signature intact were all accepted.
fn intact_signatures(m: &Value, issued: &[String]) -> Result<(), String> {
  let details = m.get("reasoning_details").and_then(Value::as_array);
  for (j, detail) in details.into_iter().flatten().enumerate() {
    if let Some(signature) = detail.get("signature").and_then(Value::as_str)
      && !issued.iter().any(|s| s == signature)
    {
      return Err(format!(
        "reasoning_details[{j}]: Invalid `signature` in `thinking` block (not one this run's \
         replies carried, or modified): {signature:?}"
      ));
    }
  }
  Ok(())
}

/// OpenAI: every tool call is answered by the `tool` messages right after
/// its assistant message, before anything else and before the request ends;
/// and every `tool` message answers a call of that assistant message.
fn tool_pairing(messages: &[Value]) -> Result<(), String> {
  let mut open: Option<(usize, Vec<String>)> = None;
  for (i, m) in messages.iter().enumerate() {
    if m["role"] == "tool" {
      let id = m["tool_call_id"].as_str().unwrap_or_default();
      let Some((_, ids)) = open.as_mut() else {
        return Err(format!(
          "messages[{i}]: messages with role 'tool' must be a response to a preceding message \
           with 'tool_calls'"
        ));
      };
      let Some(k) = ids.iter().position(|x| x == id) else {
        return Err(format!(
          "messages[{i}]: 'tool_call_id' of {id:?} not found in 'tool_calls' of previous message"
        ));
      };
      ids.remove(k);
      continue;
    }
    unanswered(&open)?;
    open = (m["role"] == "assistant").then(|| {
      (
        i,
        calls(m)
          .map(|c| c["id"].as_str().unwrap_or_default().to_owned())
          .collect(),
      )
    });
  }
  unanswered(&open)
}

fn unanswered(open: &Option<(usize, Vec<String>)>) -> Result<(), String> {
  match open {
    Some((i, ids)) if !ids.is_empty() => Err(format!(
      "messages[{i}]: an assistant message with 'tool_calls' must be followed by tool messages \
       responding to each 'tool_call_id'; unanswered: {}",
      ids.join(", ")
    )),
    _ => Ok(()),
  }
}

/// Anthropic (via OpenRouter): the request doesn't end with an assistant
/// message, text or calls ("This model does not support assistant message
/// prefill"; "`tool_use` ids were found without `tool_result` blocks").
fn ends_with_input(messages: &[Value]) -> Result<(), String> {
  match messages.last() {
    Some(m) if m["role"] == "assistant" => Err(format!(
      "messages[{}]: the conversation must end with a user or tool message, not an assistant \
       message",
      messages.len() - 1
    )),
    _ => Ok(()),
  }
}

fn calls(m: &Value) -> impl Iterator<Item = &Value> {
  m.get("tool_calls")
    .and_then(Value::as_array)
    .into_iter()
    .flatten()
}

#[cfg(test)]
mod tests {
  use serde_json::json;

  use super::*;

  fn user() -> Value {
    json!({"role": "user", "content": "hi"})
  }

  /// An assistant message calling `ids`, each with `arguments`.
  fn asst_args(ids: &[&str], arguments: &str) -> Value {
    let calls: Vec<Value> = ids
      .iter()
      .map(|id| json!({"id": id, "type": "function", "function": {"name": "f", "arguments": arguments}}))
      .collect();
    json!({"role": "assistant", "content": null, "tool_calls": calls})
  }

  fn asst(ids: &[&str]) -> Value {
    asst_args(ids, "{}")
  }

  fn tool(id: &str) -> Value {
    json!({"role": "tool", "tool_call_id": id, "content": "ok"})
  }

  fn text(s: &str) -> Value {
    json!({"role": "assistant", "content": s})
  }

  /// The calibration table, as (shape, openai_chat ok, openrouter ok).
  #[test]
  fn rules_match_the_calibration() {
    let null_text = json!({"role": "assistant", "content": null});
    let shapes: Vec<(&str, Vec<Value>, bool, bool)> = vec![
      (
        "call then result",
        vec![user(), asst(&["c1"]), tool("c1")],
        true,
        true,
      ),
      (
        "unanswered, then user",
        vec![user(), asst(&["c1"]), user()],
        false,
        true,
      ),
      (
        "unanswered at the end",
        vec![user(), asst(&["c1"])],
        false,
        false,
      ),
      (
        "one of two unanswered",
        vec![user(), asst(&["c1", "c2"]), tool("c1"), user()],
        false,
        true,
      ),
      (
        "unknown id",
        vec![user(), asst(&["c1"]), tool("c9")],
        false,
        true,
      ),
      (
        "result without a call",
        vec![user(), tool("c1")],
        false,
        true,
      ),
      (
        "duplicate ids",
        vec![user(), asst(&["c1", "c1"]), tool("c1"), tool("c1")],
        false,
        false,
      ),
      (
        "array arguments",
        vec![user(), asst_args(&["c1"], "[1]"), tool("c1")],
        true,
        false,
      ),
      (
        "non-JSON arguments",
        vec![user(), asst_args(&["c1"], "x"), tool("c1")],
        true,
        true,
      ),
      ("null content", vec![user(), null_text, user()], false, true),
      ("empty content", vec![user(), text(""), user()], true, true),
      ("trailing assistant", vec![user(), text("The")], true, false),
      (
        "results out of order",
        vec![user(), asst(&["c1", "c2"]), tool("c2"), tool("c1")],
        true,
        true,
      ),
      ("two users", vec![user(), user()], true, true),
    ];
    for (name, ms, chat, openrouter) in shapes {
      assert_eq!(
        messages(&ms, Endpoint::OpenAiChat, &[]).is_ok(),
        chat,
        "openai_chat: {name}"
      );
      assert_eq!(
        messages(&ms, Endpoint::OpenRouter, &[]).is_ok(),
        openrouter,
        "openrouter: {name}"
      );
    }
  }

  /// `calibrate/reasoning.py`'s variants, on `openrouter`.
  #[test]
  fn signatures_sent_back_must_be_intact() {
    let issued = ["sig-1".to_string()];
    let with = |details: Value| {
      let mut m = asst(&["c1"]);
      m["reasoning_details"] = details;
      vec![user(), m, tool("c1")]
    };
    let detail = |text: &str, sig: Option<&str>| {
      let mut d = json!({"type": "reasoning.text", "text": text, "index": 0});
      if let Some(sig) = sig {
        d["signature"] = json!(sig);
      }
      d
    };
    let ok = |ms: Vec<Value>| messages(&ms, Endpoint::OpenRouter, &issued).is_ok();
    assert!(
      ok(with(json!([detail("thinking", Some("sig-1"))]))),
      "intact"
    );
    assert!(
      ok(with(json!([detail("rewritten", Some("sig-1"))]))),
      "text rewritten"
    );
    assert!(ok(with(json!([detail("thinking", None)]))), "unsigned");
    assert!(ok(vec![user(), asst(&["c1"]), tool("c1")]), "no reasoning");
    assert!(
      !ok(with(json!([detail("thinking", Some("sig-X"))]))),
      "signature changed"
    );
  }

  /// `calibrate/responses.py`'s rejected shapes, and the stateless rule.
  #[test]
  fn responses_rules_match_the_calibration() {
    let user = json!({"role": "user", "content": "hi"});
    let call =
      |id: &str| json!({"type": "function_call", "call_id": id, "name": "f", "arguments": "{}"});
    let output = |id: &str| json!({"type": "function_call_output", "call_id": id, "output": "ok"});
    let reasoning = |enc: &str| json!({"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": enc});
    let issued = ["enc-1".to_string()];
    let ok = |raw: Value, items: Vec<Value>| responses(&raw, &items, &issued).is_ok();
    let raw = json!({});
    let cases: Vec<(&str, Vec<Value>, bool)> = vec![
      (
        "valid",
        vec![user.clone(), reasoning("enc-1"), call("c1"), output("c1")],
        true,
      ),
      (
        "reasoning dropped",
        vec![user.clone(), call("c1"), output("c1")],
        true,
      ),
      (
        "encrypted changed",
        vec![user.clone(), reasoning("enc-X"), call("c1"), output("c1")],
        false,
      ),
      (
        "call without output",
        vec![user.clone(), call("c1"), user.clone()],
        false,
      ),
      (
        "output without call",
        vec![user.clone(), output("c1")],
        false,
      ),
      (
        "outputs out of order",
        vec![
          user.clone(),
          call("c1"),
          call("c2"),
          output("c2"),
          output("c1"),
        ],
        true,
      ),
    ];
    for (name, items, want) in cases {
      assert_eq!(ok(raw.clone(), items), want, "{name}");
    }
    let stateful = json!({"previous_response_id": "resp_1"});
    assert!(!ok(stateful, vec![user]), "previous_response_id");
  }

  /// `calibrate/anthropic.py`'s variants (thinking enabled unless noted).
  #[test]
  fn anthropic_rules_match_the_calibration() {
    let issued = ["sig-1".to_string()];
    let user = |s: &str| json!({"role": "user", "content": s});
    let asst = |blocks: Value| json!({"role": "assistant", "content": blocks});
    let blocks = |blocks: Value| json!({"role": "user", "content": blocks});
    let th = |sig: Option<&str>| match sig {
      Some(sig) => json!({"type": "thinking", "thinking": "Hmm.", "signature": sig}),
      None => json!({"type": "thinking", "thinking": "Hmm."}),
    };
    let call =
      |id: &str, input: Value| json!({"type": "tool_use", "id": id, "name": "f", "input": input});
    let c1 = || call("c1", json!({}));
    let res = |id: &str, content: Value| json!({"type": "tool_result", "tool_use_id": id, "content": content});
    let r1 = || res("c1", json!("ok"));
    let text = |s: &str| json!({"type": "text", "text": s});
    let ask = || user("hi");
    let cases: Vec<(&str, Vec<Value>, bool)> = vec![
      (
        "valid",
        vec![
          ask(),
          asst(json!([th(Some("sig-1")), c1()])),
          blocks(json!([r1()])),
        ],
        true,
      ),
      (
        "thinking dropped",
        vec![ask(), asst(json!([c1()])), blocks(json!([r1()]))],
        true,
      ),
      (
        "thinking after the call",
        vec![
          ask(),
          asst(json!([c1(), th(Some("sig-1"))])),
          blocks(json!([r1()])),
        ],
        true,
      ),
      (
        "signature changed",
        vec![
          ask(),
          asst(json!([th(Some("sig-X")), c1()])),
          blocks(json!([r1()])),
        ],
        false,
      ),
      (
        "signature empty",
        vec![
          ask(),
          asst(json!([th(Some("")), c1()])),
          blocks(json!([r1()])),
        ],
        false,
      ),
      (
        "signature missing",
        vec![ask(), asst(json!([th(None), c1()])), blocks(json!([r1()]))],
        false,
      ),
      (
        "earlier turn: signature changed",
        vec![
          ask(),
          asst(json!([th(Some("sig-X")), c1()])),
          blocks(json!([r1()])),
          asst(json!([text("Done.")])),
          user("thanks"),
        ],
        false,
      ),
      (
        "call without result, then user",
        vec![ask(), asst(json!([c1()])), user("well?")],
        false,
      ),
      (
        "call without result at the end",
        vec![ask(), asst(json!([c1()]))],
        false,
      ),
      (
        "result for an unknown id",
        vec![
          ask(),
          asst(json!([c1()])),
          blocks(json!([res("c9", json!("ok"))])),
        ],
        false,
      ),
      (
        "result with no call",
        vec![ask(), blocks(json!([r1()]))],
        false,
      ),
      (
        "text before the result",
        vec![
          ask(),
          asst(json!([c1()])),
          blocks(json!([text("Here:"), r1()])),
        ],
        false,
      ),
      (
        "text after the result",
        vec![
          ask(),
          asst(json!([c1()])),
          blocks(json!([r1(), text("Go on.")])),
        ],
        true,
      ),
      (
        "results in separate user messages",
        vec![
          ask(),
          asst(json!([c1(), call("c2", json!({}))])),
          blocks(json!([r1()])),
          blocks(json!([res("c2", json!("ok"))])),
        ],
        true,
      ),
      (
        "text between the results",
        vec![
          ask(),
          asst(json!([c1(), call("c2", json!({}))])),
          blocks(json!([r1(), text("And:"), res("c2", json!("ok"))])),
        ],
        false,
      ),
      (
        "results out of order",
        vec![
          ask(),
          asst(json!([c1(), call("c2", json!({}))])),
          blocks(json!([res("c2", json!("ok")), r1()])),
        ],
        true,
      ),
      (
        "duplicate call ids",
        vec![
          ask(),
          asst(json!([c1(), c1()])),
          blocks(json!([r1(), r1()])),
        ],
        false,
      ),
      (
        "call id reused in a later turn",
        vec![
          ask(),
          asst(json!([c1()])),
          blocks(json!([r1()])),
          asst(json!([c1()])),
          blocks(json!([r1()])),
        ],
        true,
      ),
      (
        "input an array",
        vec![
          ask(),
          asst(json!([call("c1", json!([1]))])),
          blocks(json!([r1()])),
        ],
        false,
      ),
      (
        "trailing assistant",
        vec![ask(), asst(json!([text("Order")]))],
        false,
      ),
      ("two user messages", vec![ask(), user("again")], true),
      (
        "two assistant messages",
        vec![
          ask(),
          asst(json!([text("A")])),
          asst(json!([text("B")])),
          user("ok"),
        ],
        true,
      ),
      (
        "empty text block",
        vec![ask(), asst(json!([text("")])), user("ok")],
        false,
      ),
      (
        "empty string content",
        vec![
          ask(),
          json!({"role": "assistant", "content": ""}),
          user("ok"),
        ],
        true,
      ),
      (
        "null content",
        vec![
          ask(),
          json!({"role": "assistant", "content": null}),
          user("ok"),
        ],
        false,
      ),
      (
        "first message from the assistant",
        vec![asst(json!([text("Hi.")])), user("ok")],
        true,
      ),
      (
        "system role",
        vec![json!({"role": "system", "content": "Be brief."}), ask()],
        false,
      ),
      (
        "result text blocks",
        vec![
          ask(),
          asst(json!([c1()])),
          blocks(json!([res("c1", json!([text("ok")]))])),
        ],
        true,
      ),
      (
        "result an empty text block",
        vec![
          ask(),
          asst(json!([c1()])),
          blocks(json!([res("c1", json!([text("")]))])),
        ],
        true,
      ),
      (
        "result list of strings",
        vec![
          ask(),
          asst(json!([c1()])),
          blocks(json!([res("c1", json!(["ok"]))])),
        ],
        false,
      ),
      (
        "result a number",
        vec![
          ask(),
          asst(json!([c1()])),
          blocks(json!([res("c1", json!(4200))])),
        ],
        false,
      ),
      (
        "result an object",
        vec![
          ask(),
          asst(json!([c1()])),
          blocks(json!([res("c1", json!({"id": "42"}))])),
        ],
        false,
      ),
    ];
    for (name, messages, want) in cases {
      let raw = json!({"thinking": {"type": "adaptive"}, "messages": messages});
      assert_eq!(anthropic(&raw, &issued).is_ok(), want, "{name}");
    }
    let enabled =
      json!({"thinking": {"type": "enabled", "budget_tokens": 1024}, "messages": [ask()]});
    assert!(
      anthropic(&enabled, &issued).is_err(),
      "thinking.type enabled"
    );
  }
}
