//! One tool call: the agent calls a tool, the tool runs once, the model
//! continues from its result, and later turns carry the exchange.

use anyhow::Result;
use serde_json::json;

use super::tools::LOOKUP_ORDER;
use crate::agent::{AgentSpec, Mcp};
use crate::case;
use crate::case::{Case, call, exec, text};
use crate::ev;
use crate::msg::{Msg, assistant, tool_call, tool_result, user};
use crate::scenario::{CaseDef, Scenario, both_modes};

pub fn scenarios() -> Vec<Scenario> {
  both_modes(
    "lookup",
    AgentSpec {
      instructions: "You are a customer-support agent for a small shop.",
      tools: &[LOOKUP_ORDER],
      output: None,
      mcp: Mcp::NONE,
      subagents: &[],
      handoffs: &[],
    },
    // Gated, `next_turn` rebuilds the history from a transcript holding the
    // approval request and decision, and a result from the resume path.
    |_mode| {
      vec![
        CaseDef::new("one_call", case!(one_call)),
        CaseDef::new("next_turn", case!(next_turn)),
      ]
    },
  )
}

/// The first turn's conversation, as the second model request sees it.
fn after_call() -> Vec<Msg> {
  vec![
    user("Where is order 42?"),
    tool_call("call_1", "lookup_order", json!({"order_id": "42"})),
    tool_result("call_1", "shipped"),
  ]
}

/// Ask about order 42: the model calls `lookup_order`, which returns
/// "shipped", and the model answers from it.
async fn lookup_turn(h: &mut Case) -> Result<()> {
  h.user("Where is order 42?").await?;
  h.model()
    .await?
    .expect_history(&[user("Where is order 42?")])?
    .reply(call("call_1", "lookup_order", json!({"order_id": "42"})))
    .await?;
  h.tool(exec("lookup_order", json!({"order_id": "42"})).returns(json!("shipped")))
    .await?;
  h.model()
    .await?
    .expect_history(&after_call())?
    .reply(text("Order 42 has shipped."))
    .await?;
  h.journal(&[
    ev::assistant(""),
    ev::tool_call("call_1", "lookup_order", json!({"order_id": "42"})),
    ev::tool_result("call_1", json!("shipped")),
    ev::assistant("Order 42 has shipped."),
    ev::turn_end(),
  ])
  .await
}

async fn one_call(h: &mut Case) -> Result<()> {
  lookup_turn(h).await
}

/// The next turn's request carries the tool exchange, rebuilt from the
/// journal.
async fn next_turn(h: &mut Case) -> Result<()> {
  lookup_turn(h).await?;
  h.user("Thanks!").await?;
  let mut history = after_call();
  history.extend([assistant("Order 42 has shipped."), user("Thanks!")]);
  h.model()
    .await?
    .expect_history(&history)?
    .reply(text("You're welcome."))
    .await?;
  h.journal(&[ev::assistant("You're welcome."), ev::turn_end()])
    .await
}
