//! A tool raises, in both modes. What happens is the framework's native
//! behavior, defined by the plain run: the error goes back to the model as
//! the tool's result, or the turn ends in `ERROR`. The gated run (the
//! approval-resume path) must do the same. Either way the next turn's
//! request is valid.

use anyhow::Result;
use serde_json::json;

use super::tools::{LOOKUP_ORDER, OPENAI_AGENTS_TOOL_ERROR, UNFINISHED};
use crate::adapter::Adapter;
use crate::agent::{AgentSpec, Mcp};
use crate::case;
use crate::case::{Case, call, exec, text};
use crate::ev;
use crate::msg::{assistant, tool_call, tool_result, user};
use crate::scenario::{CaseDef, Mode, Scenario, both_modes};

pub fn scenarios() -> Vec<Scenario> {
  both_modes(
    "tool_raises",
    AgentSpec {
      instructions: "You are a customer-support agent for a small shop.",
      tools: &[LOOKUP_ORDER],
      output: None,
      mcp: Mcp::NONE,
      subagents: &[],
      handoffs: &[],
    },
    |_mode: Mode| vec![CaseDef::new("then_next_turn", case!(then_next_turn))],
  )
}

/// Framework-native, established by running each framework ungated: what
/// a raising tool does to the turn.
enum Native {
  /// The SDK reports the error to the model as the tool's result (this
  /// text, written by the SDK) and the model continues.
  ToModel(&'static str),
  /// The exception ends the turn with `ERROR`.
  Errors,
}

fn native(adapter: Adapter) -> Native {
  match adapter {
    // The `@function_tool` default `failure_error_function`.
    Adapter::OpenAiAgents => Native::ToModel(OPENAI_AGENTS_TOOL_ERROR),
    // A plain exception propagates out of `ToolNode` / the agent run.
    Adapter::LangGraph | Adapter::PydanticAi => Native::Errors,
  }
}

const ASK: &str = "Where is order 42?";
const SAY: &str = "Let me check.";
const BOOM: &str = "order database unavailable";

async fn then_next_turn(h: &mut Case) -> Result<()> {
  let order = json!({"order_id": "42"});
  h.user(ASK).await?;
  h.model()
    .await?
    .expect_history(&[user(ASK)])?
    .reply(text(SAY).and(call("call_1", "lookup_order", order.clone())))
    .await?;
  h.tool(exec("lookup_order", order.clone()).raises(BOOM))
    .await?;

  let mut history = vec![
    user(ASK),
    assistant(SAY),
    tool_call("call_1", "lookup_order", order.clone()),
  ];
  match native(h.adapter) {
    Native::ToModel(reported) => {
      history.push(tool_result("call_1", reported));
      h.model()
        .await?
        .expect_history(&history)?
        .reply(text("Sorry, I can't look that up right now."))
        .await?;
      h.journal(&[
        ev::assistant(SAY),
        ev::tool_call("call_1", "lookup_order", order),
        ev::tool_result("call_1", json!(reported)),
        ev::assistant("Sorry, I can't look that up right now."),
        ev::turn_end(),
      ])
      .await?;
      history.push(assistant("Sorry, I can't look that up right now."));
    }
    Native::Errors => {
      h.journal(&[
        ev::assistant(SAY),
        ev::tool_call("call_1", "lookup_order", order),
        ev::error_containing(BOOM),
        ev::turn_end(),
      ])
      .await?;
      history.push(tool_result("call_1", UNFINISHED));
    }
  }

  // The next turn's request carries the failed turn, and stays valid.
  h.user("Try again.").await?;
  history.push(user("Try again."));
  h.model()
    .await?
    .expect_history(&history)?
    .reply(text("It has shipped."))
    .await?;
  h.journal(&[ev::assistant("It has shipped."), ev::turn_end()])
    .await
}
