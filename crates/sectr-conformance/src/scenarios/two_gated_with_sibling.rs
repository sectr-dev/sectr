//! Two gated calls and a non-gated sibling in one response: the sibling
//! runs exactly once, on the first pause turn (as in `gated_with_sibling`),
//! and the gated calls become
//! sequential approvals (as in `two_gated`), each decision applying to the
//! call that paused.

use anyhow::Result;
use serde_json::{Value, json};

use super::tools::{LOOKUP_ORDER, REFUND, REJECTION};
use crate::agent::{AgentSpec, Mcp};
use crate::case;
use crate::case::{Case, call, exec, text};
use crate::ev;
use crate::msg::{Msg, assistant, tool_call, tool_result, user};
use crate::scenario::{CaseDef, Gate, Mode, Scenario};

pub fn scenario() -> Scenario {
  Scenario {
    id: "two_gated_with_sibling",
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
      CaseDef::new("approve_both", case!(approve_both)),
      CaseDef::new("approve_then_deny", case!(approve_then_deny)),
    ],
  }
}

const ASK: &str = "Check order 9, then refund orders 1 and 2.";
const SAY: &str = "Checking order 9 and requesting both refunds.";

/// `call_1` looks up order 9; `call_2` refunds order 1, `call_3` order 2.
fn order(id: &str) -> &'static str {
  match id {
    "call_1" => "9",
    "call_2" => "1",
    _ => "2",
  }
}

fn args(id: &str) -> Value {
  json!({"order_id": order(id)})
}

fn refunded(id: &str) -> String {
  format!("Refunded {}", order(id))
}

fn other(id: &str) -> &'static str {
  if id == "call_2" { "call_3" } else { "call_2" }
}

/// Up to the second pause. Returns the gated calls in the order they
/// paused.
async fn first_approved(h: &mut Case) -> Result<(&'static str, &'static str)> {
  h.user(ASK).await?;
  h.model()
    .await?
    .expect_history(&[user(ASK)])?
    .reply(
      text(SAY)
        .and(call("call_1", "lookup_order", args("call_1")))
        .and(call("call_2", "refund", args("call_2")))
        .and(call("call_3", "refund", args("call_3"))),
    )
    .await?;
  h.tool(exec("lookup_order", args("call_1")).returns(json!("shipped")))
    .await?;
  h.journal(&[
    ev::assistant(SAY),
    ev::tool_call("call_1", "lookup_order", args("call_1")),
    ev::tool_call("call_2", "refund", args("call_2")),
    ev::tool_call("call_3", "refund", args("call_3")),
    ev::tool_result("call_1", json!("shipped")),
    ev::one_of(&[
      ev::approval_required("call_2", "refund", args("call_2")),
      ev::approval_required("call_3", "refund", args("call_3")),
    ]),
  ])
  .await?;
  let first = if h.paused()? == "call_2" {
    "call_2"
  } else {
    "call_3"
  };
  let second = other(first);

  h.approve().await?;
  h.tool(exec("refund", args(first)).returns(json!(refunded(first))))
    .await?;
  h.journal(&[
    ev::tool_result(first, json!(refunded(first))),
    ev::approval_required(second, "refund", args(second)),
  ])
  .await?;
  Ok((first, second))
}

/// The history once every call is answered (`second` with `result`), in
/// call order.
fn answered(first: &str, second: &str, result: &str) -> Vec<Msg> {
  let refund_result = |id: &str| {
    if id == second {
      result.to_string()
    } else {
      refunded(first)
    }
  };
  vec![
    user(ASK),
    assistant(SAY),
    tool_call("call_1", "lookup_order", args("call_1")),
    tool_call("call_2", "refund", args("call_2")),
    tool_call("call_3", "refund", args("call_3")),
    tool_result("call_1", "shipped"),
    tool_result("call_2", &refund_result("call_2")),
    tool_result("call_3", &refund_result("call_3")),
  ]
}

async fn approve_both(h: &mut Case) -> Result<()> {
  let (first, second) = first_approved(h).await?;
  let refunded = refunded(second);
  h.approve().await?;
  h.tool(exec("refund", args(second)).returns(json!(refunded)))
    .await?;
  h.model()
    .await?
    .expect_history(&answered(first, second, &refunded))?
    .reply(text("Order 9 has shipped, and both orders are refunded."))
    .await?;
  h.journal(&[
    ev::tool_result(second, json!(refunded)),
    ev::assistant("Order 9 has shipped, and both orders are refunded."),
    ev::turn_end(),
  ])
  .await
}

async fn approve_then_deny(h: &mut Case) -> Result<()> {
  let (first, second) = first_approved(h).await?;
  h.deny().await?;
  h.model()
    .await?
    .expect_history(&answered(first, second, REJECTION))?
    .reply(text("Order 9 has shipped; one refund went through."))
    .await?;
  h.journal(&[
    ev::tool_result(second, json!(REJECTION)),
    ev::assistant("Order 9 has shipped; one refund went through."),
    ev::turn_end(),
  ])
  .await
}
