//! A gated call with a non-gated sibling in the same response. The sibling
//! runs exactly once, on the pause turn: its result is journaled before
//! `TOOL_APPROVAL_REQUIRED`, and the gated call runs only on approval.

use anyhow::Result;
use serde_json::json;

use super::tools::{LOOKUP_ORDER, REFUND, REJECTION};
use crate::agent::{AgentSpec, Mcp};
use crate::case;
use crate::case::{Case, call, exec, text};
use crate::ev;
use crate::msg::{assistant, tool_call, tool_result, user};
use crate::scenario::{CaseDef, Gate, Mode, Scenario};

pub fn scenario() -> Scenario {
  Scenario {
    id: "gated_with_sibling",
    agent: AgentSpec {
      instructions: "You are a customer-support agent for a small shop.",
      tools: &[LOOKUP_ORDER, REFUND],
      output: None,
      mcp: Mcp::NONE,
      subagents: &[],
      handoffs: &[],
    },
    policy: &[],
    mode: Mode::Plain,
    gate: Gate::Native,
    cases: vec![
      CaseDef::new("approve", case!(approve)),
      CaseDef::new("deny", case!(deny)),
      CaseDef::new("sibling_first", case!(sibling_first)),
    ],
  }
}

const ASK: &str = "Check order 42 and refund it.";
const SAY: &str = "Checking it and requesting the refund.";

/// Up to the pause: the sibling runs, then the gated call pauses.
async fn pause(h: &mut Case) -> Result<()> {
  let order = json!({"order_id": "42"});
  h.user(ASK).await?;
  h.model()
    .await?
    .expect_history(&[user(ASK)])?
    .reply(
      text(SAY)
        .and(call("call_1", "lookup_order", order.clone()))
        .and(call("call_2", "refund", order.clone())),
    )
    .await?;
  h.tool(exec("lookup_order", order.clone()).returns(json!("shipped")))
    .await?;
  h.journal(&[
    ev::assistant(SAY),
    ev::tool_call("call_1", "lookup_order", order.clone()),
    ev::tool_call("call_2", "refund", order.clone()),
    ev::tool_result("call_1", json!("shipped")),
    ev::approval_required("call_2", "refund", order),
  ])
  .await
}

/// After the decision: the model sees both results.
async fn finish(h: &mut Case, refund_result: &str) -> Result<()> {
  let order = json!({"order_id": "42"});
  h.model()
    .await?
    .expect_history(&[
      user(ASK),
      assistant(SAY),
      tool_call("call_1", "lookup_order", order.clone()),
      tool_call("call_2", "refund", order),
      tool_result("call_1", "shipped"),
      tool_result("call_2", refund_result),
    ])?
    .reply(text("Done."))
    .await?;
  h.journal(&[
    ev::tool_result("call_2", json!(refund_result)),
    ev::assistant("Done."),
    ev::turn_end(),
  ])
  .await
}

async fn approve(h: &mut Case) -> Result<()> {
  let order = json!({"order_id": "42"});
  pause(h).await?;
  h.approve().await?;
  h.tool(exec("refund", order).returns(json!("Refunded 42")))
    .await?;
  finish(h, "Refunded 42").await
}

async fn deny(h: &mut Case) -> Result<()> {
  pause(h).await?;
  h.deny().await?;
  finish(h, REJECTION).await
}

/// The sibling finishes at once (answered ahead), before the gated call's
/// pause. It still runs exactly once: a second execution would reach the
/// case and fail it.
async fn sibling_first(h: &mut Case) -> Result<()> {
  let order = json!({"order_id": "42"});
  h.answer_ahead([exec("lookup_order", order.clone()).returns(json!("shipped"))]);
  h.user(ASK).await?;
  h.model()
    .await?
    .expect_history(&[user(ASK)])?
    .reply(
      text(SAY)
        .and(call("call_1", "lookup_order", order.clone()))
        .and(call("call_2", "refund", order.clone())),
    )
    .await?;
  h.journal(&[
    ev::assistant(SAY),
    ev::tool_call("call_1", "lookup_order", order.clone()),
    ev::tool_call("call_2", "refund", order.clone()),
    ev::tool_result("call_1", json!("shipped")),
    ev::approval_required("call_2", "refund", order.clone()),
  ])
  .await?;
  h.approve().await?;
  h.tool(exec("refund", order).returns(json!("Refunded 42")))
    .await?;
  finish(h, "Refunded 42").await
}
