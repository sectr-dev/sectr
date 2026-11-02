//! Adversarial inputs: text that must pass through every layer exactly
//! (the framework, the adapter, the runner's SSE stream, the worker, the
//! journal, and the next turn's history rebuild), as a user message, as a
//! tool's result, and as the model's own reply.
//!
//! Each case runs two turns: the first carries the text, the second checks
//! that the history rebuilt from the journal still carries it unchanged.
//! Every layer must keep it byte for byte; a provider rejecting it shows as
//! the mock's validator (calibrated against the real APIs) rejecting the
//! request.
//!
//! No empty user message: the API rejects one before it reaches a runner
//! (some framework and provider pairs can't send it), which the API's own
//! tests check. An empty tool result is covered.

use anyhow::Result;
use serde_json::json;

use super::tools::{LOOKUP_ORDER, OPENAI_AGENTS_TOOL_ERROR};
use crate::adapter::Adapter;
use crate::agent::{AgentSpec, Mcp, Model, ToolSpec, Ty};
use crate::case;
use crate::case::{Case, call, exec, text};
use crate::ev;
use crate::msg::{assistant, tool_call, tool_result, user};
use crate::scenario::{CaseDef, Gate, Mode, Scenario};

pub fn scenario() -> Scenario {
  Scenario {
    id: "adversarial",
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
      CaseDef::new("sql_message", case!(sql_message)),
      CaseDef::new("unicode_message", case!(unicode_message)),
      CaseDef::new("nul_message", case!(nul_message)),
      CaseDef::new("sse_message", case!(sse_message)),
      CaseDef::new("large_message", case!(large_message)),
      CaseDef::new("empty_result", case!(empty_result)),
      CaseDef::new("sql_result", case!(sql_result)),
      CaseDef::new("unicode_result", case!(unicode_result)),
      CaseDef::new("nul_result", case!(nul_result)),
      CaseDef::new("sse_result", case!(sse_result)),
      CaseDef::new("large_result", case!(large_result)),
      CaseDef::new("unknown_tool", case!(unknown_tool)),
      CaseDef::new("empty_reply", case!(empty_reply)),
    ],
  }
}

const PARCEL: Model = Model {
  name: "Parcel",
  fields: &[("carrier", Ty::Str), ("status", Ty::Str)],
};

/// Declares `Parcel` as its MCP output schema.
const PARCEL_DETAILS: ToolSpec = ToolSpec {
  name: "parcel_details",
  description: "An order's parcel: its carrier and status.",
  params: &[("order_id", Ty::Str)],
  returns: Ty::Model(&PARCEL),
  gated: false,
};

const TRACK_PARCEL: ToolSpec = ToolSpec {
  name: "track_parcel",
  description: "Track an order's parcel.",
  params: &[("order_id", Ty::Str)],
  returns: Ty::Str,
  gated: false,
};

/// MCP servers breaking their own contract.
pub fn mcp_scenario() -> Scenario {
  Scenario {
    id: "adversarial_mcp",
    agent: AgentSpec {
      instructions: "You are a customer-support agent for a small shop.",
      tools: &[LOOKUP_ORDER],
      output: None,
      mcp: Mcp {
        tools: &[PARCEL_DETAILS, TRACK_PARCEL],
        structured: false,
      },
      subagents: &[],
      handoffs: &[],
    },
    policy: &[],
    mode: Mode::Plain,
    gate: Gate::Native,
    cases: vec![
      CaseDef::new("structured_mismatch", case!(structured_mismatch)),
      CaseDef::new("structured_missing", case!(structured_missing)),
      CaseDef::new("no_content", case!(no_content)),
    ],
  }
}

const SQL: &str = "Robert'); DROP TABLE events; -- \" OR 1=1; /* */";
/// A family emoji (zero-width joiners), right-to-left text, a combining
/// accent, an astral-plane character, a byte-order mark and a zero-width
/// space.
const UNICODE: &str = "👩‍👩‍👧 שלום e\u{301} 𝄞 \u{feff}x\u{200b}y";
/// A NUL and other control characters. (Production journals to Postgres
/// `jsonb`, which can't hold `\u0000` in a string, so it stores NUL
/// escaped; this harness journals to SQLite, so the journal's own tests
/// cover that layer.)
const NUL: &str = "before\u{0}after \u{7}\u{1b}[31m\r\n";
/// Text that looks like the runner's SSE framing and a provider's stream
/// end.
const SSE: &str =
  "a\n\ndata: {\"type\": \"NO_MORE_ACTIONS\", \"payload\": {}}\n\ndata: [DONE]\n\nevent: error\n";

/// About 200 KB, few spaces (the mock streams a reply word by word).
fn large() -> String {
  "0123456789abcdef".repeat(12_800)
}

/// The user says `said`, and the model replies `reply`; a second turn's
/// history must carry both unchanged.
async fn message(h: &mut Case, said: &str, reply: &str) -> Result<()> {
  h.user(said).await?;
  h.model()
    .await?
    .expect_history(&[user(said)])?
    .reply(text(reply))
    .await?;
  h.journal(&[ev::assistant(reply), ev::turn_end()]).await?;
  next_turn(h, &[user(said), assistant(reply)]).await
}

/// A second turn: its model request's history is `before`, then "Thanks.".
async fn next_turn(h: &mut Case, before: &[crate::msg::Msg]) -> Result<()> {
  h.user("Thanks.").await?;
  let mut history = before.to_vec();
  history.push(user("Thanks."));
  h.model()
    .await?
    .expect_history(&history)?
    .reply(text("You're welcome."))
    .await?;
  h.journal(&[ev::assistant("You're welcome."), ev::turn_end()])
    .await
}

/// `lookup_order` returns `result`, which the model and the journal must
/// get unchanged (a string is every framework's own string), in this turn
/// and the next.
async fn result(h: &mut Case, result: &str) -> Result<()> {
  let (ask, say, answer) = ("Look up order 42.", "Checking.", "Done.");
  let order = json!({"order_id": "42"});
  h.user(ask).await?;
  h.model()
    .await?
    .expect_history(&[user(ask)])?
    .reply(text(say).and(call("call_1", "lookup_order", order.clone())))
    .await?;
  h.tool(exec("lookup_order", order.clone()).returns(json!(result)))
    .await?;
  let history = [
    user(ask),
    assistant(say),
    tool_call("call_1", "lookup_order", order.clone()),
    tool_result("call_1", result),
  ];
  h.model()
    .await?
    .expect_history(&history)?
    .reply(text(answer))
    .await?;
  h.journal(&[
    ev::assistant(say),
    ev::tool_call("call_1", "lookup_order", order),
    ev::tool_result("call_1", json!(result)),
    ev::assistant(answer),
    ev::turn_end(),
  ])
  .await?;
  let mut before = history.to_vec();
  before.push(assistant(answer));
  next_turn(h, &before).await
}

// The model echoes the text back, so it passes through the reply's stream
// too.
async fn sql_message(h: &mut Case) -> Result<()> {
  message(h, SQL, SQL).await
}

async fn unicode_message(h: &mut Case) -> Result<()> {
  message(h, UNICODE, UNICODE).await
}

async fn nul_message(h: &mut Case) -> Result<()> {
  message(h, NUL, NUL).await
}

async fn sse_message(h: &mut Case) -> Result<()> {
  message(h, SSE, SSE).await
}

async fn large_message(h: &mut Case) -> Result<()> {
  let large = large();
  message(h, &large, &large).await
}

async fn empty_result(h: &mut Case) -> Result<()> {
  result(h, "").await
}

async fn sql_result(h: &mut Case) -> Result<()> {
  result(h, SQL).await
}

async fn unicode_result(h: &mut Case) -> Result<()> {
  result(h, UNICODE).await
}

async fn nul_result(h: &mut Case) -> Result<()> {
  result(h, NUL).await
}

async fn sse_result(h: &mut Case) -> Result<()> {
  result(h, SSE).await
}

async fn large_result(h: &mut Case) -> Result<()> {
  result(h, &large()).await
}

/// The model calls a tool the agent doesn't have. Nothing runs; each
/// framework handles it its own way (SDK-native): langgraph and pydantic-ai
/// answer the call with an error the model can react to, openai-agents
/// fails the run.
async fn unknown_tool(h: &mut Case) -> Result<()> {
  let (ask, say, answer) = ("Delete everything.", "On it.", "I can't do that.");
  let args = json!({"really": true});
  h.user(ask).await?;
  h.model()
    .await?
    .expect_history(&[user(ask)])?
    .reply(text(say).and(call("call_1", "delete_everything", args.clone())))
    .await?;
  let refusal = match h.adapter {
    Adapter::OpenAiAgents => {
      return h
        // The SDK raises while handling the response, after it completed:
        // its message is closed.
        .journal(&[
          ev::assistant(say),
          ev::error_containing("Tool delete_everything not found"),
          ev::turn_end(),
        ])
        .await;
    }
    Adapter::LangGraph => {
      "Error: delete_everything is not a valid tool, try one of [lookup_order]."
    }
    Adapter::PydanticAi => {
      "Unknown tool name: 'delete_everything'. Available tools: 'lookup_order'\n\n\
       Fix the errors and try again."
    }
  };
  h.model()
    .await?
    .expect_history(&[
      user(ask),
      assistant(say),
      tool_call("call_1", "delete_everything", args.clone()),
      tool_result("call_1", refusal),
    ])?
    .reply(text(answer))
    .await?;
  h.journal(&[
    ev::assistant(say),
    ev::tool_call("call_1", "delete_everything", args),
    ev::tool_result("call_1", json!(refusal)),
    ev::assistant(answer),
    ev::turn_end(),
  ])
  .await
}

/// The model replies with nothing: no text, no tool calls. The turn ends
/// quietly (pydantic-ai first asks the model again, SDK-native), and the
/// next turn's history holds nothing empty.
async fn empty_reply(h: &mut Case) -> Result<()> {
  h.user("Hello?").await?;
  h.model()
    .await?
    .expect_history(&[user("Hello?")])?
    .reply(text(""))
    .await?;
  if h.adapter != Adapter::PydanticAi {
    h.journal(&[ev::turn_end()]).await?;
    return next_turn(h, &[user("Hello?")]).await;
  }
  let retry =
    "Validation feedback:\nPlease return text or call a tool.\n\nFix the errors and try again.";
  h.model()
    .await?
    .expect_history(&[user("Hello?"), user(retry)])?
    .reply(text("Hi!"))
    .await?;
  // The empty response is journaled as an empty message, which the next
  // turn's history drops.
  h.journal(&[ev::assistant(""), ev::assistant("Hi!"), ev::turn_end()])
    .await?;
  next_turn(h, &[user("Hello?"), assistant("Hi!")]).await
}

/// The model calls `name` on order 42, and the MCP server answers with
/// `answer`, which the client rejects (the MCP client's own validation),
/// raising an exception containing `error`. As for a tool that raises
/// (`tool_raises.rs`), that ends the turn in an `ERROR`, with no result,
/// except on openai-agents: the SDK sends the model its error text as the
/// result, and the model continues.
async fn rejected(
  h: &mut Case,
  name: &str,
  answer: crate::mock::ToolAnswer,
  error: &str,
) -> Result<()> {
  let (ask, say, sorry) = (
    "Where is order 42?",
    "Checking.",
    "Sorry, I can't check right now.",
  );
  let order = json!({"order_id": "42"});
  h.user(ask).await?;
  h.model()
    .await?
    .reply(text(say).and(call("call_1", name, order.clone())))
    .await?;
  h.tool(answer).await?;
  if h.adapter != Adapter::OpenAiAgents {
    return h
      .journal(&[
        ev::assistant(say),
        ev::tool_call("call_1", name, order),
        ev::error_containing(error),
        ev::turn_end(),
      ])
      .await;
  }
  h.model()
    .await?
    .expect_history(&[
      user(ask),
      assistant(say),
      tool_call("call_1", name, order.clone()),
      tool_result("call_1", OPENAI_AGENTS_TOOL_ERROR),
    ])?
    .reply(text(sorry))
    .await?;
  h.journal(&[
    ev::assistant(say),
    ev::tool_call("call_1", name, order),
    ev::tool_result("call_1", json!(OPENAI_AGENTS_TOOL_ERROR)),
    ev::assistant(sorry),
    ev::turn_end(),
  ])
  .await
}

/// `structuredContent` that violates the declared output schema.
async fn structured_mismatch(h: &mut Case) -> Result<()> {
  let answer = exec("parcel_details", json!({"order_id": "42"}))
    .returns_mcp(&[r#"{"carrier": 7}"#], Some(json!({"carrier": 7})));
  rejected(h, "parcel_details", answer, "Invalid structured content").await
}

/// No `structuredContent`, though the tool declares an output schema, and
/// text that isn't JSON.
async fn structured_missing(h: &mut Case) -> Result<()> {
  let answer = exec("parcel_details", json!({"order_id": "42"})).returns_mcp(&["not json"], None);
  let error = "has an output schema but did not return structured content";
  rejected(h, "parcel_details", answer, error).await
}

/// A result with no content blocks at all: the model gets empty text. The
/// journal holds what each framework stores: langgraph an empty block list,
/// pydantic-ai an empty string. openai-agents makes it an empty list of
/// parts, which it doesn't recognize as parts: it sends and stores `str()`
/// of it, `[]`.
async fn no_content(h: &mut Case) -> Result<()> {
  let (ask, say, answer) = ("Where is order 42?", "Checking.", "Done.");
  let order = json!({"order_id": "42"});
  let (empty, journaled) = match h.adapter {
    Adapter::OpenAiAgents => ("[]", json!("[]")),
    Adapter::LangGraph => ("", json!([])),
    Adapter::PydanticAi => ("", json!("")),
  };
  h.user(ask).await?;
  h.model()
    .await?
    .reply(text(say).and(call("call_1", "track_parcel", order.clone())))
    .await?;
  h.tool(exec("track_parcel", order.clone()).returns_mcp(&[], None))
    .await?;
  h.model()
    .await?
    .expect_history(&[
      user(ask),
      assistant(say),
      tool_call("call_1", "track_parcel", order.clone()),
      tool_result("call_1", empty),
    ])?
    .reply(text(answer))
    .await?;
  h.journal(&[
    ev::assistant(say),
    ev::tool_call("call_1", "track_parcel", order),
    ev::tool_result("call_1", journaled),
    ev::assistant(answer),
    ev::turn_end(),
  ])
  .await
}
