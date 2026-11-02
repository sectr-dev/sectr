//! Handoffs (openai-agents' `handoffs=[...]`, so on openai-agents alone):
//! the conversation stays with the agent it was handed off to.
//!
//! - The transfer is journaled as the SDK stores it: its call
//!   (`transfer_to_<agent>`) and its output (`{"assistant": "<agent>"}`), as
//!   a `TOOL_CALL` and a `TOOL_RESULT`. The handed-off agent's own turn is
//!   journaled inline, like the agent's: its answer is the turn's.
//! - The next turn continues with the agent the conversation is with, and
//!   replays what it saw, the transfer included.
//! - A handoff back to the agent under test is followed the same way.

use anyhow::Result;
use serde_json::json;

use super::tools::LOOKUP_ORDER;
use crate::agent::{AgentSpec, HandoffAgent, Mcp};
use crate::case;
use crate::case::{Case, call, exec, text};
use crate::ev;
use crate::msg::{Msg, assistant, tool_call, tool_result, user};
use crate::scenario::{CaseDef, Gate, Mode, Scenario};

/// The order desk: it looks orders up, and hands back to the agent under
/// test (`handoff`, so `transfer_to_handoff`).
const ORDERS: HandoffAgent = HandoffAgent {
  name: "orders",
  instructions: "You are the order desk. Answer from your tools, in one sentence.",
  tools: &[LOOKUP_ORDER],
  back: true,
};

pub fn scenario() -> Scenario {
  Scenario {
    id: "handoff",
    agent: AgentSpec {
      instructions: "You are a shop's front desk. Hand order questions to the order desk.",
      tools: &[],
      output: None,
      mcp: Mcp::NONE,
      subagents: &[],
      handoffs: &[ORDERS],
    },
    policy: &[],
    mode: Mode::Plain,
    gate: Gate::Native,
    cases: vec![
      CaseDef::new("journaled", case!(journaled)),
      CaseDef::new("next_turn", case!(next_turn)),
      CaseDef::new("back", case!(back)),
    ],
  }
}

const ASK: &str = "Where is order 42?";
const SAY: &str = "Let me get the order desk.";
const CHECKING: &str = "Checking.";
const ANSWER: &str = "Order 42 has shipped.";
const AGAIN: &str = "Thanks. When will it arrive?";
const LATER: &str = "In two days.";

/// The SDK's transfer output (`Handoff.get_transfer_message`).
fn transfer_output(agent: &str) -> String {
  format!(r#"{{"assistant": "{agent}"}}"#)
}

fn order() -> serde_json::Value {
  json!({"order_id": "42"})
}

/// The conversation up to the order desk's first request.
fn handed_off() -> Vec<Msg> {
  vec![
    user(ASK),
    assistant(SAY),
    tool_call("h_1", "transfer_to_orders", json!({})),
    tool_result("h_1", &transfer_output("orders")),
  ]
}

/// The agent hands off; the order desk looks the order up and answers.
async fn hands_off(h: &mut Case) -> Result<Vec<Msg>> {
  h.user(ASK).await?;
  h.model()
    .await?
    .expect_history(&[user(ASK)])?
    .reply(text(SAY).and(call("h_1", "transfer_to_orders", json!({}))))
    .await?;
  let mut history = handed_off();
  h.handoff_model(&ORDERS)
    .await?
    .expect_history(&history)?
    .reply(text(CHECKING).and(call("call_1", "lookup_order", order())))
    .await?;
  h.tool(exec("lookup_order", order()).returns(json!("shipped")))
    .await?;
  history.extend([
    assistant(CHECKING),
    tool_call("call_1", "lookup_order", order()),
    tool_result("call_1", "shipped"),
  ]);
  h.handoff_model(&ORDERS)
    .await?
    .expect_history(&history)?
    .reply(text(ANSWER))
    .await?;
  history.push(assistant(ANSWER));
  Ok(history)
}

/// The journal holds the transfer call and its output, then the order
/// desk's turn.
async fn journaled(h: &mut Case) -> Result<()> {
  journaled_turn(h).await.map(drop)
}

/// [`hands_off`], and its journal checked.
async fn journaled_turn(h: &mut Case) -> Result<Vec<Msg>> {
  let history = hands_off(h).await?;
  h.journal(&[
    ev::assistant(SAY),
    ev::tool_call("h_1", "transfer_to_orders", json!({})),
    ev::tool_result("h_1", json!(transfer_output("orders"))),
    ev::assistant(CHECKING),
    ev::tool_call("call_1", "lookup_order", order()),
    ev::tool_result("call_1", json!("shipped")),
    ev::assistant(ANSWER),
    ev::turn_end(),
  ])
  .await?;
  Ok(history)
}

/// The next turn is the order desk's, and replays what it saw.
async fn next_turn(h: &mut Case) -> Result<()> {
  let mut history = journaled_turn(h).await?;
  h.strict_replay();
  h.user(AGAIN).await?;
  history.push(user(AGAIN));
  h.handoff_model(&ORDERS)
    .await?
    .expect_history(&history)?
    .reply(text(LATER))
    .await?;
  h.journal(&[ev::assistant(LATER), ev::turn_end()]).await
}

/// The order desk hands back: the rest of the turn, and the next, are the
/// agent's.
async fn back(h: &mut Case) -> Result<()> {
  let (handing_back, welcome) = ("Back to the front desk.", "Anything else?");
  h.user(ASK).await?;
  h.model()
    .await?
    .expect_history(&[user(ASK)])?
    .reply(text(SAY).and(call("h_1", "transfer_to_orders", json!({}))))
    .await?;
  let mut history = handed_off();
  h.handoff_model(&ORDERS)
    .await?
    .expect_history(&history)?
    .reply(text(handing_back).and(call("h_2", "transfer_to_handoff", json!({}))))
    .await?;
  history.extend([
    assistant(handing_back),
    tool_call("h_2", "transfer_to_handoff", json!({})),
    tool_result("h_2", &transfer_output("handoff")),
  ]);
  h.model()
    .await?
    .expect_history(&history)?
    .reply(text(welcome))
    .await?;
  history.push(assistant(welcome));
  h.journal(&[
    ev::assistant(SAY),
    ev::tool_call("h_1", "transfer_to_orders", json!({})),
    ev::tool_result("h_1", json!(transfer_output("orders"))),
    ev::assistant(handing_back),
    ev::tool_call("h_2", "transfer_to_handoff", json!({})),
    ev::tool_result("h_2", json!(transfer_output("handoff"))),
    ev::assistant(welcome),
    ev::turn_end(),
  ])
  .await?;
  h.strict_replay();
  h.user(AGAIN).await?;
  history.push(user(AGAIN));
  h.model()
    .await?
    .expect_history(&history)?
    .reply(text(LATER))
    .await?;
  h.journal(&[ev::assistant(LATER), ev::turn_end()]).await
}
