//! A natively gated tool: the call pauses the turn, then an approval runs
//! it exactly once and a denial never does. Either way the model continues
//! from the call's result. Also in a later turn (the call goes after that
//! turn's user message), and twice in a row (a second call after an
//! approval pauses again).

use anyhow::Result;
use serde_json::json;

use super::tools::{REFUND, REJECTION};
use crate::agent::{AgentSpec, Mcp};
use crate::case;
use crate::case::{Case, call, exec, text};
use crate::ev;
use crate::msg::{Msg, assistant, tool_call, tool_result, user};
use crate::scenario::{CaseDef, Gate, Mode, Scenario};

pub fn scenario() -> Scenario {
  Scenario {
    id: "gated",
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
      CaseDef::new("approve", case!(approve)),
      CaseDef::new("deny", case!(deny)),
      // A tool-only call in a later turn.
      CaseDef::new("later_turn_approve", case!(later_turn_approve)),
      CaseDef::new("later_turn_deny", case!(later_turn_deny)),
      CaseDef::new("second_call", case!(second_call)),
    ],
  }
}

/// Ask for a refund: the model calls `refund`, and the turn suspends on
/// the approval without running it.
async fn pause(h: &mut Case) -> Result<()> {
  h.user("Refund order 42.").await?;
  h.model()
    .await?
    .expect_history(&[user("Refund order 42.")])?
    .reply(call("call_1", "refund", json!({"order_id": "42"})))
    .await?;
  h.journal(&[
    ev::assistant(""),
    ev::tool_call("call_1", "refund", json!({"order_id": "42"})),
    ev::approval_required("call_1", "refund", json!({"order_id": "42"})),
  ])
  .await
}

async fn approve(h: &mut Case) -> Result<()> {
  pause(h).await?;
  h.approve().await?;
  h.tool(exec("refund", json!({"order_id": "42"})).returns(json!("Refunded 42")))
    .await?;
  h.model()
    .await?
    .expect_history(&[
      user("Refund order 42."),
      tool_call("call_1", "refund", json!({"order_id": "42"})),
      tool_result("call_1", "Refunded 42"),
    ])?
    .reply(text("Order 42 is refunded."))
    .await?;
  h.journal(&[
    ev::tool_result("call_1", json!("Refunded 42")),
    ev::assistant("Order 42 is refunded."),
    ev::turn_end(),
  ])
  .await
}

async fn deny(h: &mut Case) -> Result<()> {
  pause(h).await?;
  h.deny().await?;
  h.model()
    .await?
    .expect_history(&[
      user("Refund order 42."),
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

const HI: &str = "Hi, I have a question about an order.";
const HELLO: &str = "Sure, which order?";
const LATER: &str = "Order 42. Please refund it.";

/// A text-only first turn, then a tool-only call in the second turn: the
/// call (and its empty message) belongs after the second turn's user
/// message, not on the first turn's answer.
async fn later_turn_pause(h: &mut Case) -> Result<()> {
  h.user(HI).await?;
  h.model()
    .await?
    .expect_history(&[user(HI)])?
    .reply(text(HELLO))
    .await?;
  h.journal(&[ev::assistant(HELLO), ev::turn_end()]).await?;
  h.user(LATER).await?;
  h.model()
    .await?
    .expect_history(&[user(HI), assistant(HELLO), user(LATER)])?
    .reply(call("call_1", "refund", json!({"order_id": "42"})))
    .await?;
  h.journal(&[
    ev::assistant(""),
    ev::tool_call("call_1", "refund", json!({"order_id": "42"})),
    ev::approval_required("call_1", "refund", json!({"order_id": "42"})),
  ])
  .await
}

/// The resume's request, the call answered with `result`.
fn later_turn_history(result: &str) -> Vec<Msg> {
  vec![
    user(HI),
    assistant(HELLO),
    user(LATER),
    tool_call("call_1", "refund", json!({"order_id": "42"})),
    tool_result("call_1", result),
  ]
}

async fn later_turn_approve(h: &mut Case) -> Result<()> {
  later_turn_pause(h).await?;
  h.approve().await?;
  h.tool(exec("refund", json!({"order_id": "42"})).returns(json!("Refunded 42")))
    .await?;
  h.model()
    .await?
    .expect_history(&later_turn_history("Refunded 42"))?
    .reply(text("Order 42 is refunded."))
    .await?;
  h.journal(&[
    ev::tool_result("call_1", json!("Refunded 42")),
    ev::assistant("Order 42 is refunded."),
    ev::turn_end(),
  ])
  .await
}

async fn later_turn_deny(h: &mut Case) -> Result<()> {
  later_turn_pause(h).await?;
  h.deny().await?;
  h.model()
    .await?
    .expect_history(&later_turn_history(REJECTION))?
    .reply(text("The refund wasn't approved."))
    .await?;
  h.journal(&[
    ev::tool_result("call_1", json!(REJECTION)),
    ev::assistant("The refund wasn't approved."),
    ev::turn_end(),
  ])
  .await
}

/// After an approved call, the model calls the tool again (a second
/// response, not a second call in one response): the resumed turn pauses
/// again, and the second decision (a denial) applies to the second call.
async fn second_call(h: &mut Case) -> Result<()> {
  let (ask, first_say, second_say) = (
    "Refund orders 1 and 2, one at a time.",
    "Refunding order 1 first.",
    "Now order 2.",
  );
  let (one, two) = (json!({"order_id": "1"}), json!({"order_id": "2"}));
  h.user(ask).await?;
  h.model()
    .await?
    .expect_history(&[user(ask)])?
    .reply(text(first_say).and(call("call_1", "refund", one.clone())))
    .await?;
  h.journal(&[
    ev::assistant(first_say),
    ev::tool_call("call_1", "refund", one.clone()),
    ev::approval_required("call_1", "refund", one.clone()),
  ])
  .await?;

  h.approve().await?;
  h.tool(exec("refund", one.clone()).returns(json!("Refunded 1")))
    .await?;
  let mut history = vec![
    user(ask),
    assistant(first_say),
    tool_call("call_1", "refund", one),
    tool_result("call_1", "Refunded 1"),
  ];
  h.model()
    .await?
    .expect_history(&history)?
    .reply(text(second_say).and(call("call_2", "refund", two.clone())))
    .await?;
  h.journal(&[
    ev::tool_result("call_1", json!("Refunded 1")),
    ev::assistant(second_say),
    ev::tool_call("call_2", "refund", two.clone()),
    ev::approval_required("call_2", "refund", two.clone()),
  ])
  .await?;

  h.deny().await?;
  history.extend([
    assistant(second_say),
    tool_call("call_2", "refund", two),
    tool_result("call_2", REJECTION),
  ]);
  h.model()
    .await?
    .expect_history(&history)?
    .reply(text("Order 1 is refunded; order 2 wasn't approved."))
    .await?;
  h.journal(&[
    ev::tool_result("call_2", json!(REJECTION)),
    ev::assistant("Order 1 is refunded; order 2 wasn't approved."),
    ev::turn_end(),
  ])
  .await
}
