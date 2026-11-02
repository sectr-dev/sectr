//! A user message while a gated call is paused abandons the call
//! (`schema/README.md`, "every prefix of a valid journal is a valid
//! journal"): the model sees it answered by the placeholder, the call
//! never runs, and a late decision is refused.

use anyhow::Result;
use serde_json::json;

use super::tools::{REFUND, UNFINISHED};
use crate::agent::{AgentSpec, Mcp};
use crate::case;
use crate::case::{Case, call, text};
use crate::ev;
use crate::msg::{assistant, tool_call, tool_result, user};
use crate::scenario::{CaseDef, Gate, Mode, Scenario};

pub fn scenario() -> Scenario {
  Scenario {
    id: "abandon",
    agent: AgentSpec {
      instructions: "You are a customer-support agent for a small shop.",
      tools: &[REFUND],
      output: None,
      mcp: Mcp::NONE,
      subagents: &[],
      handoffs: &[],
    },
    policy: &[],
    mode: Mode::Plain,
    gate: Gate::Native,
    cases: vec![CaseDef::new(
      "then_late_approval",
      case!(then_late_approval),
    )],
  }
}

async fn then_late_approval(h: &mut Case) -> Result<()> {
  let (ask, say, order) = (
    "Refund order 42.",
    "Requesting the refund.",
    json!({"order_id": "42"}),
  );
  h.user(ask).await?;
  h.model()
    .await?
    .expect_history(&[user(ask)])?
    .reply(text(say).and(call("call_1", "refund", order.clone())))
    .await?;
  h.journal(&[
    ev::assistant(say),
    ev::tool_call("call_1", "refund", order.clone()),
    ev::approval_required("call_1", "refund", order.clone()),
  ])
  .await?;

  let never_mind = "Never mind, keep the order.";
  h.user(never_mind).await?;
  h.model()
    .await?
    .expect_history(&[
      user(ask),
      assistant(say),
      tool_call("call_1", "refund", order),
      tool_result("call_1", UNFINISHED),
      user(never_mind),
    ])?
    .reply(text("Okay, no refund."))
    .await?;
  h.journal(&[ev::assistant("Okay, no refund."), ev::turn_end()])
    .await?;
  h.approve_expecting_refusal().await
}
