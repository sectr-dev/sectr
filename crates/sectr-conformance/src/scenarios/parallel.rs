//! Two calls in one response, the second completing first: each runs
//! once, the model gets the results in call order, and the journal has
//! them in completion order (as each tool finishes).

use anyhow::Result;
use serde_json::json;

use super::tools::LOOKUP_ORDER;
use crate::adapter::Adapter;
use crate::agent::{AgentSpec, Mcp};
use crate::case;
use crate::case::{Case, call, exec, text};
use crate::ev;
use crate::msg::{assistant, tool_call, tool_result, user};
use crate::scenario::{CaseDef, Gate, Mode, Scenario};

pub fn scenario() -> Scenario {
  Scenario {
    id: "parallel",
    agent: AgentSpec {
      instructions: "You are a customer-support agent for a small shop.",
      tools: &[LOOKUP_ORDER],
      output: None,
      mcp: Mcp::NONE,
      subagents: &[],
      handoffs: &[],
    },
    policy: &[],
    mode: Mode::Plain,
    gate: Gate::Native,
    cases: vec![
      CaseDef::new("two_lookups", case!(two_lookups)).known_failure(
        Adapter::OpenAiAgents,
        "parallel tool results are journaled in call order, not completion order",
      ),
    ],
  }
}

async fn two_lookups(h: &mut Case) -> Result<()> {
  let (one, two) = (json!({"order_id": "1"}), json!({"order_id": "2"}));
  h.user("Where are orders 1 and 2?").await?;
  h.model()
    .await?
    .expect_history(&[user("Where are orders 1 and 2?")])?
    .reply(
      text("Let me check both.")
        .and(call("call_1", "lookup_order", one.clone()))
        .and(call("call_2", "lookup_order", two.clone())),
    )
    .await?;
  // Both are in flight together; call 2 completes first.
  h.tools([
    exec("lookup_order", two.clone()).returns(json!("processing")),
    exec("lookup_order", one.clone()).returns(json!("shipped")),
  ])
  .await?;
  h.model()
    .await?
    .expect_history(&[
      user("Where are orders 1 and 2?"),
      assistant("Let me check both."),
      tool_call("call_1", "lookup_order", one.clone()),
      tool_call("call_2", "lookup_order", two.clone()),
      tool_result("call_1", "shipped"),
      tool_result("call_2", "processing"),
    ])?
    .reply(text("Order 1 has shipped; order 2 is processing."))
    .await?;
  h.journal(&[
    ev::assistant("Let me check both."),
    ev::tool_call("call_1", "lookup_order", one),
    ev::tool_call("call_2", "lookup_order", two),
    ev::tool_result("call_2", json!("processing")),
    ev::tool_result("call_1", json!("shipped")),
    ev::assistant("Order 1 has shipped; order 2 is processing."),
    ev::turn_end(),
  ])
  .await
}
