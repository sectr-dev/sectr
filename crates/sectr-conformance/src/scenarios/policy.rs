//! Deployment-policy gating (`[deploy] approval_tools`, which reaches the
//! runner as `SECTR_APPROVAL_POLICY`): a tool the code
//! doesn't gate pauses when the policy names it, native gating still holds
//! alongside a policy, and tools neither gates run freely.

use anyhow::Result;
use serde_json::json;

use super::tools::{CANCEL_ORDER, LOOKUP_ORDER, REFUND, REJECTION};
use crate::agent::{AgentSpec, Mcp};
use crate::case;
use crate::case::{Case, call, exec, text};
use crate::ev;
use crate::msg::{assistant, tool_call, tool_result, user};
use crate::scenario::{CaseDef, Gate, Mode, Scenario};

pub fn scenario() -> Scenario {
  Scenario {
    id: "policy",
    agent: AgentSpec {
      instructions: "You are a customer-support agent for a small shop.",
      tools: &[LOOKUP_ORDER, REFUND, CANCEL_ORDER],
      output: None,
      mcp: Mcp::NONE,
      subagents: &[],
      handoffs: &[],
    },
    policy: &["cancel_order"],
    mode: Mode::Plain,
    gate: Gate::Native,
    cases: vec![
      CaseDef::new("policy_gates", case!(policy_gates)),
      CaseDef::new("native_still_gates", case!(native_still_gates)),
      CaseDef::new("ungated_runs", case!(ungated_runs)),
    ],
  }
}

/// The model says `say` and calls `name` on order 42; the turn pauses.
async fn pause(h: &mut Case, ask: &str, say: &str, name: &str) -> Result<()> {
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

/// `cancel_order` is gated only by the policy: approving runs it once.
async fn policy_gates(h: &mut Case) -> Result<()> {
  let (ask, say) = ("Cancel order 42.", "Requesting the cancellation.");
  let order = json!({"order_id": "42"});
  pause(h, ask, say, "cancel_order").await?;
  h.approve().await?;
  h.tool(exec("cancel_order", order.clone()).returns(json!("Cancelled 42")))
    .await?;
  h.model()
    .await?
    .expect_history(&[
      user(ask),
      assistant(say),
      tool_call("call_1", "cancel_order", order),
      tool_result("call_1", "Cancelled 42"),
    ])?
    .reply(text("Order 42 is cancelled."))
    .await?;
  h.journal(&[
    ev::tool_result("call_1", json!("Cancelled 42")),
    ev::assistant("Order 42 is cancelled."),
    ev::turn_end(),
  ])
  .await
}

/// `refund` is gated in code; a policy that doesn't name it doesn't
/// replace that. Denied, it never runs.
async fn native_still_gates(h: &mut Case) -> Result<()> {
  let (ask, say) = ("Refund order 42.", "Requesting the refund.");
  pause(h, ask, say, "refund").await?;
  h.deny().await?;
  h.model()
    .await?
    .expect_history(&[
      user(ask),
      assistant(say),
      tool_call("call_1", "refund", json!({"order_id": "42"})),
      tool_result("call_1", REJECTION),
    ])?
    .reply(text("The refund wasn't approved."))
    .await?;
  h.journal(&[
    ev::tool_result("call_1", json!(REJECTION)),
    ev::assistant("The refund wasn't approved."),
    ev::turn_end(),
  ])
  .await
}

/// `lookup_order` is gated by neither: it runs without a pause.
async fn ungated_runs(h: &mut Case) -> Result<()> {
  let (ask, say) = ("Where is order 42?", "Let me check.");
  let order = json!({"order_id": "42"});
  h.user(ask).await?;
  h.model()
    .await?
    .expect_history(&[user(ask)])?
    .reply(text(say).and(call("call_1", "lookup_order", order.clone())))
    .await?;
  h.tool(exec("lookup_order", order.clone()).returns(json!("shipped")))
    .await?;
  h.model()
    .await?
    .expect_history(&[
      user(ask),
      assistant(say),
      tool_call("call_1", "lookup_order", order.clone()),
      tool_result("call_1", "shipped"),
    ])?
    .reply(text("It has shipped."))
    .await?;
  h.journal(&[
    ev::assistant(say),
    ev::tool_call("call_1", "lookup_order", order),
    ev::tool_result("call_1", json!("shipped")),
    ev::assistant("It has shipped."),
    ev::turn_end(),
  ])
  .await
}
