//! Two gated calls in one response become sequential approvals: the turn
//! pauses on one, the decision resumes it and it pauses on the other. The
//! framework decides which pauses first; each decision applies to the call
//! that paused, and no call runs without its own approval.

use anyhow::Result;
use serde_json::{Value, json};

use super::tools::{REFUND, REJECTION};
use crate::agent::{AgentSpec, Mcp};
use crate::case;
use crate::case::{Case, call, exec, text};
use crate::ev;
use crate::msg::{Msg, assistant, tool_call, tool_result, user};
use crate::scenario::{CaseDef, Gate, Mode, Scenario};

pub fn scenario() -> Scenario {
  Scenario {
    id: "two_gated",
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
    cases: vec![
      CaseDef::new("approve_both", case!(approve_both)),
      CaseDef::new("approve_then_deny", case!(approve_then_deny)),
    ],
  }
}

const ASK: &str = "Refund orders 1 and 2.";
const SAY: &str = "I'll request both refunds.";

/// `call_1` refunds order 1, `call_2` order 2.
fn order(id: &str) -> &'static str {
  if id == "call_1" { "1" } else { "2" }
}

fn args(id: &str) -> Value {
  json!({"order_id": order(id)})
}

fn refunded(id: &str) -> String {
  format!("Refunded {}", order(id))
}

fn other(id: &str) -> &'static str {
  if id == "call_1" { "call_2" } else { "call_1" }
}

/// The model asks for both refunds; the turn pauses on one of them, and
/// approving it runs it and pauses on the other. Returns the call ids in
/// the order they paused.
async fn first_approved(h: &mut Case) -> Result<(&'static str, &'static str)> {
  h.user(ASK).await?;
  h.model()
    .await?
    .expect_history(&[user(ASK)])?
    .reply(
      text(SAY)
        .and(call("call_1", "refund", args("call_1")))
        .and(call("call_2", "refund", args("call_2"))),
    )
    .await?;
  h.journal(&[
    ev::assistant(SAY),
    ev::tool_call("call_1", "refund", args("call_1")),
    ev::tool_call("call_2", "refund", args("call_2")),
    ev::one_of(&[
      ev::approval_required("call_1", "refund", args("call_1")),
      ev::approval_required("call_2", "refund", args("call_2")),
    ]),
  ])
  .await?;
  let first = if h.paused()? == "call_1" {
    "call_1"
  } else {
    "call_2"
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

/// The history once both calls are answered (`second` with `result`): the
/// results in call order, whichever was answered first.
fn answered(first: &str, second: &str, result: &str) -> Vec<Msg> {
  let mut history = vec![
    user(ASK),
    assistant(SAY),
    tool_call("call_1", "refund", args("call_1")),
    tool_call("call_2", "refund", args("call_2")),
  ];
  let first_result = refunded(first);
  history.extend(
    ["call_1", "call_2"]
      .map(|id| tool_result(id, if id == second { result } else { &first_result })),
  );
  history
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
    .reply(text("Both orders are refunded."))
    .await?;
  h.journal(&[
    ev::tool_result(second, json!(refunded)),
    ev::assistant("Both orders are refunded."),
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
    .reply(text("One refund went through; the other wasn't approved."))
    .await?;
  h.journal(&[
    ev::tool_result(second, json!(REJECTION)),
    ev::assistant("One refund went through; the other wasn't approved."),
    ev::turn_end(),
  ])
  .await
}
