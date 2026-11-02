//! The replay of the model's own tool calls: a later turn must send a call's
//! arguments exactly as the model wrote them, as the first turn's later
//! requests did ([`crate::replay`]; rule of 2026-10-12). The model writes
//! them as JSON text, in its own key order and spacing; the adapters
//! journal them parsed (approvals show them, the resume path runs them),
//! the journal sorts their keys, and the rebuild re-serializes them with
//! `json.dumps`.

use anyhow::Result;
use serde_json::json;

use crate::adapter::Adapter;
use crate::agent::{AgentSpec, Mcp, ToolSpec, Ty};
use crate::case;
use crate::case::{Case, call_text, exec, text};
use crate::ev;
use crate::msg::{assistant, tool_call, tool_result, user};
use crate::scenario::{CaseDef, Gate, Mode, Scenario};

const REFUND_ORDER: ToolSpec = ToolSpec {
  name: "refund_order",
  description: "Refund an order.",
  params: &[("order_id", Ty::Str), ("reason", Ty::Str)],
  returns: Ty::Str,
  gated: false,
};

const SORTED_KEYS: &str = "tool-call arguments come back with their keys sorted";

pub fn scenario() -> Scenario {
  Scenario {
    id: "replay",
    agent: AgentSpec {
      instructions: "You are a customer-support agent for a small shop.",
      tools: &[REFUND_ORDER],
      output: None,
      mcp: Mcp::NONE,
      subagents: &[],
      handoffs: &[],
    },
    policy: &[],
    // An approval resume sends the call for the first time: there's no
    // earlier request to match.
    mode: Mode::Plain,
    gate: Gate::Native,
    cases: vec![
      CaseDef::new("arguments", case!(arguments))
        .known_failure(Adapter::OpenAiAgents, SORTED_KEYS)
        .known_failure(Adapter::LangGraph, SORTED_KEYS)
        .known_failure(Adapter::PydanticAi, SORTED_KEYS),
    ],
  }
}

/// The model writes the arguments compact, its keys not in alphabetical
/// order; the next turn must send that text unchanged.
async fn arguments(h: &mut Case) -> Result<()> {
  h.strict_replay();
  const WRITTEN: &str = r#"{"reason":"late","order_id":"42"}"#;
  let args = json!({"order_id": "42", "reason": "late"});
  let (ask, say, answer) = ("Refund order 42, it came late.", "Refunding.", "Refunded.");
  h.user(ask).await?;
  h.model()
    .await?
    .expect_history(&[user(ask)])?
    .reply(text(say).and(call_text("call_1", "refund_order", WRITTEN)))
    .await?;
  h.tool(exec("refund_order", args.clone()).returns(json!("Refunded 42")))
    .await?;
  let mut history = vec![
    user(ask),
    assistant(say),
    tool_call("call_1", "refund_order", args.clone()),
    tool_result("call_1", "Refunded 42"),
  ];
  h.model()
    .await?
    .expect_history(&history)?
    .reply(text(answer))
    .await?;
  h.journal(&[
    ev::assistant(say),
    ev::tool_call("call_1", "refund_order", args),
    ev::tool_result("call_1", json!("Refunded 42")),
    ev::assistant(answer),
    ev::turn_end(),
  ])
  .await?;
  let (next, reply) = ("Thanks.", "You're welcome.");
  h.user(next).await?;
  history.extend([assistant(answer), user(next)]);
  h.model()
    .await?
    .expect_history(&history)?
    .reply(text(reply))
    .await?;
  h.journal(&[ev::assistant(reply), ev::turn_end()]).await
}
