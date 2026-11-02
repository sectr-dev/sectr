//! Model requests that fail as providers fail them: an HTTP error before
//! any stream, or a stream cut short (reset, truncated, an error chunk)
//! after some text. The clients don't retry (`max_retries=0`), so the turn
//! ends with `ERROR`, any partial text stays an unclosed message, and the
//! next turn's request is valid: the fold keeps the partial text as an
//! assistant message.

use anyhow::Result;

use crate::adapter::Adapter;
use crate::agent::{AgentSpec, Mcp};
use crate::case;
use crate::case::{Case, Fault, fault, text};
use crate::endpoint::Endpoint;
use crate::ev;
use crate::msg::{Msg, assistant, user};
use crate::scenario::{CaseDef, Gate, Mode, Scenario};

const CUT_OFF_STREAM: &str = "a stream cut off by the provider is journaled as a complete answer";

pub fn scenario() -> Scenario {
  Scenario {
    id: "faults",
    agent: AgentSpec {
      instructions: "You are a helpful assistant.",
      tools: &[],
      output: None,
      mcp: Mcp::NONE,
      subagents: &[],
      handoffs: &[],
    },
    policy: &[],
    mode: Mode::Plain,
    gate: Gate::Native,
    cases: vec![
      CaseDef::new("status_500", case!(status_500)),
      CaseDef::new("status_429", case!(status_429)),
      // LiteLLM ends the stream quietly on a reset too.
      CaseDef::new("reset", case!(reset))
        .known_failure((Adapter::OpenAiAgents, Endpoint::Anthropic), CUT_OFF_STREAM),
      // Journaled as a complete answer: no ERROR, the message closed.
      CaseDef::new("truncated", case!(truncated))
        // openai-agents' Responses model fails a stream with no
        // `response.completed`; its Chat Completions and LiteLLM models
        // don't.
        .known_failure(
          (Adapter::OpenAiAgents, Endpoint::OpenAiChat),
          CUT_OFF_STREAM,
        )
        .known_failure(
          (Adapter::OpenAiAgents, Endpoint::OpenRouter),
          CUT_OFF_STREAM,
        )
        .known_failure((Adapter::OpenAiAgents, Endpoint::Anthropic), CUT_OFF_STREAM)
        .known_failure(Adapter::LangGraph, CUT_OFF_STREAM)
        .known_failure(Adapter::PydanticAi, CUT_OFF_STREAM),
      CaseDef::new("error_chunk", case!(error_chunk)),
    ],
  }
}

const ASK: &str = "What's the capital of France?";
const PARTIAL: &str = "The capital of";
const AGAIN: &str = "Are you there?";

/// The first request fails with `fault`; the turn ends errored after
/// `first`; the next turn's request is `history`.
async fn faulted(h: &mut Case, fault: Fault, partial: bool) -> Result<()> {
  let retried = retries(h, &fault);
  h.user(ASK).await?;
  h.model()
    .await?
    .expect_history(&[user(ASK)])?
    .fail(fault)
    .await?;
  if retried {
    // The retry succeeds, and the failed attempt leaves no trace.
    h.model()
      .await?
      .expect_history(&[user(ASK)])?
      .reply(text("Paris."))
      .await?;
    return h.journal(&[ev::assistant("Paris."), ev::turn_end()]).await;
  }
  let mut first = Vec::new();
  if partial {
    first.push(ev::partial(PARTIAL));
  }
  first.extend([ev::error_containing(""), ev::turn_end()]);
  h.journal(&first).await?;

  h.user(AGAIN).await?;
  let mut history: Vec<Msg> = vec![user(ASK)];
  if partial {
    history.push(assistant(PARTIAL));
  }
  history.push(user(AGAIN));
  h.model()
    .await?
    .expect_history(&history)?
    .reply(text("Yes. Paris."))
    .await?;
  h.journal(&[ev::assistant("Yes. Paris."), ev::turn_end()])
    .await
}

/// SDK-native: whether the client retries this fault despite
/// `max_retries=0`. `ChatOpenRouter` passes no retry config for 0, so the
/// `openrouter` SDK falls back to its default: retry 5xx with backoff for
/// up to an hour.
fn retries(h: &Case, fault: &Fault) -> bool {
  let five_xx = matches!(fault, Fault::Status(code) if *code >= 500);
  match (h.adapter, h.endpoint) {
    (Adapter::LangGraph, Endpoint::OpenRouter) => five_xx,
    (Adapter::LangGraph | Adapter::OpenAiAgents | Adapter::PydanticAi, _) => false,
  }
}

async fn status_500(h: &mut Case) -> Result<()> {
  faulted(h, fault::status(500), false).await
}

async fn status_429(h: &mut Case) -> Result<()> {
  faulted(h, fault::status(429), false).await
}

async fn reset(h: &mut Case) -> Result<()> {
  faulted(h, fault::reset(text(PARTIAL)), true).await
}

async fn truncated(h: &mut Case) -> Result<()> {
  faulted(h, fault::truncated(text(PARTIAL)), true).await
}

async fn error_chunk(h: &mut Case) -> Result<()> {
  faulted(h, fault::error_chunk(text(PARTIAL)), true).await
}
