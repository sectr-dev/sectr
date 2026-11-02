//! Structured output isn't supported in the beta, and fails loudly: the
//! adapter rejects an agent with an output type when it wraps it, so the
//! runner never starts and no model request is made.

use anyhow::Result;

use crate::agent::{AgentSpec, Mcp, Ty};
use crate::case;
use crate::case::Case;
use crate::endpoint::Endpoint;
use crate::ev;
use crate::scenario::{CaseDef, Gate, Mode, Scenario};

pub fn scenario() -> Scenario {
  let mut rejected = CaseDef::new("rejected", case!(rejected));
  // It fails before any model request, the same on every endpoint.
  for endpoint in Endpoint::ALL {
    if endpoint != Endpoint::OpenAiChat {
      rejected = rejected.unsupported(endpoint, "fails before any model request");
    }
  }
  Scenario {
    id: "structured_output",
    agent: AgentSpec {
      instructions: "You are a helpful assistant.",
      tools: &[],
      output: Some(&[("city", Ty::Str)]),
      mcp: Mcp::NONE,
      subagents: &[],
      handoffs: &[],
    },
    policy: &[],
    mode: Mode::Plain,
    gate: Gate::Native,
    cases: vec![rejected],
  }
}

/// The session fails at startup, and its `SESSION_ERROR` says why: the
/// adapter's message, from the end of the runner's stderr.
async fn rejected(h: &mut Case) -> Result<()> {
  h.user("What's the capital of France?").await?;
  h.journal(&[
    ev::session_error_containing("structured output is not supported on Sectr yet"),
    ev::turn_end(),
  ])
  .await
}
