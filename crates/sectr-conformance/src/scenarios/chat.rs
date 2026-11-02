//! Text-only turns: a turn completes, and the next turn's request carries
//! the previous one.

use anyhow::Result;

use crate::agent::{AgentSpec, Mcp};
use crate::case;
use crate::case::{Case, text};
use crate::ev;
use crate::msg::{assistant, user};
use crate::scenario::{CaseDef, Gate, Mode, Scenario};

pub fn scenario() -> Scenario {
  Scenario {
    id: "chat",
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
    cases: vec![CaseDef::new("two_turns", case!(two_turns))],
  }
}

async fn two_turns(h: &mut Case) -> Result<()> {
  h.user("hi").await?;
  h.model()
    .await?
    .expect_history(&[user("hi")])?
    .reply(text("Hello, there."))
    .await?;
  h.journal(&[ev::assistant("Hello, there."), ev::turn_end()])
    .await?;

  h.user("hi again").await?;
  h.model()
    .await?
    .expect_history(&[user("hi"), assistant("Hello, there."), user("hi again")])?
    .reply(text("Hello again."))
    .await?;
  h.journal(&[ev::assistant("Hello again."), ev::turn_end()])
    .await?;
  Ok(())
}
