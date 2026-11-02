//! Hand-written journal prefixes, seeded before the first turn: the shapes
//! a crash (or another adapter) can leave. The next turn's model request
//! must be what the canonical fold (`SessionContext.messages`,
//! `sectr/context.py`) makes of them, and valid for the endpoint:
//!
//! - a message never closed keeps its partial text; if a `SESSION_ERROR`
//!   followed it (the turn died under it), a synthesized user prompt to
//!   continue comes right after it;
//! - a call with no result, followed by more conversation, is answered by
//!   the placeholder, right after its call's real results;
//! - a runner `ERROR` adds nothing.

use anyhow::Result;
use serde_json::json;

use super::tools::{LOOKUP_ORDER, UNFINISHED};
use crate::agent::{AgentSpec, Mcp};
use crate::case;
use crate::case::{Case, text};
use crate::ev;
use crate::msg::{Msg, assistant, tool_call, tool_result, user};
use crate::scenario::{CaseDef, Gate, Mode, Scenario};
use crate::seed;
use crate::seed::Seed;

pub fn scenario() -> Scenario {
  Scenario {
    id: "prefixes",
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
      CaseDef::new(
        "open_message_session_error",
        case!(open_message_session_error),
      ),
      CaseDef::new("open_message_cut", case!(open_message_cut)),
      CaseDef::new(
        "open_reasoning_session_error",
        case!(open_reasoning_session_error),
      ),
      CaseDef::new("call_without_result", case!(call_without_result)),
      CaseDef::new("call_without_message", case!(call_without_message)),
      CaseDef::new("two_calls_one_result", case!(two_calls_one_result)),
      CaseDef::new("result_then_crash", case!(result_then_crash)),
      CaseDef::new("error_mid_turn", case!(error_mid_turn)),
    ],
  }
}

const ASK: &str = "Where is order 42?";
const AGAIN: &str = "Hello? Where is it?";
/// The fold's continuation prompt after a message the turn died under
/// (`sectr/context.py`).
const CONTINUE: &str = "An error interrupted the generation of your previous message. Please continue from where it \
   left off.";

/// Seed `prefix`, ask again, and expect the next request's history to be
/// `folded` followed by the new message.
async fn after(h: &mut Case, prefix: &[Seed], folded: &[Msg]) -> Result<()> {
  h.seed(prefix).await?;
  h.user(AGAIN).await?;
  let mut history = folded.to_vec();
  history.push(user(AGAIN));
  h.model()
    .await?
    .expect_history(&history)?
    .reply(text("It has shipped."))
    .await?;
  h.journal(&[ev::assistant("It has shipped."), ev::turn_end()])
    .await
}

fn order() -> serde_json::Value {
  json!({"order_id": "42"})
}

async fn open_message_session_error(h: &mut Case) -> Result<()> {
  let prefix = [
    seed::user(ASK),
    seed::partial("Order 42 is"),
    seed::session_error("runner exited"),
    seed::turn_end(),
  ];
  let folded = [user(ASK), assistant("Order 42 is"), user(CONTINUE)];
  after(h, &prefix, &folded).await
}

/// Cut with nothing after the open message (e.g. the worker died before
/// the reconciler ended the turn): kept as is, no continuation prompt.
async fn open_message_cut(h: &mut Case) -> Result<()> {
  let prefix = [seed::user(ASK), seed::partial("Order 42 is")];
  let folded = [user(ASK), assistant("Order 42 is")];
  after(h, &prefix, &folded).await
}

/// The turn died after reasoning, before any text: the empty message
/// still gets the continuation prompt; journaled reasoning is not sent
/// back.
async fn open_reasoning_session_error(h: &mut Case) -> Result<()> {
  let prefix = [
    seed::user(ASK),
    seed::partial_reasoning("They want order 42's status."),
    seed::session_error("runner exited"),
    seed::turn_end(),
  ];
  let folded = [user(ASK), user(CONTINUE)];
  after(h, &prefix, &folded).await
}

async fn call_without_result(h: &mut Case) -> Result<()> {
  let prefix = [
    seed::user(ASK),
    seed::assistant("Checking."),
    seed::tool_call("call_1", "lookup_order", order()),
    seed::session_error("runner exited"),
    seed::turn_end(),
  ];
  let folded = [
    user(ASK),
    assistant("Checking."),
    tool_call("call_1", "lookup_order", order()),
    tool_result("call_1", UNFINISHED),
  ];
  after(h, &prefix, &folded).await
}

/// A call journaled with no assistant message before it (what
/// openai-agents used to journal for a tool-only response, so older
/// sessions hold it), and no result.
async fn call_without_message(h: &mut Case) -> Result<()> {
  let prefix = [
    seed::user(ASK),
    seed::tool_call("call_1", "lookup_order", order()),
    seed::session_error("runner exited"),
    seed::turn_end(),
  ];
  let folded = [
    user(ASK),
    tool_call("call_1", "lookup_order", order()),
    tool_result("call_1", UNFINISHED),
  ];
  after(h, &prefix, &folded).await
}

/// The placeholder goes after the call's real results, in call order.
async fn two_calls_one_result(h: &mut Case) -> Result<()> {
  let other = json!({"order_id": "43"});
  let prefix = [
    seed::user(ASK),
    seed::assistant("Checking both."),
    seed::tool_call("call_1", "lookup_order", order()),
    seed::tool_call("call_2", "lookup_order", other.clone()),
    seed::tool_result("call_1", json!("shipped")),
    seed::session_error("runner exited"),
    seed::turn_end(),
  ];
  let folded = [
    user(ASK),
    assistant("Checking both."),
    tool_call("call_1", "lookup_order", order()),
    tool_call("call_2", "lookup_order", other),
    tool_result("call_1", "shipped"),
    tool_result("call_2", UNFINISHED),
  ];
  after(h, &prefix, &folded).await
}

async fn result_then_crash(h: &mut Case) -> Result<()> {
  let prefix = [
    seed::user(ASK),
    seed::assistant("Checking."),
    seed::tool_call("call_1", "lookup_order", order()),
    seed::tool_result("call_1", json!("shipped")),
    seed::session_error("runner exited"),
    seed::turn_end(),
  ];
  let folded = [
    user(ASK),
    assistant("Checking."),
    tool_call("call_1", "lookup_order", order()),
    tool_result("call_1", "shipped"),
  ];
  after(h, &prefix, &folded).await
}

async fn error_mid_turn(h: &mut Case) -> Result<()> {
  let prefix = [
    seed::user(ASK),
    seed::assistant("Let me look that up."),
    seed::error("model provider failed"),
    seed::turn_end(),
  ];
  let folded = [user(ASK), assistant("Let me look that up.")];
  after(h, &prefix, &folded).await
}
