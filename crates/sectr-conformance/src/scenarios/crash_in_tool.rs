//! The runner dies inside a tool, after its side effect (`os._exit`). The
//! platform ends the turn (`SESSION_ERROR`), the tool never runs again, and
//! the next turn's request is valid: the fold answers the call with the
//! placeholder. After an approval, a second decision for the call is
//! refused: dev and prod share the rule (`journal::awaiting_approval`).

use anyhow::Result;
use serde_json::json;

use super::tools::{LOOKUP_ORDER, REFUND, UNFINISHED};
use crate::agent::{AgentSpec, Mcp};
use crate::case;
use crate::case::{Case, call, exec, text};
use crate::ev;
use crate::msg::{assistant, tool_call, tool_result, user};
use crate::scenario::{CaseDef, Gate, Mode, Scenario};

pub fn scenario() -> Scenario {
  Scenario {
    id: "crash_in_tool",
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
      CaseDef::new("ungated", case!(ungated)),
      CaseDef::new("after_approval", case!(after_approval)),
    ],
  }
}

const SAY: &str = "One moment.";
const AGAIN: &str = "Did that work?";

/// The next turn: the model sees the call answered by the placeholder.
async fn next_turn(h: &mut Case, ask: &str, name: &str) -> Result<()> {
  let order = json!({"order_id": "42"});
  h.user(AGAIN).await?;
  h.model()
    .await?
    .expect_history(&[
      user(ask),
      assistant(SAY),
      tool_call("call_1", name, order),
      tool_result("call_1", UNFINISHED),
      user(AGAIN),
    ])?
    .reply(text("I couldn't confirm it."))
    .await?;
  h.journal(&[ev::assistant("I couldn't confirm it."), ev::turn_end()])
    .await
}

async fn ungated(h: &mut Case) -> Result<()> {
  let (ask, order) = ("Where is order 42?", json!({"order_id": "42"}));
  h.user(ask).await?;
  h.model()
    .await?
    .expect_history(&[user(ask)])?
    .reply(text(SAY).and(call("call_1", "lookup_order", order.clone())))
    .await?;
  h.tool(exec("lookup_order", order.clone()).exits(1)).await?;
  h.journal(&[
    ev::assistant(SAY),
    ev::tool_call("call_1", "lookup_order", order),
    ev::session_error_containing(""),
    ev::turn_end(),
  ])
  .await?;
  next_turn(h, ask, "lookup_order").await
}

async fn after_approval(h: &mut Case) -> Result<()> {
  let (ask, order) = ("Refund order 42.", json!({"order_id": "42"}));
  h.user(ask).await?;
  h.model()
    .await?
    .expect_history(&[user(ask)])?
    .reply(text(SAY).and(call("call_1", "refund", order.clone())))
    .await?;
  h.journal(&[
    ev::assistant(SAY),
    ev::tool_call("call_1", "refund", order.clone()),
    ev::approval_required("call_1", "refund", order.clone()),
  ])
  .await?;
  h.approve().await?;
  h.tool(exec("refund", order).exits(1)).await?;
  h.journal(&[ev::session_error_containing(""), ev::turn_end()])
    .await?;
  // The refund ran once; approving again must not run it again.
  h.approve_expecting_refusal().await?;
  next_turn(h, ask, "refund").await
}
