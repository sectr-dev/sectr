//! Subagents, under the beta contract: an agent the outer one
//! calls as a tool ([`SubagentTool`], written each framework's usual way).
//!
//! - Opaque: the journal holds the outer `TOOL_CALL` and `TOOL_RESULT` and
//!   nothing of the subagent's own turn (its messages, calls, results).
//! - The subagent tool itself can be gated, in code or by the policy, like
//!   any tool: approving it runs the whole subagent.
//! - A gated tool inside a subagent (in code, or named by the policy) fails
//!   loudly with the shared error code [`SUBAGENT_GATED_TOOL`], and never
//!   runs. An adapter does that at construction, at the start of the turn
//!   (for what it only knows then, like the deployment's policy), or when
//!   the subagent calls the tool: the case accepts any of them.
//!
//! The framework-specific ways users write subagents are covered by each
//! adapter's unit tests; these cases pin the contract at the platform's
//! boundary, on one usual shape per framework.

use anyhow::Result;
use serde_json::json;

use super::tools::{CANCEL_ORDER, REFUND, REJECTION};
use crate::adapter::Adapter;
use crate::agent::{AgentSpec, Mcp, Subagent, SubagentTool, ToolSpec, Ty};
use crate::case;
use crate::case::{Case, call, exec, text};
use crate::ev;
use crate::msg::{assistant, tool_call, tool_result, user};
use crate::scenario::{CaseDef, Gate, Mode, Scenario};

/// The error code every adapter fails with when a subagent has a gated
/// tool.
pub const SUBAGENT_GATED_TOOL: &str = "subagent_gated_tool";

const INSTRUCTIONS: &str = "You are a customer-support agent for a small shop.";
const SPECIALIST_INSTRUCTIONS: &str =
  "You are a shipping specialist. Answer from your tools, in one sentence.";

const TRACK_PARCEL: ToolSpec = ToolSpec {
  name: "track_parcel",
  description: "Track an order's parcel.",
  params: &[("order_id", Ty::Str)],
  returns: Ty::Str,
  gated: false,
};

/// A subagent whose tools are all ungated.
const SPECIALIST: Subagent = Subagent {
  name: "specialist",
  instructions: SPECIALIST_INSTRUCTIONS,
  tools: &[TRACK_PARCEL],
};

/// Runs the specialist; ungated.
const ASK_SPECIALIST: SubagentTool = SubagentTool {
  name: "ask_specialist",
  description: "Ask the shipping specialist about an order.",
  gated: false,
  agent: &SPECIALIST,
};

/// Runs the specialist; gated in code.
const ESCALATE: SubagentTool = SubagentTool {
  name: "escalate",
  description: "Escalate an order question to the shipping specialist (needs approval).",
  gated: true,
  agent: &SPECIALIST,
};

/// Runs the specialist; gated by the deployment policy.
const ASK_MANAGER: SubagentTool = SubagentTool {
  name: "ask_manager",
  description: "Ask the shipping manager about an order.",
  gated: false,
  agent: &SPECIALIST,
};

pub fn scenarios() -> Vec<Scenario> {
  vec![
    Scenario {
      id: "subagent",
      agent: AgentSpec {
        instructions: INSTRUCTIONS,
        tools: &[],
        output: None,
        mcp: Mcp::NONE,
        subagents: &[ASK_SPECIALIST, ESCALATE, ASK_MANAGER],
        handoffs: &[],
      },
      policy: &["ask_manager"],
      mode: Mode::Plain,
      gate: Gate::Native,
      cases: vec![
        CaseDef::new("delegates", case!(delegates)),
        CaseDef::new("gated_approved", case!(gated_approved)),
        CaseDef::new("gated_denied", case!(gated_denied)),
        CaseDef::new("policy_approved", case!(policy_approved)),
        CaseDef::new("policy_denied", case!(policy_denied)),
      ],
    },
    fails_loudly_scenario(
      "subagent_inner_gated",
      &GATED_INSIDE,
      &[],
      CaseDef::new("fails_loudly", case!(fails_loudly_refund)),
    ),
    fails_loudly_scenario(
      "subagent_inner_policy",
      &POLICY_INSIDE,
      &["cancel_order"],
      CaseDef::new("fails_loudly", case!(fails_loudly_cancel)).known_failure(
        Adapter::PydanticAi,
        "the approval policy doesn't reach a delegate",
      ),
    ),
  ]
}

/// A subagent with a tool gated in code.
const GATED_SPECIALIST: Subagent = Subagent {
  name: "specialist",
  instructions: SPECIALIST_INSTRUCTIONS,
  tools: &[TRACK_PARCEL, REFUND],
};
const GATED_INSIDE: [SubagentTool; 1] = [SubagentTool {
  agent: &GATED_SPECIALIST,
  ..ASK_SPECIALIST
}];

/// A subagent with a tool the policy names (`cancel_order`).
const POLICY_SPECIALIST: Subagent = Subagent {
  name: "specialist",
  instructions: SPECIALIST_INSTRUCTIONS,
  tools: &[TRACK_PARCEL, CANCEL_ORDER],
};
const POLICY_INSIDE: [SubagentTool; 1] = [SubagentTool {
  agent: &POLICY_SPECIALIST,
  ..ASK_SPECIALIST
}];

/// An agent whose one subagent has a gated tool, which the subagent's
/// model calls in `fails`.
fn fails_loudly_scenario(
  id: &'static str,
  subagents: &'static [SubagentTool],
  policy: &'static [&'static str],
  fails: CaseDef,
) -> Scenario {
  Scenario {
    id,
    agent: AgentSpec {
      instructions: INSTRUCTIONS,
      tools: &[],
      output: None,
      mcp: Mcp::NONE,
      subagents,
      handoffs: &[],
    },
    policy,
    mode: Mode::Plain,
    gate: Gate::Native,
    cases: vec![fails],
  }
}

const ASK: &str = "Where is order 42?";
const SAY: &str = "Let me ask the specialist.";
const ANSWER: &str = "Your order is in transit.";

fn order() -> serde_json::Value {
  json!({"order_id": "42"})
}

/// The specialist's own turn, from its model's first request: it calls
/// `track_parcel` and answers "It's in transit.".
async fn specialist_answers(h: &mut Case) -> Result<()> {
  let (checking, answer) = ("Checking.", "It's in transit.");
  h.subagent_model(&SPECIALIST)
    .await?
    .expect_history(&[user(ASK)])?
    .reply(text(checking).and(call("inner_1", "track_parcel", order())))
    .await?;
  h.tool(exec("track_parcel", order()).returns(json!("In transit")))
    .await?;
  h.subagent_model(&SPECIALIST)
    .await?
    .expect_history(&[
      user(ASK),
      assistant(checking),
      tool_call("inner_1", "track_parcel", order()),
      tool_result("inner_1", "In transit"),
    ])?
    .reply(text(answer))
    .await
}

/// The outer model calls `name` with the user's question.
async fn calls(h: &mut Case, name: &str) -> Result<()> {
  h.user(ASK).await?;
  h.model()
    .await?
    .expect_history(&[user(ASK)])?
    .reply(text(SAY).and(call("call_1", name, json!({"input": ASK}))))
    .await
}

/// The outer model gets `result` for `name` and answers; the journal holds
/// `before`, then the outer call's result and answer, and nothing of the
/// subagent's turn.
async fn finishes(h: &mut Case, name: &str, result: &str, before: Vec<ev::Ev>) -> Result<()> {
  h.model()
    .await?
    .expect_history(&[
      user(ASK),
      assistant(SAY),
      tool_call("call_1", name, json!({"input": ASK})),
      tool_result("call_1", result),
    ])?
    .reply(text(ANSWER))
    .await?;
  let mut expected = before;
  expected.extend([
    ev::tool_result("call_1", json!(result)),
    ev::assistant(ANSWER),
    ev::turn_end(),
  ]);
  h.journal(&expected).await
}

/// The subagent runs its own turn and answers; the journal shows only the
/// outer call and its result.
async fn delegates(h: &mut Case) -> Result<()> {
  calls(h, "ask_specialist").await?;
  specialist_answers(h).await?;
  let call = ev::tool_call("call_1", "ask_specialist", json!({"input": ASK}));
  finishes(
    h,
    "ask_specialist",
    "It's in transit.",
    vec![ev::assistant(SAY), call],
  )
  .await
}

/// The outer model calls `name`; the turn pauses before the subagent runs.
async fn pauses(h: &mut Case, name: &str) -> Result<()> {
  calls(h, name).await?;
  h.journal(&[
    ev::assistant(SAY),
    ev::tool_call("call_1", name, json!({"input": ASK})),
    ev::approval_required("call_1", name, json!({"input": ASK})),
  ])
  .await
}

/// Approving `name` runs the whole subagent once.
async fn approved(h: &mut Case, name: &str) -> Result<()> {
  pauses(h, name).await?;
  h.approve().await?;
  specialist_answers(h).await?;
  finishes(h, name, "It's in transit.", vec![]).await
}

/// Denied, the subagent never runs.
async fn denied(h: &mut Case, name: &str) -> Result<()> {
  pauses(h, name).await?;
  h.deny().await?;
  finishes(h, name, REJECTION, vec![]).await
}

/// `escalate` is gated in code.
async fn gated_approved(h: &mut Case) -> Result<()> {
  approved(h, "escalate").await
}

async fn gated_denied(h: &mut Case) -> Result<()> {
  denied(h, "escalate").await
}

/// `ask_manager` is gated by the policy.
async fn policy_approved(h: &mut Case) -> Result<()> {
  approved(h, "ask_manager").await
}

async fn policy_denied(h: &mut Case) -> Result<()> {
  denied(h, "ask_manager").await
}

async fn fails_loudly_refund(h: &mut Case) -> Result<()> {
  fails_loudly(h, &GATED_SPECIALIST, "refund").await
}

async fn fails_loudly_cancel(h: &mut Case) -> Result<()> {
  fails_loudly(h, &POLICY_SPECIALIST, "cancel_order").await
}

/// The subagent has a gated tool, `inner`. The adapter rejects the agent
/// when the runner starts (the code in the `SESSION_ERROR`, from the
/// runner's stderr), or the turn ends in an `ERROR` with the code: at its
/// start, before the model runs, or once the subagent's model calls
/// `inner`. Either way `inner` never runs: an execution would reach the
/// case unanswered, and fail it.
async fn fails_loudly(h: &mut Case, agent: &'static Subagent, inner: &str) -> Result<()> {
  h.user(ASK).await?;
  let Some(first) = h.model_or_end().await? else {
    h.journal(&[
      ev::one_of(&[
        ev::session_error_containing(SUBAGENT_GATED_TOOL),
        ev::error_code(SUBAGENT_GATED_TOOL),
      ]),
      ev::turn_end(),
    ])
    .await?;
    return Ok(());
  };
  let input = json!({"input": ASK});
  first
    .expect_history(&[user(ASK)])?
    .reply(text(SAY).and(call("call_1", "ask_specialist", input.clone())))
    .await?;
  h.subagent_model(agent)
    .await?
    .expect_history(&[user(ASK)])?
    .reply(call("inner_1", inner, order()))
    .await?;
  h.journal(&[
    ev::assistant(SAY),
    ev::tool_call("call_1", "ask_specialist", input),
    ev::error_code(SUBAGENT_GATED_TOOL),
    ev::turn_end(),
  ])
  .await
}
