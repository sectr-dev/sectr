//! MCP tools: the harness serves `track_parcel`, `parcel_details`,
//! `cancel_order` and `refund` over MCP (streamable HTTP), and the agent
//! attaches the server the framework's native way, next to a local tool. An
//! MCP tool runs like a local one; `refund` is gated in code, the
//! framework's way for MCP tools; and the deployment's policy gates
//! `cancel_order` just as it gates a local tool: the policy is the
//! platform's "deployment config alone can gate a tool" guarantee, whatever
//! provides the tool.
//!
//! An MCP result is content blocks, and optionally `structuredContent`. As
//! for any tool result (see `results.rs`), a later turn must send it exactly
//! as the first turn did, and the journal holds what the framework stores
//! (revised 2026-10-12):
//!
//! - openai-agents stores text blocks as `input_text` parts
//!   (`ToolOutputTextDict`), and langgraph as LangChain text blocks
//!   (`ToolMessage.content`). The journal holds those parts, as
//!   `{type, text}` (no LangChain block `id`): providers read a part's
//!   fields, not its text, so the worker sorting their keys is harmless.
//!   The mock model joins a result's parts, so the model-side checks see
//!   their text. Both ignore `structuredContent`, unless openai-agents'
//!   `use_structured_content` is on: then it stores `json.dumps` of it, a
//!   string ([`structured_scenario`]). langgraph keeps it in
//!   `ToolMessage.artifact`, which the model never sees.
//! - pydantic-ai has no switch: it takes `structuredContent` when every
//!   block is text, and otherwise one block's text, or several blocks as a
//!   list. Its adapter stores `model_response_str()` of that, as for any
//!   result, and every provider gets that string.

use anyhow::Result;
use serde_json::{Value, json};

use super::tools::{CANCEL_ORDER, LOOKUP_ORDER, REFUND, REJECTION};
use crate::adapter::Adapter;
use crate::agent::{AgentSpec, Mcp, Model, ToolSpec, Ty};
use crate::case;
use crate::case::{Case, call, exec, text};
use crate::ev;
use crate::mock::ToolAnswer;
use crate::msg::{assistant, tool_call, tool_result, user};
use crate::scenario::{CaseDef, Gate, Mode, Scenario};

const TRACK_PARCEL: ToolSpec = ToolSpec {
  name: "track_parcel",
  description: "Track an order's parcel.",
  params: &[("order_id", Ty::Str)],
  returns: Ty::Str,
  gated: false,
};

/// What `parcel_details` returns: the MCP server declares it as the tool's
/// output schema, so the tool may return `structuredContent`.
const PARCEL: Model = Model {
  name: "Parcel",
  fields: &[("carrier", Ty::Str), ("status", Ty::Str)],
};

const PARCEL_DETAILS: ToolSpec = ToolSpec {
  name: "parcel_details",
  description: "An order's parcel: its carrier and status.",
  params: &[("order_id", Ty::Str)],
  returns: Ty::Model(&PARCEL),
  gated: false,
};

const INSTRUCTIONS: &str = "You are a customer-support agent for a small shop.";

pub fn scenario() -> Scenario {
  Scenario {
    id: "mcp",
    agent: AgentSpec {
      instructions: INSTRUCTIONS,
      tools: &[LOOKUP_ORDER],
      output: None,
      mcp: Mcp {
        tools: &[TRACK_PARCEL, PARCEL_DETAILS, CANCEL_ORDER, REFUND],
        structured: false,
      },
      subagents: &[],
      handoffs: &[],
    },
    policy: &["cancel_order"],
    mode: Mode::Plain,
    gate: Gate::Native,
    cases: vec![
      CaseDef::new("runs", case!(runs)),
      CaseDef::new("two_blocks", case!(two_blocks)),
      CaseDef::new("structured", case!(structured)),
      CaseDef::new("policy_gates", case!(policy_gates)),
      CaseDef::new("policy_denies", case!(policy_denies)),
      CaseDef::new("native_gates", case!(native_gates)),
      CaseDef::new("native_denies", case!(native_denies)),
    ],
  }
}

/// The same server with openai-agents' `use_structured_content` on. Only
/// openai-agents has the switch; the other adapters' one behavior is
/// `mcp/structured`.
pub fn structured_scenario() -> Scenario {
  const NO_SWITCH: &str = "no switch for structured content: `mcp/structured` covers it";
  Scenario {
    id: "mcp_structured",
    agent: AgentSpec {
      instructions: INSTRUCTIONS,
      tools: &[LOOKUP_ORDER],
      output: None,
      mcp: Mcp {
        tools: &[PARCEL_DETAILS],
        structured: true,
      },
      subagents: &[],
      handoffs: &[],
    },
    policy: &[],
    mode: Mode::Plain,
    gate: Gate::Native,
    cases: vec![
      CaseDef::new("structured", case!(structured_preferred))
        .unsupported(Adapter::LangGraph, NO_SWITCH)
        .unsupported(Adapter::PydanticAi, NO_SWITCH),
    ],
  }
}

/// The model calls `name` on order 42 and the tool answers `answer`; the
/// model must get `rendered` (as the mock reads it), and the journal hold
/// `journaled`. The next turn's request replays the result as the first
/// turn sent it.
async fn returns(
  h: &mut Case,
  name: &str,
  answer: ToolAnswer,
  rendered: &str,
  journaled: Value,
) -> Result<()> {
  let (ask, say, done) = ("Where is order 42?", "Let me check.", "Here you go.");
  let order = json!({"order_id": "42"});
  h.user(ask).await?;
  h.model()
    .await?
    .expect_history(&[user(ask)])?
    .reply(text(say).and(call("call_1", name, order.clone())))
    .await?;
  h.tool(answer).await?;
  let mut history = vec![
    user(ask),
    assistant(say),
    tool_call("call_1", name, order.clone()),
    tool_result("call_1", rendered),
  ];
  h.model()
    .await?
    .expect_history(&history)?
    .reply(text(done))
    .await?;
  h.journal(&[
    ev::assistant(say),
    ev::tool_call("call_1", name, order),
    ev::tool_result("call_1", journaled),
    ev::assistant(done),
    ev::turn_end(),
  ])
  .await?;
  let (next, reply) = ("Thanks. Anything else?", "That's all.");
  h.user(next).await?;
  history.extend([assistant(done), user(next)]);
  h.model()
    .await?
    .expect_history(&history)?
    .reply(text(reply))
    .await?;
  h.journal(&[ev::assistant(reply), ev::turn_end()]).await
}

/// The parts openai-agents or langgraph stores for text blocks.
fn parts(adapter: Adapter, blocks: &[&str]) -> Value {
  let ty = match adapter {
    Adapter::OpenAiAgents => "input_text",
    _ => "text",
  };
  blocks
    .iter()
    .map(|text| json!({"type": ty, "text": text}))
    .collect()
}

fn order() -> Value {
  json!({"order_id": "42"})
}

/// `track_parcel`, which no policy names, runs without a pause: one text
/// block.
async fn runs(h: &mut Case) -> Result<()> {
  let answer = exec("track_parcel", order()).returns(json!("In transit"));
  let journaled = match h.adapter {
    Adapter::OpenAiAgents | Adapter::LangGraph => parts(h.adapter, &["In transit"]),
    Adapter::PydanticAi => json!("In transit"),
  };
  returns(h, "track_parcel", answer, "In transit", journaled).await
}

/// Two text blocks.
async fn two_blocks(h: &mut Case) -> Result<()> {
  let blocks = ["Shipped.", " Arriving Friday."];
  let (rendered, journaled) = match h.adapter {
    Adapter::OpenAiAgents | Adapter::LangGraph => (blocks.concat(), parts(h.adapter, &blocks)),
    // A list, which it renders as compact JSON.
    Adapter::PydanticAi => {
      let list = json!(blocks).to_string();
      (list.clone(), json!(list))
    }
  };
  let answer = exec("track_parcel", order()).returns_mcp(&blocks, None);
  returns(h, "track_parcel", answer, &rendered, journaled).await
}

/// `parcel_details`' fields as `structuredContent`, and (as the MCP spec
/// recommends) as JSON text too: pretty-printed, so it differs from each
/// framework's own JSON of the structured content.
fn parcel() -> (Value, String) {
  let fields = json!({"carrier": "DHL", "status": "in transit"});
  let text = serde_json::to_string_pretty(&fields).expect("serializes");
  (fields, text)
}

/// A result with `structuredContent`, the switch off.
async fn structured(h: &mut Case) -> Result<()> {
  let (fields, text) = parcel();
  let (rendered, journaled) = match h.adapter {
    // The text block.
    Adapter::OpenAiAgents | Adapter::LangGraph => (text.clone(), parts(h.adapter, &[&text])),
    // The structured content, as compact JSON.
    Adapter::PydanticAi => (fields.to_string(), json!(fields.to_string())),
  };
  let answer = exec("parcel_details", order()).returns_mcp(&[&text], Some(fields));
  returns(h, "parcel_details", answer, &rendered, journaled).await
}

/// A result with `structuredContent`, openai-agents' switch on: it stores
/// `json.dumps` of the structured content.
async fn structured_preferred(h: &mut Case) -> Result<()> {
  let (fields, text) = parcel();
  let string = r#"{"carrier": "DHL", "status": "in transit"}"#;
  let answer = exec("parcel_details", order()).returns_mcp(&[&text], Some(fields));
  returns(h, "parcel_details", answer, string, json!(string)).await
}

/// The model calls `name` on order 42; the turn pauses for approval.
async fn pause(h: &mut Case, name: &str, ask: &str, say: &str) -> Result<()> {
  let order = json!({"order_id": "42"});
  h.user(ask).await?;
  h.model()
    .await?
    .expect_history(&[user(ask)])?
    .reply(text(say).and(call("call_1", name, order.clone())))
    .await?;
  h.journal(&[
    ev::assistant(say),
    ev::tool_call("call_1", name, order.clone()),
    ev::approval_required("call_1", name, order),
  ])
  .await
}

/// `name` pauses; approving runs it once, and it returns `result`, one text
/// block, journaled as in [`runs`]. The next turn's request replays it as
/// the resume sent it.
async fn approved(h: &mut Case, name: &str, ask: &str, say: &str, result: &str) -> Result<()> {
  let order = json!({"order_id": "42"});
  let journaled = match h.adapter {
    Adapter::OpenAiAgents | Adapter::LangGraph => parts(h.adapter, &[result]),
    Adapter::PydanticAi => json!(result),
  };
  pause(h, name, ask, say).await?;
  h.approve().await?;
  h.tool(exec(name, order.clone()).returns(json!(result)))
    .await?;
  let mut history = vec![
    user(ask),
    assistant(say),
    tool_call("call_1", name, order),
    tool_result("call_1", result),
  ];
  h.model()
    .await?
    .expect_history(&history)?
    .reply(text("Done."))
    .await?;
  h.journal(&[
    ev::tool_result("call_1", journaled),
    ev::assistant("Done."),
    ev::turn_end(),
  ])
  .await?;
  let (next, reply) = ("Thanks.", "You're welcome.");
  h.user(next).await?;
  history.extend([assistant("Done."), user(next)]);
  h.model()
    .await?
    .expect_history(&history)?
    .reply(text(reply))
    .await?;
  h.journal(&[ev::assistant(reply), ev::turn_end()]).await
}

/// `name` pauses; denied, it never runs.
async fn denied(h: &mut Case, name: &str, ask: &str, say: &str) -> Result<()> {
  pause(h, name, ask, say).await?;
  h.deny().await?;
  h.model()
    .await?
    .expect_history(&[
      user(ask),
      assistant(say),
      tool_call("call_1", name, json!({"order_id": "42"})),
      tool_result("call_1", REJECTION),
    ])?
    .reply(text("That wasn't approved."))
    .await?;
  h.journal(&[
    ev::tool_result("call_1", json!(REJECTION)),
    ev::assistant("That wasn't approved."),
    ev::turn_end(),
  ])
  .await
}

const CANCEL: (&str, &str) = ("Cancel order 42.", "Requesting the cancellation.");
const REFUND_ASK: (&str, &str) = ("Refund order 42.", "Requesting the refund.");

/// `cancel_order` is gated only by the policy: approving runs it once.
async fn policy_gates(h: &mut Case) -> Result<()> {
  approved(h, "cancel_order", CANCEL.0, CANCEL.1, "Cancelled 42").await
}

/// Denied, `cancel_order` never runs.
async fn policy_denies(h: &mut Case) -> Result<()> {
  denied(h, "cancel_order", CANCEL.0, CANCEL.1).await
}

/// `refund` is gated in code: approving runs it once.
async fn native_gates(h: &mut Case) -> Result<()> {
  approved(h, "refund", REFUND_ASK.0, REFUND_ASK.1, "Refunded 42").await
}

/// Denied, `refund` never runs.
async fn native_denies(h: &mut Case) -> Result<()> {
  denied(h, "refund", REFUND_ASK.0, REFUND_ASK.1).await
}
