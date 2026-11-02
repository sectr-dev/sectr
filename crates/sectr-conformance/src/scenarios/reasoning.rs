//! Streamed reasoning, in both modes: each model response is one assistant
//! bracket holding its reasoning (and text, if any) followed by its calls,
//! so a reasoning-only response before a call is closed before the call,
//! also when the call pauses for approval. The next request's history is
//! the conversation as the SDK sends it natively, reasoning included: in
//! the tool loop and on the next turn alike (calibrated with
//! `examples/native_history.rs`, which runs each SDK keeping its own
//! history). On OpenRouter a signature sent back must be intact, which the
//! mock checks. Each invocation replays the last one's request exactly
//! (strict replay): the reasoning the adapter journaled as framework state
//! goes back as the SDK sent it.

use anyhow::Result;
use serde_json::json;

use super::tools::LOOKUP_ORDER;
use crate::adapter::Adapter;
use crate::agent::{AgentSpec, Mcp};
use crate::case;
use crate::case::{Case, call, exec, reasoning, text};
use crate::endpoint::Endpoint;
use crate::ev;
use crate::msg::{assistant, thought, tool_call, tool_result, user};
use crate::scenario::{CaseDef, Scenario, both_modes};

/// OpenAI's Chat Completions returns no reasoning text (OpenRouter's
/// dialect of it does).
const NO_REASONING: &str = "OpenAI Chat Completions returns no reasoning";

pub fn scenarios() -> Vec<Scenario> {
  both_modes(
    "reasoning",
    AgentSpec {
      instructions: "You are a customer-support agent for a small shop.",
      tools: &[LOOKUP_ORDER],
      output: None,
      mcp: Mcp::NONE,
      subagents: &[],
      handoffs: &[],
    },
    |_| {
      vec![
        CaseDef::new("then_tool", case!(then_tool)).unsupported(Endpoint::OpenAiChat, NO_REASONING),
        CaseDef::new("then_text", case!(then_text)).unsupported(Endpoint::OpenAiChat, NO_REASONING),
        CaseDef::new("then_tool_next_turn", case!(then_tool_next_turn))
          .unsupported(Endpoint::OpenAiChat, NO_REASONING),
      ]
    },
  )
}

/// SDK-native: whether the reasoning sent back, in the tool loop and on
/// later turns, carries the provider's signature (OpenRouter's `reasoning_details` signature,
/// the Responses reasoning item's `encrypted_content`, the Anthropic
/// thinking block's signature).
fn signed(h: &Case) -> bool {
  match (h.adapter, h.endpoint) {
    // Its Chat Completions model replays only the `reasoning` text.
    (Adapter::OpenAiAgents, Endpoint::OpenRouter) => false,
    (Adapter::OpenAiAgents, Endpoint::OpenAiResponses) => true,
    (
      Adapter::LangGraph | Adapter::PydanticAi,
      Endpoint::OpenRouter | Endpoint::OpenAiResponses,
    ) => true,
    (_, Endpoint::Anthropic) => true,
    (_, Endpoint::OpenAiChat) => unreachable!("reasoning is unsupported on openai_chat"),
  }
}

const ASK: &str = "Where is order 42?";
const REASON: &str = "They want order 42's status.";

/// A response with only reasoning and a call, then the answer.
async fn then_tool(h: &mut Case) -> Result<()> {
  let (ask, reason) = (ASK, REASON);
  // Each SDK sends the response's reasoning back within the tool loop:
  // openai-agents as text only, the others signed. The gated run must do
  // the same after resuming from the journal.
  let order = json!({"order_id": "42"});
  h.strict_replay();
  h.user(ask).await?;
  h.model()
    .await?
    .expect_history(&[user(ask)])?
    .reply(reasoning(reason).and(call("call_1", "lookup_order", order.clone())))
    .await?;
  h.tool(exec("lookup_order", order.clone()).returns(json!("shipped")))
    .await?;
  h.model()
    .await?
    .expect_history(&[
      user(ask),
      thought(reason, signed(h)),
      tool_call("call_1", "lookup_order", order.clone()),
      tool_result("call_1", "shipped"),
    ])?
    .reply(text("It has shipped."))
    .await?;
  h.journal(&[
    ev::message(reason, ""),
    ev::tool_call("call_1", "lookup_order", order),
    ev::tool_result("call_1", json!("shipped")),
    ev::assistant("It has shipped."),
    ev::turn_end(),
  ])
  .await
}

/// `then_tool`, then another turn: its request sends the first turn's
/// reasoning back, as within the tool loop.
async fn then_tool_next_turn(h: &mut Case) -> Result<()> {
  then_tool(h).await?;
  let order = json!({"order_id": "42"});
  h.user("Thanks!").await?;
  h.model()
    .await?
    .expect_history(&[
      user(ASK),
      thought(REASON, signed(h)),
      tool_call("call_1", "lookup_order", order),
      tool_result("call_1", "shipped"),
      assistant("It has shipped."),
      user("Thanks!"),
    ])?
    .reply(text("You're welcome."))
    .await?;
  h.journal(&[ev::assistant("You're welcome."), ev::turn_end()])
    .await
}

/// Reasoning and text in one response; the next turn's request sends the
/// reasoning back with the text.
async fn then_text(h: &mut Case) -> Result<()> {
  h.strict_replay();
  h.user("Hi!").await?;
  h.model()
    .await?
    .expect_history(&[user("Hi!")])?
    .reply(reasoning("Just a greeting.").and(text("Hello! How can I help?")))
    .await?;
  h.journal(&[
    ev::message("Just a greeting.", "Hello! How can I help?"),
    ev::turn_end(),
  ])
  .await?;
  h.user("Nothing, thanks.").await?;
  h.model()
    .await?
    .expect_history(&[
      user("Hi!"),
      thought("Just a greeting.", signed(h)),
      assistant("Hello! How can I help?"),
      user("Nothing, thanks."),
    ])?
    .reply(text("Have a nice day."))
    .await?;
  h.journal(&[ev::assistant("Have a nice day."), ev::turn_end()])
    .await
}
