//! Scenarios: an agent plus the cases run against it, and how each adapter
//! or endpoint departs from running every case as written.

use anyhow::Result;
use futures::future::BoxFuture;

use crate::adapter::Adapter;
use crate::agent::{AgentSpec, ToolSpec};
use crate::case::Case;
use crate::endpoint::Endpoint;

pub type CaseFn = for<'a> fn(&'a mut Case) -> BoxFuture<'a, Result<()>>;

pub struct Scenario {
  /// Names the generated project (`NN_<id>`, numbered by catalog order).
  pub id: &'static str,
  pub agent: AgentSpec,
  /// The deployment's approval policy: tool names gated by config, not by
  /// the agent's code. Not compiled; passed as `sectr dev` passes
  /// `[deploy] approval_tools`, so the runner gets it as
  /// `SECTR_APPROVAL_POLICY`.
  pub policy: &'static [&'static str],
  pub mode: Mode,
  pub gate: Gate,
  pub cases: Vec<CaseDef>,
}

impl Scenario {
  /// The adapters the scenario is generated and run for.
  pub fn adapters(&self) -> &'static [Adapter] {
    if !self.agent.handoffs.is_empty() {
      return &[Adapter::OpenAiAgents];
    }
    match self.gate {
      Gate::Native => &Adapter::ALL,
      Gate::Middleware | Gate::Wrapped => &[Adapter::LangGraph],
    }
  }

  /// This scenario again, its gated tools gated `gate`'s way
  /// (`<id>_<gate>`), with the same cases and exceptions.
  pub fn gated_by(&self, gate: Gate) -> Scenario {
    let suffix = match gate {
      Gate::Native => panic!("{}: already gated natively", self.id),
      Gate::Middleware => "middleware",
      Gate::Wrapped => "wrapped",
    };
    assert!(
      self.agent.subagents.is_empty() && self.agent.mcp.tools.is_empty(),
      "{}: only an agent's own tools can be gated another way",
      self.id
    );
    Scenario {
      // Built once per catalog listing; leaking keeps `Scenario` all-static.
      id: Box::leak(format!("{}_{suffix}", self.id).into_boxed_str()),
      agent: self.agent.clone(),
      policy: self.policy,
      mode: self.mode,
      gate,
      cases: self.cases.clone(),
    }
  }
}

/// How the agent's code gates its gated tools ([`ToolSpec::gated`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
  /// Each framework's usual way, on every adapter: openai-agents
  /// `needs_approval=True`, pydantic-ai `requires_approval=True`, and on
  /// langgraph a Sectr `@tool(needs_approval=True)` converted with
  /// `as_langgraph_tool`.
  Native,
  /// langgraph only: the tools are plain LangChain `@tool`s, and
  /// `create_agent`'s `HumanInTheLoopMiddleware(interrupt_on=...)` gates
  /// the gated ones.
  Middleware,
  /// langgraph only: the tools are plain LangChain `@tool`s, the gated ones
  /// converted with `as_langgraph_tool(t, needs_approval=True)`.
  Wrapped,
}

/// How a scenario's tools are gated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
  /// As the agent declares them.
  Plain,
  /// The gated twin of a [`both_modes`] scenario: every tool is gated, and
  /// the harness approves each pause itself and drops its
  /// `TOOL_APPROVAL_REQUIRED` from the journal output. The same case code
  /// runs, so everything it checks (journal and model requests) must come
  /// out the same as without gating, and every execution must follow its
  /// own approval.
  Gated,
}

/// A scenario and its gated twin (`<id>_gated`, [`Mode::Gated`]). The
/// plain run takes the framework's own tool path, the gated run the
/// approval-resume path (the adapter's, for openai_agents and langgraph):
/// the plain run is the reference the gated run must match. `cases` builds
/// each mode's case list, so known failures can differ by mode.
pub fn both_modes(
  id: &'static str,
  agent: AgentSpec,
  cases: impl Fn(Mode) -> Vec<CaseDef>,
) -> Vec<Scenario> {
  // Built once per catalog listing; leaking keeps `AgentSpec` all-static.
  let gated: &'static [ToolSpec] = Box::leak(
    agent
      .tools
      .iter()
      .map(|t| ToolSpec {
        gated: true,
        ..t.clone()
      })
      .collect::<Vec<_>>()
      .into_boxed_slice(),
  );
  vec![
    Scenario {
      id,
      agent: agent.clone(),
      policy: &[],
      mode: Mode::Plain,
      gate: Gate::Native,
      cases: cases(Mode::Plain),
    },
    Scenario {
      id: Box::leak(format!("{id}_gated").into_boxed_str()),
      agent: AgentSpec {
        tools: gated,
        ..agent
      },
      policy: &[],
      mode: Mode::Gated,
      gate: Gate::Native,
      cases: cases(Mode::Gated),
    },
  ]
}

#[derive(Clone)]
pub struct CaseDef {
  pub name: &'static str,
  pub run: CaseFn,
  pub exceptions: Vec<(Target, Exception)>,
}

#[derive(Clone)]
pub enum Exception {
  /// The adapter's SDK (or the endpoint, or the SDK's client for it) can't
  /// express this case.
  Unsupported(&'static str),
  /// The case is correct and these runs fail it, for the reason given.
  /// Strict: a passing run fails with "appears fixed, remove the tag".
  KnownFailure(&'static str),
  /// The SDK genuinely diverges; these runs use their own version.
  Override(CaseFn),
}

/// The runs an [`Exception`] applies to: an adapter on every endpoint
/// (`Adapter::LangGraph`), every adapter on an endpoint
/// (`Endpoint::OpenAiChat`), or one pair (`(Adapter::LangGraph,
/// Endpoint::OpenAiChat)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Target {
  adapter: Option<Adapter>,
  endpoint: Option<Endpoint>,
}

impl Target {
  fn matches(self, adapter: Adapter, endpoint: Endpoint) -> bool {
    self.adapter.is_none_or(|a| a == adapter) && self.endpoint.is_none_or(|e| e == endpoint)
  }
}

impl From<Adapter> for Target {
  fn from(adapter: Adapter) -> Self {
    Target {
      adapter: Some(adapter),
      endpoint: None,
    }
  }
}

impl From<Endpoint> for Target {
  fn from(endpoint: Endpoint) -> Self {
    Target {
      adapter: None,
      endpoint: Some(endpoint),
    }
  }
}

impl From<(Adapter, Endpoint)> for Target {
  fn from((adapter, endpoint): (Adapter, Endpoint)) -> Self {
    Target {
      adapter: Some(adapter),
      endpoint: Some(endpoint),
    }
  }
}

/// What the runner does for one (case, adapter, endpoint).
pub enum Plan {
  Run(CaseFn),
  ExpectFailure(CaseFn, &'static str),
  Skip(&'static str),
}

impl CaseDef {
  pub fn new(name: &'static str, run: CaseFn) -> Self {
    CaseDef {
      name,
      run,
      exceptions: Vec::new(),
    }
  }

  pub fn unsupported(self, target: impl Into<Target>, reason: &'static str) -> Self {
    self.except(target.into(), Exception::Unsupported(reason))
  }

  pub fn known_failure(self, target: impl Into<Target>, reason: &'static str) -> Self {
    self.except(target.into(), Exception::KnownFailure(reason))
  }

  pub fn override_for(self, target: impl Into<Target>, run: CaseFn) -> Self {
    self.except(target.into(), Exception::Override(run))
  }

  fn except(mut self, target: Target, exception: Exception) -> Self {
    self.exceptions.push((target, exception));
    self
  }

  /// A matching `unsupported` wins (a run that can't happen can't fail
  /// either); otherwise the first matching exception, in the order listed.
  pub fn plan(&self, adapter: Adapter, endpoint: Endpoint) -> Plan {
    let mut matching = self
      .exceptions
      .iter()
      .filter(|(t, _)| t.matches(adapter, endpoint))
      .map(|(_, e)| e);
    let unsupported = matching.clone().find_map(|e| match e {
      Exception::Unsupported(reason) => Some(*reason),
      _ => None,
    });
    if let Some(reason) = unsupported {
      return Plan::Skip(reason);
    }
    match matching.next() {
      None => Plan::Run(self.run),
      Some(Exception::Unsupported(_)) => unreachable!("handled above"),
      Some(Exception::KnownFailure(reason)) => Plan::ExpectFailure(self.run, reason),
      Some(Exception::Override(run)) => Plan::Run(*run),
    }
  }
}

/// `case!(f)` turns `async fn f(&mut Case) -> Result<()>` into a [`CaseFn`].
#[macro_export]
macro_rules! case {
  ($f:path) => {{
    fn run(h: &mut $crate::case::Case) -> $crate::scenario::BoxedCase<'_> {
      Box::pin($f(h))
    }
    run as $crate::scenario::CaseFn
  }};
}

pub type BoxedCase<'a> = BoxFuture<'a, Result<()>>;

#[cfg(test)]
mod tests {
  use super::*;

  async fn noop(_: &mut Case) -> Result<()> {
    Ok(())
  }

  fn outcome(plan: Plan) -> String {
    match plan {
      Plan::Run(_) => "run".into(),
      Plan::ExpectFailure(_, reason) => format!("known {reason}"),
      Plan::Skip(reason) => format!("skip {reason}"),
    }
  }

  #[test]
  fn exceptions_target_adapters_endpoints_or_pairs() {
    let (chat, lg, oa, pa) = (
      Endpoint::OpenAiChat,
      Adapter::LangGraph,
      Adapter::OpenAiAgents,
      Adapter::PydanticAi,
    );
    let case = CaseDef::new("c", crate::case!(noop))
      .known_failure(oa, "first")
      .known_failure((lg, chat), "second")
      .unsupported(chat, "no reasoning here")
      .known_failure(pa, "third");
    // A matching `unsupported` wins wherever it's listed.
    for adapter in [oa, lg, pa] {
      assert_eq!(outcome(case.plan(adapter, chat)), "skip no reasoning here");
    }
    let case = CaseDef::new("c", crate::case!(noop))
      .known_failure((lg, chat), "second")
      .known_failure(lg, "first");
    // Otherwise the first match, in the order listed.
    assert_eq!(outcome(case.plan(lg, chat)), "known second");
    assert_eq!(outcome(case.plan(oa, chat)), "run");
  }
}
