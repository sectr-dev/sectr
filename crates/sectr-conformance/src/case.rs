//! The API cases are written against.
//!
//! A case drives one session through `DevDispatch`, the same in-process
//! orchestration `sectr dev` runs: it journals the platform events, spawns a
//! fresh runner per invocation, and runs the worker's `TurnEngine`. The turn
//! runs in the background; the case awaits whichever side it expects next, a
//! model request ([`Case::model`]), a tool execution ([`Case::tool`]) or
//! journal output ([`Case::journal`]). Model requests and tool executions
//! block the agent until the case answers, so the sides stay in lockstep,
//! and every execution is one the case answered explicitly: an unexpected
//! (e.g. repeated) execution fails the case as an unanswered request. Every
//! wait fails at once if the turn has already ended, otherwise after a
//! timeout, and reports the caller's line with the journal so far. The
//! ordering and strictness rules are summarized in the crate docs.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::future::Future;
use std::panic::Location;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use sectr_common::api::UserMessageInput;
use sectr_common::events::{
  ApprovalDecision, Event, EventPayload, EventType, NewEvent, TypedDelta,
};
use sectr_common::journal::SessionId;
use sectr_worker::dispatch::DevDispatch;
use sectr_worker::turn::TurnResult;
use tokio::task::JoinHandle;

use crate::adapter::Adapter;
use crate::agent::{AgentSpec, HandoffAgent, Subagent};
use crate::endpoint::Endpoint;
use crate::ev::Ev;
use crate::mock;
pub use crate::mock::{Fault, ModelCall, Reply, call, call_text, exec, reasoning, text};
use crate::replay;
use crate::replay::Prefix;
use crate::warning;
use crate::warning::Warning;

/// Model faults for [`ModelCall::fail`].
pub mod fault {
  pub use crate::mock::{error_chunk, reset, status, truncated};
}
use crate::mock::{Request, ToolAnswer, ToolExec};
use crate::scenario::Mode;
use crate::seed::Seed;

/// How long any single wait (a request, a journal event, a turn end)
/// may take. Covers a runner spawn plus framework imports.
const WAIT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(15);

/// How long a journal wait tolerates no new events while the agent is
/// blocked on a request the case hasn't answered: the turn can't progress,
/// so fail with that request rather than time out.
const BLOCKED: Duration = Duration::from_secs(2);

/// [`Case::tools`]: how long to wait for more executions in flight with
/// one that arrived, and the gap between answering them, so they complete
/// in the listed order.
const QUIET: Duration = Duration::from_millis(300);
const STAGGER: Duration = Duration::from_millis(200);

type Turn = JoinHandle<std::result::Result<TurnResult, sectr_worker::dispatch::DispatchError>>;

pub struct Case {
  pub adapter: Adapter,
  pub endpoint: Endpoint,
  agent: AgentSpec,
  route: &'static str,
  dispatch: Arc<DevDispatch>,
  model: mock::Run,
  session: Option<SessionId>,
  turn: Option<Turn>,
  /// How the last reaped turn ended, until a `TurnEnd` checks it.
  ended: Option<TurnResult>,
  /// The last fetched journal seq.
  seen_seq: Option<i64>,
  /// Every event fetched so far, for failure reports.
  log: Vec<Event>,
  /// Turn output not yet matched by [`Case::journal`].
  output: VecDeque<Event>,
  /// A request taken off the channel by a journal wait, not yet answered.
  held: Option<mock::Incoming>,
  /// The call id of the last matched `TOOL_APPROVAL_REQUIRED`, until a
  /// decision.
  paused: Option<String>,
  mode: Mode,
  /// [`Mode::Gated`]: approvals the harness granted, and executions the
  /// case answered. Each execution must follow its own approval.
  approvals: usize,
  executions: usize,
  /// Whether the current turn's output matched an `ERROR`.
  errored: bool,
  /// Whether the current turn's output matched a `SESSION_ERROR`.
  failed: bool,
  /// What the case noticed but didn't stop for.
  warnings: Vec<Warning>,
  /// The prefix of the agent's last model request ([`crate::replay`]), and
  /// the one the next invocation's first request must start with.
  last_prefix: Option<Prefix>,
  replay_from: Option<Prefix>,
  /// Whether a replay difference fails the case ([`Case::strict_replay`]).
  strict_replay: bool,
}

impl Case {
  pub(crate) fn new(
    adapter: Adapter,
    endpoint: Endpoint,
    agent: AgentSpec,
    mode: Mode,
    dispatch: Arc<DevDispatch>,
    model: mock::Run,
  ) -> Self {
    Case {
      adapter,
      endpoint,
      agent,
      route: "/chat",
      dispatch,
      model,
      session: None,
      turn: None,
      ended: None,
      seen_seq: None,
      log: Vec::new(),
      output: VecDeque::new(),
      held: None,
      paused: None,
      mode,
      approvals: 0,
      executions: 0,
      errored: false,
      failed: false,
      warnings: Vec::new(),
      last_prefix: None,
      replay_from: None,
      strict_replay: false,
    }
  }

  /// Fail the case on a replay difference ([`crate::replay`]), not just
  /// warn: for a case that checks the replay itself.
  pub fn strict_replay(&mut self) {
    self.strict_replay = true;
  }

  /// Journal a user message (`INVOCATION_STARTED`, reason `user_message`;
  /// `SESSION_CREATED` first on the first turn) and start the invocation.
  /// Fails if the previous turn is still running or left output unmatched.
  #[track_caller]
  pub fn user(&mut self, text: &str) -> impl Future<Output = Result<()>> + '_ {
    let (at, text) = (Location::caller(), text.to_owned());
    async move {
      self.settled(at).await?;
      let session = match self.session {
        Some(session) => session,
        None => {
          let session = self.dispatch.create_session(self.route).await?;
          self.session = Some(session);
          session
        }
      };
      let input = UserMessageInput::new(serde_json::json!(text))?;
      let dispatch = self.dispatch.clone();
      self.replay_from = self.last_prefix.take();
      self.turn = Some(tokio::spawn(async move {
        dispatch.send_message(session, input).await
      }));
      Ok(())
    }
  }

  /// Approve the pending tool call: journal `TOOL_APPROVAL_GRANTED` and
  /// start the resume invocation (reason `approval_decision`). Fails like
  /// [`Case::user`], and if no approval is pending.
  #[track_caller]
  pub fn approve(&mut self) -> impl Future<Output = Result<()>> + '_ {
    self.decide(Location::caller(), true)
  }

  /// Deny the pending tool call (`TOOL_APPROVAL_DENIED`), as [`Case::approve`].
  #[track_caller]
  pub fn deny(&mut self) -> impl Future<Output = Result<()>> + '_ {
    self.decide(Location::caller(), false)
  }

  /// Seed the journal with `prefix` before the first [`Case::user`]: a
  /// prior conversation as the journal holds it, e.g. cut by a crash (see
  /// [`crate::seed`]). Seeded events are inputs, never matched; the next
  /// turn's model request shows how the adapter folds them.
  #[track_caller]
  pub fn seed(&mut self, prefix: &[Seed]) -> impl Future<Output = Result<()>> + '_ {
    let (at, prefix) = (Location::caller(), prefix.to_vec());
    async move {
      if self.session.is_some() {
        bail!("{at}: seed before the first turn");
      }
      let session = self.dispatch.create_session(self.route).await?;
      self.session = Some(session);
      let deployment = self
        .dispatch
        .registry()
        .get_session(session)
        .await?
        .deployment_id;
      let mut payloads = Vec::new();
      for item in prefix {
        seed_payloads(item, self.route, deployment, &mut payloads);
      }
      let events = payloads.into_iter().map(NewEvent::new).collect();
      self
        .dispatch
        .journal()
        .append(session, deployment, events)
        .await?;
      // Inputs: past the cursor, never output.
      let seeded = self.dispatch.journal().fetch(session, None).await?;
      self.seen_seq = seeded.last().map(|e| e.seq);
      self.log.extend(seeded);
      Ok(())
    }
  }

  /// Approve a call that is no longer pending (the session moved past it,
  /// or its resume died): the platform must refuse the decision, journal
  /// nothing and run nothing.
  #[track_caller]
  pub fn approve_expecting_refusal(&mut self) -> impl Future<Output = Result<()>> + '_ {
    let at = Location::caller();
    async move {
      self.settled(at).await?;
      let Some(session) = self.session else {
        bail!("{at}: no session yet");
      };
      let decision = ApprovalDecision {
        approved: true,
        approver: None,
        extra: Default::default(),
      };
      // A refusal is immediate; a resume still running was accepted.
      let dispatch = self.dispatch.clone();
      let resume =
        tokio::spawn(async move { dispatch.resume_with_decision(session, decision).await });
      let accepted = match tokio::time::timeout(BLOCKED, resume).await {
        Ok(Ok(Err(_refused))) => return self.settled(at).await,
        Ok(Ok(Ok(result))) => format!("the turn ended as {result:?}"),
        Ok(Err(e)) => return Err(e.into()),
        Err(_) => "a resume is running".to_string(),
      };
      bail!(self.report(
        at,
        &format!("the platform accepted a decision for a call no longer pending ({accepted})")
      ))
    }
  }

  /// Answer these executions as soon as they arrive, without waiting for
  /// the case: tools that finish at once, e.g. a sibling that completes
  /// before a gated call pauses. Each answers one execution; a repeat
  /// reaches the case as an unexpected execution, and an answer no
  /// execution used fails the case at the end. Plain mode only (gated
  /// mode counts the executions the case answers).
  pub fn answer_ahead(&mut self, answers: impl IntoIterator<Item = ToolAnswer>) {
    assert_eq!(self.mode, Mode::Plain, "answer_ahead is plain-mode only");
    self.model.answer_ahead(answers);
  }

  /// The call id of the pending approval, as matched by the last
  /// [`ev::approval_required`](crate::ev::approval_required) or
  /// [`ev::one_of`](crate::ev::one_of).
  #[track_caller]
  pub fn paused(&self) -> Result<String> {
    let at = Location::caller();
    self
      .paused
      .clone()
      .ok_or_else(|| anyhow!("{at}: no TOOL_APPROVAL_REQUIRED matched since the last decision"))
  }

  async fn decide(&mut self, at: &'static Location<'static>, approved: bool) -> Result<()> {
    self.settled(at).await?;
    self.paused = None;
    let Some(session) = self.session else {
      bail!("{at}: no session yet, so no approval is pending");
    };
    let dispatch = self.dispatch.clone();
    self.replay_from = self.last_prefix.take();
    self.turn = Some(tokio::spawn(async move {
      let decision = ApprovalDecision {
        approved,
        approver: None,
        extra: Default::default(),
      };
      dispatch.resume_with_decision(session, decision).await
    }));
    Ok(())
  }

  /// Wait for the agent's next model request. Checks that it carries the
  /// agent's instructions as a system prompt and offers the agent's tools.
  #[track_caller]
  pub fn model(&mut self) -> impl Future<Output = Result<ModelCall>> + '_ {
    let at = Location::caller();
    async move {
      let request = self.next_request(at, "a model request").await?;
      self.agent_model(at, request)
    }
  }

  /// Like [`Case::model`], but the turn may end first, without a model
  /// request (`None`): for an agent that may be rejected when the runner
  /// starts, or whose turn may fail before its model runs.
  #[track_caller]
  pub fn model_or_end(&mut self) -> impl Future<Output = Result<Option<ModelCall>>> + '_ {
    let at = Location::caller();
    async move {
      match self.next_request_or_end(at, "a model request").await? {
        None => Ok(None),
        Some(request) => self.agent_model(at, request).map(Some),
      }
    }
  }

  /// Wait for a model request from `agent`, run by a subagent tool: it
  /// carries the subagent's instructions and offers its tools.
  #[track_caller]
  pub fn subagent_model(
    &mut self,
    agent: &'static Subagent,
  ) -> impl Future<Output = Result<ModelCall>> + '_ {
    let at = Location::caller();
    async move {
      let request = self.next_request(at, "a subagent's model request").await?;
      let tools: Vec<&str> = agent.tools.iter().map(|t| t.name).collect();
      self.checked_model(at, request, agent.instructions, &tools)
    }
  }

  /// Wait for a model request from `agent`, which the conversation was
  /// handed off to: it carries the handoff agent's instructions and offers
  /// its tools, and is checked like the agent's own ([`Case::model`]): it
  /// streams, and replays the invocation before.
  #[track_caller]
  pub fn handoff_model(
    &mut self,
    agent: &'static HandoffAgent,
  ) -> impl Future<Output = Result<ModelCall>> + '_ {
    let at = Location::caller();
    async move {
      let request = self
        .next_request(at, "a handoff agent's model request")
        .await?;
      let tools: Vec<&str> = agent.tools.iter().map(|t| t.name).collect();
      self.conversation_model(at, request, agent.instructions, &tools)
    }
  }

  /// `request` as a model request from the case's agent, which the adapter
  /// must stream (its deltas are what reaches the journal). A subagent's
  /// needn't ([`Case::subagent_model`]).
  fn agent_model(&mut self, at: &Location<'_>, request: Request) -> Result<ModelCall> {
    let tools: Vec<&str> = self
      .agent
      .tools
      .iter()
      .chain(self.agent.mcp.tools)
      .map(|t| t.name)
      .chain(self.agent.subagents.iter().map(|t| t.name))
      .collect();
    self.conversation_model(at, request, self.agent.instructions, &tools)
  }

  /// `request` as a model request from the agent the conversation is with
  /// (the case's agent, or one it handed off to): it carries
  /// `instructions`, offers `tools`, streams, and passes the replay check.
  fn conversation_model(
    &mut self,
    at: &Location<'_>,
    request: Request,
    instructions: &str,
    tools: &[&str],
  ) -> Result<ModelCall> {
    let call = self.checked_model(at, request, instructions, tools)?;
    if !call.streamed() {
      bail!(self.report(
        at,
        "the agent's model request doesn't stream (stream: true)"
      ));
    }
    self.check_replay(at, &call)?;
    Ok(call)
  }

  /// The replay check ([`crate::replay`]) on the agent's model request
  /// `call`: the first of an invocation must start with the previous
  /// invocation's last.
  fn check_replay(&mut self, at: &Location<'_>, call: &ModelCall) -> Result<()> {
    let prefix = Prefix::of(self.endpoint, call.text());
    let (differences, kind) = match self.replay_from.take() {
      Some(prev) if replay::drops_only_reasoning(&prev, &prefix) => (
        replay::compare(&prev, &prefix),
        warning::REPLAY_DROPPED_REASONING,
      ),
      Some(prev) => (replay::compare(&prev, &prefix), warning::REPLAY_CHANGED),
      None => (Vec::new(), warning::REPLAY_CHANGED),
    };
    self.last_prefix = Some(prefix);
    if self.strict_replay && !differences.is_empty() {
      bail!(self.report(
        at,
        &format!(
          "the model request replays the last one differently:\n{}",
          differences.join("\n")
        )
      ));
    }
    for detail in differences {
      self.warnings.push(Warning {
        kind,
        detail: format!("{at}: {detail}"),
      });
    }
    Ok(())
  }

  /// `request` as a model request carrying `instructions` as a system
  /// prompt and offering `tools`.
  fn checked_model(
    &self,
    at: &Location<'_>,
    request: Request,
    instructions: &str,
    tools: &[&str],
  ) -> Result<ModelCall> {
    let call = match request {
      Request::Model(call) => call,
      other => bail!(self.report(
        at,
        &format!("expected a model request, got {}", other.describe())
      )),
    };
    if !call.system().iter().any(|s| s.contains(instructions)) {
      bail!(
        "{at}: the model request doesn't carry the agent's instructions {instructions:?}\n  \
         system messages: {:?}",
        call.system()
      );
    }
    let offered = call.tools();
    let missing: Vec<&str> = tools
      .iter()
      .copied()
      .filter(|name| !offered.contains(name))
      .collect();
    if !missing.is_empty() {
      bail!("{at}: the model request doesn't offer the tools {missing:?}\n  offered: {offered:?}");
    }
    // SDK-native: openai-agents' LiteLLM model sends Anthropic each
    // parallel call as its own assistant message, followed by its result.
    let split = matches!(
      (self.adapter, self.endpoint),
      (Adapter::OpenAiAgents, Endpoint::Anthropic)
    );
    Ok(call.one_call_per_message(split))
  }

  /// Answer the agent's next tool execution: it must match `answer`'s name
  /// and arguments, and gets its outcome. The tool blocks until then.
  #[track_caller]
  pub fn tool(&mut self, answer: ToolAnswer) -> impl Future<Output = Result<()>> + '_ {
    self.tools([answer])
  }

  /// Answer the next tool executions, one per answer, matched by name and
  /// arguments. The case decides the completion order: executions in
  /// flight together (the SDKs run a response's tools concurrently) are
  /// answered in the listed order, 200ms apart. An execution that
  /// only starts after another finishes is answered when it arrives.
  #[track_caller]
  pub fn tools(
    &mut self,
    answers: impl IntoIterator<Item = ToolAnswer>,
  ) -> impl Future<Output = Result<()>> + '_ {
    let at = Location::caller();
    let mut answers: Vec<Option<ToolAnswer>> = answers.into_iter().map(Some).collect();
    async move {
      let mut arrived: Vec<(usize, ToolExec)> = Vec::new();
      let mut left = answers.len();
      while left > 0 {
        // Wait for one execution, then for the others in flight with it.
        let mut next = Some(self.next_request(at, "a tool execution").await?);
        while let Some(request) = next.take() {
          let exec = self.expected_exec(at, request, &answers, &arrived)?;
          let i = answers
            .iter()
            .enumerate()
            .position(|(i, a)| {
              a.as_ref()
                .is_some_and(|a| a.name == exec.name() && a.args == *exec.args())
                && !arrived.iter().any(|(j, _)| *j == i)
            })
            .expect("expected_exec checked it");
          if self.mode == Mode::Gated && self.executions + arrived.len() >= self.approvals {
            bail!(self.report(
              at,
              &format!(
                "gated mode: tool execution {}({}) without its own approval",
                exec.name(),
                exec.args()
              )
            ));
          }
          arrived.push((i, exec));
          if arrived.len() < left {
            next = self.try_request(at, QUIET).await?;
          }
        }
        arrived.sort_by_key(|(i, _)| *i);
        for (n, (i, exec)) in arrived.drain(..).enumerate() {
          if n > 0 {
            tokio::time::sleep(STAGGER).await;
          }
          let answer = answers[i].take().expect("answered once");
          exec.decide(answer.outcome)?;
          self.executions += 1;
          left -= 1;
        }
      }
      Ok(())
    }
  }

  /// `request` as a tool execution one of the unanswered, not yet arrived
  /// `answers` expects; fails otherwise.
  fn expected_exec(
    &self,
    at: &Location<'_>,
    request: Request,
    answers: &[Option<ToolAnswer>],
    arrived: &[(usize, ToolExec)],
  ) -> Result<ToolExec> {
    let exec = match request {
      Request::Tool(exec) => exec,
      other => bail!(self.report(
        at,
        &format!("expected a tool execution, got {}", other.describe())
      )),
    };
    let waiting: Vec<&ToolAnswer> = answers
      .iter()
      .enumerate()
      .filter(|(i, _)| !arrived.iter().any(|(j, _)| j == i))
      .filter_map(|(_, a)| a.as_ref())
      .collect();
    if !waiting
      .iter()
      .any(|a| a.name == exec.name() && a.args == *exec.args())
    {
      let expected: Vec<String> = waiting
        .iter()
        .map(|a| format!("{}({})", a.name, a.args))
        .collect();
      bail!(self.report(
        at,
        &format!(
          "unexpected tool execution {}({})\n  expected one of: {}",
          exec.name(),
          exec.args(),
          expected.join(", ")
        )
      ));
    }
    Ok(exec)
  }

  /// A request arriving within `wait`, if any.
  async fn try_request(&mut self, at: &Location<'_>, wait: Duration) -> Result<Option<Request>> {
    let incoming = match self.held.take() {
      Some(incoming) => incoming,
      None => match tokio::time::timeout(wait, self.model.next()).await {
        Err(_) => return Ok(None),
        Ok(None) => bail!("{at}: the mock server is gone"),
        Ok(Some(incoming)) => incoming,
      },
    };
    match incoming {
      Ok(request) => Ok(Some(request)),
      Err(reason) => bail!(self.report(at, &format!("the mock rejected a request: {reason}"))),
    }
  }

  /// The agent's next request; fails if the turn ends without one.
  async fn next_request(&mut self, at: &Location<'_>, what: &str) -> Result<Request> {
    match self.next_request_or_end(at, what).await? {
      Some(request) => Ok(request),
      None => bail!(self.report(at, &format!("expected {what}, but the turn ended"))),
    }
  }

  /// The agent's next request, or `None` if the turn ends without one.
  async fn next_request_or_end(
    &mut self,
    at: &Location<'_>,
    what: &str,
  ) -> Result<Option<Request>> {
    if let Some(incoming) = self.held.take() {
      return match incoming {
        Ok(request) => Ok(Some(request)),
        Err(reason) => bail!(self.report(at, &format!("the mock rejected a request: {reason}"))),
      };
    }
    let deadline = Instant::now() + WAIT;
    loop {
      if let Ok(incoming) = tokio::time::timeout(POLL, self.model.next()).await {
        return match incoming {
          Some(Ok(request)) => Ok(Some(request)),
          Some(Err(reason)) => {
            bail!(self.report(at, &format!("the mock rejected a request: {reason}")))
          }
          None => bail!("{at}: the mock server is gone"),
        };
      }
      if self.turn_ended().await? {
        self.fetch().await?;
        return Ok(None);
      }
      if Instant::now() > deadline {
        bail!(self.report(at, &format!("timed out waiting for {what}")));
      }
    }
  }

  /// Match the turn's next output events, in order, against `expected`.
  /// Events the case journaled itself (session start, invocation starts,
  /// approval decisions) are inputs and never appear here. A `TurnEnd` also
  /// checks the turn concluded cleanly, an `ApprovalRequired` that it
  /// suspended (and records the call for [`Case::paused`]).
  #[track_caller]
  pub fn journal(&mut self, expected: &[Ev]) -> impl Future<Output = Result<()>> + '_ {
    let (at, expected) = (Location::caller(), expected.to_vec());
    async move {
      for ev in &expected {
        match ev {
          Ev::Assistant { reasoning, text } => self.expect_assistant(at, reasoning, text).await?,
          Ev::Partial { text } => self.expect_partial(at, text).await?,
          Ev::TurnEnd => self.expect_turn_end(at).await?,
          Ev::OneOf(options) => {
            let event = self.next_output(at, &ev.to_string()).await?;
            let Some(matched) = options.iter().find(|e| single_matches(e, &event)) else {
              bail!(self.mismatch(at, &ev.to_string(), &event));
            };
            self.matched(at, matched).await?;
          }
          single => {
            let event = self.next_output(at, &single.to_string()).await?;
            if !single_matches(single, &event) {
              bail!(self.mismatch(at, &single.to_string(), &event));
            }
            self.matched(at, single).await?;
          }
        }
      }
      Ok(())
    }
  }

  /// Whether the current turn's output so far matched a `SESSION_ERROR`:
  /// after an [`ev::one_of`](crate::ev::one_of) that offers one, which
  /// option matched.
  pub fn session_failed(&self) -> bool {
    self.failed
  }

  /// After the case returns: the turn is over, and nothing it produced
  /// (journal output, requests) went unmatched.
  pub(crate) async fn finish(&mut self) -> Result<()> {
    let at = &"after the case returned";
    if self.mode == Mode::Gated && self.approvals != self.executions {
      bail!(self.report(
        at,
        &format!(
          "gated mode: {} approvals for {} tool executions",
          self.approvals, self.executions
        )
      ));
    }
    if self.turn.is_some() {
      let deadline = Instant::now() + WAIT;
      while !self.turn_ended().await? {
        // A turn blocked on a request never ends: fail now, naming it.
        if let Some(incoming) = self.take_pending() {
          bail!(self.report(at, &unanswered(&incoming)));
        }
        if Instant::now() > deadline {
          bail!(self.report(at, "the case ended while the turn was still running"));
        }
        tokio::time::sleep(POLL).await;
      }
    }
    let unused = self.model.unused_ahead();
    if !unused.is_empty() {
      bail!(self.report(
        at,
        &format!("expected executions never happened: {}", unused.join(", "))
      ));
    }
    self.settled(at).await
  }

  // ---- matching -------------------------------------------------------------

  /// After a single-event pattern matched: an approval request must have
  /// suspended the turn.
  async fn matched(&mut self, at: &Location<'_>, ev: &Ev) -> Result<()> {
    if let Ev::Error { .. } | Ev::ErrorCode { .. } = ev {
      self.errored = true;
    }
    if let Ev::SessionError { .. } = ev {
      self.failed = true;
    }
    if let Ev::ApprovalRequired { id, .. } = ev {
      let result = self.await_turn(at).await?;
      if result != TurnResult::AwaitingApproval {
        bail!(self.report(at, &format!("the turn ended as {result:?}")));
      }
      self.paused = Some(id.clone());
    }
    Ok(())
  }

  async fn expect_assistant(
    &mut self,
    at: &Location<'_>,
    want_reasoning: &str,
    want: &str,
  ) -> Result<()> {
    let what = crate::ev::message(want_reasoning, want).to_string();
    let start = self.next_output(at, &what).await?;
    let id = match &start.payload {
      EventPayload::MessageStart(m) if m.role == "assistant" => m.message_id.clone(),
      _ => bail!(self.mismatch(at, &what, &start)),
    };
    let (mut got, mut got_reasoning) = (String::new(), String::new());
    loop {
      let event = self.next_output(at, &what).await?;
      match &event.payload {
        EventPayload::MessageDelta(d) if d.message_id == id => match &d.delta {
          _ if delta_text(&d.delta).is_empty() => self.warn_empty_delta(at, &event),
          TypedDelta::Text { text, .. } => got.push_str(text),
          TypedDelta::Reasoning { text, .. } => got_reasoning.push_str(text),
        },
        EventPayload::MessageEnd(e) if e.message_id == id => break,
        _ => bail!(self.mismatch(at, &what, &event)),
      }
    }
    if got != want {
      bail!(self.report(
        at,
        &format!("assistant text mismatch\n  expected: {want:?}\n  actual:   {got:?}")
      ));
    }
    if got_reasoning != want_reasoning {
      bail!(self.report(
        at,
        &format!(
          "assistant reasoning mismatch\n  expected: {want_reasoning:?}\n  actual:   {got_reasoning:?}"
        )
      ));
    }
    Ok(())
  }

  fn warn_empty_delta(&mut self, at: &Location<'_>, event: &Event) {
    self.warnings.push(Warning {
      kind: warning::EMPTY_DELTA,
      detail: format!("{at}: {}", line(event)),
    });
  }

  /// The warnings so far.
  pub(crate) fn warnings(&self) -> &[Warning] {
    &self.warnings
  }

  async fn expect_partial(&mut self, at: &Location<'_>, want: &str) -> Result<()> {
    let what = crate::ev::partial(want).to_string();
    let start = self.next_output(at, &what).await?;
    let id = match &start.payload {
      EventPayload::MessageStart(m) if m.role == "assistant" => m.message_id.clone(),
      _ => bail!(self.mismatch(at, &what, &start)),
    };
    let mut got = String::new();
    loop {
      let event = self.next_output(at, &what).await?;
      match &event.payload {
        EventPayload::MessageDelta(d) if d.message_id == id => match &d.delta {
          _ if delta_text(&d.delta).is_empty() => self.warn_empty_delta(at, &event),
          TypedDelta::Text { text, .. } => got.push_str(text),
          TypedDelta::Reasoning { .. } => bail!(self.mismatch(at, &what, &event)),
        },
        EventPayload::MessageEnd(e) if e.message_id == id => {
          bail!(self.report(at, &format!("expected {what}, but it was closed")))
        }
        // The message's end: put the next event back for the next pattern.
        _ => {
          self.output.push_front(event);
          break;
        }
      }
    }
    if got != want {
      bail!(self.report(
        at,
        &format!("partial text mismatch\n  expected: {want:?}\n  actual:   {got:?}")
      ));
    }
    Ok(())
  }

  async fn expect_turn_end(&mut self, at: &Location<'_>) -> Result<()> {
    let event = self.next_output(at, "NO_MORE_ACTIONS").await?;
    if event.event_type != EventType::NoMoreActions {
      bail!(self.mismatch(at, "NO_MORE_ACTIONS", &event));
    }
    let result = self.await_turn(at).await?;
    let errored = std::mem::take(&mut self.errored);
    let ok = if std::mem::take(&mut self.failed) {
      matches!(result, TurnResult::Failed { .. })
    } else {
      result == (TurnResult::Completed { errored })
    };
    if !ok {
      bail!(self.report(at, &format!("the turn ended as {result:?}")));
    }
    Ok(())
  }

  /// The next unmatched output event; fails if the turn ends without one,
  /// or stalls while the agent waits on a request the case hasn't answered.
  async fn next_output(&mut self, at: &Location<'_>, what: &str) -> Result<Event> {
    let deadline = Instant::now() + WAIT;
    let mut progress = Instant::now();
    loop {
      if let Some(event) = self.output.pop_front() {
        return Ok(event);
      }
      // Check the turn BEFORE fetching: once it has ended, one fetch sees
      // everything it journaled.
      let ended = self.turn_ended().await?;
      let seen = self.log.len();
      self.fetch().await?;
      if self.log.len() > seen {
        progress = Instant::now();
      }
      if !self.output.is_empty() {
        continue;
      }
      if ended {
        bail!(self.report(at, &format!("expected {what}, but the turn ended")));
      }
      if progress.elapsed() > BLOCKED {
        if self.held.is_none() {
          self.held = self.model.try_next();
        }
        if let Some(incoming) = &self.held {
          let blocked = unanswered(incoming);
          bail!(self.report(
            at,
            &format!("expected {what}, but the agent is blocked: {blocked}")
          ));
        }
      }
      if Instant::now() > deadline {
        bail!(self.report(at, &format!("timed out waiting for {what}")));
      }
      tokio::time::sleep(POLL).await;
    }
  }

  // ---- turn and journal state -----------------------------------------------

  /// Whether no turn is running. Reaps a finished turn; a dispatch error
  /// (the turn couldn't run at all) fails the case.
  async fn turn_ended(&mut self) -> Result<bool> {
    match &self.turn {
      None => Ok(true),
      Some(turn) if turn.is_finished() => {
        let turn = self.turn.take().expect("checked above");
        let result = turn.await?.map_err(|e| anyhow!("dispatch failed: {e}"))?;
        if self.mode == Mode::Gated && result == TurnResult::AwaitingApproval {
          self.auto_approve().await?;
          return Ok(false);
        }
        self.ended = Some(result);
        Ok(true)
      }
      Some(_) => Ok(false),
    }
  }

  /// [`Mode::Gated`]: the turn suspended. Drop its `TOOL_APPROVAL_REQUIRED`
  /// (the last output) and approve, continuing the turn.
  async fn auto_approve(&mut self) -> Result<()> {
    let at = &"gated mode, approving";
    self.fetch().await?;
    match self.output.pop_back() {
      Some(event) if event.event_type == EventType::ToolApprovalRequired => {}
      other => {
        let last = other.as_ref().map(line).unwrap_or_else(|| "nothing".into());
        bail!(self.report(
          at,
          &format!("the turn suspended, but its last output is {last}, not TOOL_APPROVAL_REQUIRED")
        ));
      }
    }
    self.approvals += 1;
    let session = self.session.expect("a turn ran");
    let dispatch = self.dispatch.clone();
    self.replay_from = self.last_prefix.take();
    self.turn = Some(tokio::spawn(async move {
      let decision = ApprovalDecision {
        approved: true,
        approver: None,
        extra: Default::default(),
      };
      dispatch.resume_with_decision(session, decision).await
    }));
    Ok(())
  }

  /// How the current turn ended, waiting for it if it's still running.
  async fn await_turn(&mut self, at: &Location<'_>) -> Result<TurnResult> {
    let deadline = Instant::now() + WAIT;
    while !self.turn_ended().await? {
      if Instant::now() > deadline {
        bail!(self.report(at, "timed out waiting for the turn to end"));
      }
      tokio::time::sleep(POLL).await;
    }
    self
      .ended
      .take()
      .ok_or_else(|| anyhow!("{at}: no turn has ended since the last check"))
  }

  /// Pull newly journaled events; turn output joins the unmatched queue.
  async fn fetch(&mut self) -> Result<()> {
    let Some(session) = self.session else {
      return Ok(());
    };
    for event in self
      .dispatch
      .journal()
      .fetch(session, self.seen_seq)
      .await?
    {
      self.seen_seq = Some(event.seq);
      if !is_input(event.event_type) {
        self.output.push_back(event.clone());
      }
      self.log.push(event);
    }
    Ok(())
  }

  /// No turn running, no unmatched output, no unanswered request.
  async fn settled(&mut self, at: &(dyn std::fmt::Display + Sync)) -> Result<()> {
    if !self.turn_ended().await? {
      bail!(self.report(at, "the previous turn is still running"));
    }
    self.fetch().await?;
    if !self.output.is_empty() {
      let unmatched: Vec<String> = self.output.iter().map(line).collect();
      bail!(self.report(
        at,
        &format!(
          "unmatched journal output:\n    {}",
          unmatched.join("\n    ")
        )
      ));
    }
    if let Some(incoming) = self.take_pending() {
      bail!(self.report(at, &unanswered(&incoming)));
    }
    Ok(())
  }

  /// A request waiting for the case, held or still on the channel.
  fn take_pending(&mut self) -> Option<mock::Incoming> {
    self.held.take().or_else(|| self.model.try_next())
  }

  // ---- reporting ------------------------------------------------------------

  fn mismatch(&self, at: &(dyn std::fmt::Display + Sync), expected: &str, got: &Event) -> String {
    self.report(
      at,
      &format!(
        "journal mismatch\n  expected: {expected}\n  actual:   {}",
        line(got)
      ),
    )
  }

  /// `at: problem`, then the journal so far.
  fn report(&self, at: &(dyn std::fmt::Display + Sync), problem: &str) -> String {
    let mut out = format!("{at}: {problem}\n  journal so far:");
    for event in &self.log {
      let _ = write!(out, "\n    {}", line(event));
    }
    out
  }
}

/// Whether a single-event pattern matches `event`.
fn single_matches(ev: &Ev, event: &Event) -> bool {
  match (ev, &event.payload) {
    (Ev::ToolCall { id, name, args }, EventPayload::ToolCall(c)) => {
      c.tool_call_id == *id && c.name == *name && c.arguments == *args
    }
    (Ev::ToolResult { id, result }, EventPayload::ToolResult(r)) => {
      r.tool_call_id == *id && r.result == *result
    }
    (Ev::Error { contains }, EventPayload::Error(e)) => e.message.contains(contains.as_str()),
    (Ev::ErrorCode { code }, EventPayload::Error(e)) => e.code.as_deref() == Some(code.as_str()),
    (Ev::SessionError { contains }, EventPayload::SessionError(e)) => {
      e.message.contains(contains.as_str())
    }
    (Ev::ApprovalRequired { id, name, args }, EventPayload::ToolApprovalRequired(r)) => {
      r.tool_call_id == *id && r.name == *name && r.arguments == *args
    }
    _ => false,
  }
}

fn unanswered(incoming: &mock::Incoming) -> String {
  match incoming {
    Ok(request) => format!("unanswered: {}", request.describe()),
    Err(reason) => format!("the mock rejected a request: {reason}"),
  }
}

/// The journal events for one seeded item.
fn seed_payloads(item: Seed, route: &str, deployment: uuid::Uuid, out: &mut Vec<EventPayload>) {
  use sectr_common::events as e;
  match item {
    Seed::User(input) => out.push(EventPayload::InvocationStarted(e::InvocationStarted {
      invocation_id: uuid::Uuid::now_v7(),
      route: route.into(),
      deployment_id: deployment,
      reason: e::InvocationReason::UserMessage {
        input: input.to_string().try_into().unwrap(),
        extra: Default::default(),
      },
      extra: Default::default(),
    })),
    Seed::Assistant {
      reasoning,
      text,
      closed,
    } => {
      let message_id = uuid::Uuid::now_v7().simple().to_string();
      out.push(EventPayload::MessageStart(e::MessageStart {
        role: "assistant".into(),
        message_id: message_id.clone(),
        extra: Default::default(),
      }));
      let delta = |delta: TypedDelta| {
        EventPayload::MessageDelta(e::MessageDelta {
          message_id: message_id.clone(),
          delta,
          extra: Default::default(),
        })
      };
      if !reasoning.is_empty() {
        out.push(delta(TypedDelta::Reasoning {
          text: reasoning,
          extra: Default::default(),
        }));
      }
      if !text.is_empty() {
        out.push(delta(TypedDelta::Text {
          text,
          extra: Default::default(),
        }));
      }
      if closed {
        out.push(EventPayload::MessageEnd(e::MessageEnd {
          message_id,
          finish_reason: None,
          framework_state: None,
          extra: Default::default(),
        }));
      }
    }
    Seed::ToolCall { id, name, args } => out.push(EventPayload::ToolCall(e::ToolCall {
      tool_call_id: id,
      name,
      arguments: args,
      extra: Default::default(),
    })),
    Seed::ToolResult { id, result } => out.push(EventPayload::ToolResult(e::ToolResult {
      tool_call_id: id,
      result,
      extra: Default::default(),
    })),
    Seed::ApprovalRequired { id, name, args } => out.push(EventPayload::ToolApprovalRequired(
      e::ToolApprovalRequired {
        tool_call_id: id,
        name,
        arguments: args,
        reason: None,
        extra: Default::default(),
      },
    )),
    Seed::Error(message) => out.push(EventPayload::Error(e::Error {
      message,
      code: None,
      transient: None,
      extra: Default::default(),
    })),
    Seed::SessionError(message) => out.push(EventPayload::SessionError(e::SessionError {
      message,
      extra: Default::default(),
    })),
    Seed::TurnEnd => out.push(EventPayload::NoMoreActions(e::NoMoreActions {
      extra: Default::default(),
    })),
  }
}

/// Events the case journals itself, rather than the turn's output.
fn is_input(t: EventType) -> bool {
  matches!(
    t,
    EventType::SessionCreated
      | EventType::InvocationStarted
      | EventType::ToolApprovalGranted
      | EventType::ToolApprovalDenied
  )
}

/// `#seq TYPE payload` for reports.
fn delta_text(delta: &TypedDelta) -> &str {
  match delta {
    TypedDelta::Text { text, .. } | TypedDelta::Reasoning { text, .. } => text,
  }
}

fn line(event: &Event) -> String {
  let payload = serde_json::to_value(event)
    .ok()
    .and_then(|v| v.get("payload").cloned())
    .unwrap_or_default();
  format!("#{} {} {payload}", event.seq, event.event_type)
}
