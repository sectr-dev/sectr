//! The mock model and tool backend.
//!
//! One server per harness process. Each case run registers under a run id and
//! the agent reaches it under `<base>/cases/<run>`:
//!
//! - `/v1/...`, the model, served as the run's [`Endpoint`]: a request to
//!   its path is parsed into the common history, handed to the case as a
//!   [`ModelCall`], and held open until the case replies; the reply is then
//!   streamed back in the endpoint's encoding. A request to any other path
//!   is rejected, so a client on the wrong API fails the case.
//! - `/tools/<name>`, which the generated tool stubs POST their arguments to.
//!   The execution is handed to the case as a [`ToolExec`] and held open
//!   until the case decides its outcome: return, raise, or exit.
//! - `/mcp`, an MCP server (streamable HTTP, [`mcp`]) serving the agent's
//!   MCP tools: a `tools/call` reaches the case as a [`ToolExec`] too.
//!
//! All arrive on one channel, in the order the agent made them.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow, bail};
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{any, post};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use crate::agent::ToolSpec;
use crate::endpoint::Endpoint;
use crate::msg::Msg;

mod anthropic;
mod chat_completions;
mod mcp;
mod responses;
mod validate;

/// What the mock hands a case: a request awaiting a reply, or a request it
/// rejected before the case saw it (a harness or adapter bug the case should
/// fail on).
pub type Incoming = std::result::Result<Request, String>;

/// A request from the agent, held open until the case answers it.
pub enum Request {
  Model(ModelCall),
  Tool(ToolExec),
}

impl Request {
  /// One line for failure reports.
  pub fn describe(&self) -> String {
    match self {
      Request::Model(call) => format!("a model request with history {:?}", call.history()),
      Request::Tool(exec) => format!("a tool execution {}({})", exec.name, exec.args),
    }
  }
}

#[derive(Default)]
struct Shared {
  runs: Mutex<HashMap<String, RunState>>,
}

#[derive(Clone)]
struct RunState {
  tx: mpsc::UnboundedSender<Incoming>,
  endpoint: Endpoint,
  /// The reasoning signatures this run's replies carried, in order.
  signatures: Arc<Mutex<Vec<String>>>,
  /// How many replies the run has sent: numbers each reply's ids, unique
  /// like a provider's (LangGraph merges messages that share an id).
  replies: Arc<std::sync::atomic::AtomicUsize>,
  /// Tool executions answered on arrival ([`Run::answer_ahead`]), each
  /// used once.
  ahead: Arc<Mutex<Vec<ToolAnswer>>>,
  /// The run's MCP server, if the agent has MCP tools.
  mcp: Option<mcp::Service>,
}

pub struct MockServer {
  addr: SocketAddr,
  shared: Arc<Shared>,
  task: tokio::task::JoinHandle<()>,
}

impl MockServer {
  /// Bind to an ephemeral localhost port and start serving.
  pub async fn start() -> Result<Self> {
    let shared = Arc::new(Shared::default());
    let app = Router::new()
      .route("/cases/{run}/v1/{*path}", post(model_request))
      .route("/cases/{run}/tools/{name}", post(tool))
      .route("/cases/{run}/mcp", any(mcp::request))
      .with_state(shared.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let task = tokio::spawn(async move {
      axum::serve(listener, app).await.expect("mock model server");
    });
    Ok(MockServer { addr, shared, task })
  }

  /// Register a case run, serving the model as `endpoint` and `mcp_tools`
  /// over MCP. The agent's `SECTR_CONFORMANCE_URL` is [`Run::base_url`];
  /// its requests arrive on [`Run::next`].
  pub fn register(&self, run: &str, endpoint: Endpoint, mcp_tools: &'static [ToolSpec]) -> Run {
    let (tx, rx) = mpsc::unbounded_channel();
    let ahead: Arc<Mutex<Vec<ToolAnswer>>> = Arc::default();
    let mcp = (!mcp_tools.is_empty()).then(|| mcp::service(mcp_tools, tx.clone(), ahead.clone()));
    self.shared.runs.lock().unwrap().insert(
      run.into(),
      RunState {
        tx,
        endpoint,
        signatures: Arc::default(),
        replies: Arc::default(),
        ahead: ahead.clone(),
        mcp,
      },
    );
    Run {
      id: run.into(),
      base_url: format!("http://{}/cases/{run}", self.addr),
      rx,
      ahead,
      shared: self.shared.clone(),
    }
  }
}

impl Drop for MockServer {
  fn drop(&mut self) {
    self.task.abort();
  }
}

/// A registered case run. Unregisters on drop.
pub struct Run {
  id: String,
  base_url: String,
  rx: mpsc::UnboundedReceiver<Incoming>,
  ahead: Arc<Mutex<Vec<ToolAnswer>>>,
  shared: Arc<Shared>,
}

impl Run {
  pub fn base_url(&self) -> &str {
    &self.base_url
  }

  /// Answer these executions as soon as they arrive, without the case: a
  /// tool that finishes at once. Each answers one execution; a repeat
  /// reaches the case like any other.
  pub fn answer_ahead(&self, answers: impl IntoIterator<Item = ToolAnswer>) {
    self.ahead.lock().unwrap().extend(answers);
  }

  /// The ahead answers no execution used yet, as `name(args)`.
  pub fn unused_ahead(&self) -> Vec<String> {
    let ahead = self.ahead.lock().unwrap();
    ahead
      .iter()
      .map(|a| format!("{}({})", a.name, a.args))
      .collect()
  }

  /// The next request, or `None` if the server is gone.
  pub async fn next(&mut self) -> Option<Incoming> {
    self.rx.recv().await
  }

  /// A request already waiting, if any.
  pub fn try_next(&mut self) -> Option<Incoming> {
    self.rx.try_recv().ok()
  }
}

impl Drop for Run {
  fn drop(&mut self) {
    self.shared.runs.lock().unwrap().remove(&self.id);
  }
}

/// One model request, held open until [`ModelCall::reply`]. Dropping it
/// without replying fails the request with a 500.
pub struct ModelCall {
  request: ModelRequest,
  endpoint: Endpoint,
  /// Expect the SDK's one-call-per-message layout
  /// ([`ModelCall::one_call_per_message`]).
  one_call_per_message: bool,
  reply: oneshot::Sender<Answer>,
}

/// How the mock answers a model request.
enum Answer {
  Reply(Reply),
  Fault(Fault),
}

/// A failed model request, as providers fail them.
#[derive(Debug, Clone)]
pub enum Fault {
  /// An HTTP error status with the endpoint's error body, before any
  /// stream (500, 429, ...).
  Status(u16),
  /// Stream this partial reply, then close cleanly: no `finish_reason`, no
  /// `[DONE]`.
  Truncated(Reply),
  /// Stream this partial reply, then reset the connection.
  Reset(Reply),
  /// Stream this partial reply, then an error chunk in the stream (the
  /// provider failing mid-generation), and close.
  ErrorChunk(Reply),
}

pub fn status(code: u16) -> Fault {
  Fault::Status(code)
}

pub fn truncated(partial: Reply) -> Fault {
  Fault::Truncated(partial)
}

pub fn reset(partial: Reply) -> Fault {
  Fault::Reset(partial)
}

pub fn error_chunk(partial: Reply) -> Fault {
  Fault::ErrorChunk(partial)
}

impl ModelCall {
  /// The conversation without the system prompt (see [`crate::msg`]).
  pub fn history(&self) -> &[Msg] {
    &self.request.history
  }

  /// The system/developer messages, in order.
  pub fn system(&self) -> &[String] {
    &self.request.system
  }

  /// Whether the client asked for a stream.
  pub fn streamed(&self) -> bool {
    self.request.stream
  }

  /// The names of the tools the request offers, in order.
  pub fn tools(&self) -> Vec<&str> {
    self.request.tools.iter().map(String::as_str).collect()
  }

  /// The request body as sent, parsed (object keys sorted).
  pub fn raw(&self) -> &Value {
    &self.request.raw
  }

  /// The request body as sent, as text: object keys in the client's order.
  pub fn text(&self) -> &str {
    &self.request.text
  }

  /// Expect `expected` histories in [`crate::msg::one_call_per_message`]'s
  /// layout (an SDK's own layout; the case states the conversation).
  pub(crate) fn one_call_per_message(mut self, on: bool) -> Self {
    self.one_call_per_message = on;
    self
  }

  /// Compare the history with `expected`, exactly. Tool results are
  /// expected in call order whatever order the tools completed in: every
  /// SDK sends them that way, and some providers match them by position.
  #[track_caller]
  pub fn expect_history(self, expected: &[Msg]) -> Result<Self> {
    let at = std::panic::Location::caller();
    let expected = if self.one_call_per_message {
      crate::msg::one_call_per_message(expected)
    } else {
      expected.to_vec()
    };
    if self.history() != expected {
      bail!(
        "{at}: model request history mismatch\n  expected: {expected:?}\n  actual:   {:?}",
        self.history()
      );
    }
    Ok(self)
  }

  /// Stream `reply` back to the agent. Fails for a reply the endpoint
  /// can't produce: reasoning on [`Endpoint::OpenAiChat`].
  pub async fn reply(self, reply: Reply) -> Result<()> {
    self.answer(Answer::Reply(reply))
  }

  /// Fail the request with `fault`.
  pub async fn fail(self, fault: Fault) -> Result<()> {
    self.answer(Answer::Fault(fault))
  }

  fn answer(self, answer: Answer) -> Result<()> {
    let reasoning = match &answer {
      Answer::Reply(r)
      | Answer::Fault(Fault::Truncated(r) | Fault::Reset(r) | Fault::ErrorChunk(r)) => {
        !r.reasoning.is_empty()
      }
      Answer::Fault(Fault::Status(_)) => false,
    };
    if self.endpoint == Endpoint::OpenAiChat && reasoning {
      bail!(
        "the openai_chat endpoint returns no reasoning; mark the case unsupported on \
         Endpoint::OpenAiChat"
      );
    }
    self
      .reply
      .send(answer)
      .map_err(|_| anyhow!("the agent hung up before the model replied"))
  }
}

/// One model response: reasoning, text, tool calls, in that order. Each
/// endpoint encodes it its own way.
#[derive(Debug, Clone, Default)]
pub struct Reply {
  /// Streamed first, one chunk per element.
  pub reasoning: Vec<String>,
  /// Streamed one content chunk per element.
  pub text: Vec<String>,
  /// Streamed after the text, each call's arguments split across chunks.
  pub calls: Vec<CallReply>,
}

#[derive(Debug, Clone)]
pub struct CallReply {
  pub id: String,
  pub name: String,
  pub args: Value,
  /// The arguments exactly as the model writes them, if not `args`'
  /// compact JSON (which sorts keys).
  pub text: Option<String>,
}

impl CallReply {
  /// The arguments as the model writes them.
  pub fn arguments(&self) -> String {
    self.text.clone().unwrap_or_else(|| self.args.to_string())
  }
}

impl Reply {
  /// This reply plus `other`'s parts. Reasoning always streams first, then
  /// text, then calls: `text("Let me check.").and(call(..)).and(call(..))`.
  pub fn and(mut self, other: Reply) -> Reply {
    self.reasoning.extend(other.reasoning);
    self.text.extend(other.text);
    self.calls.extend(other.calls);
    self
  }
}

/// A reply with one tool call.
pub fn call(id: &str, name: &str, args: Value) -> Reply {
  Reply {
    calls: vec![CallReply {
      id: id.into(),
      name: name.into(),
      args,
      text: None,
    }],
    ..Reply::default()
  }
}

/// A reply with one tool call whose arguments the model writes as `text`
/// (JSON, in whatever key order and spacing), not as compact JSON. A
/// non-streamed Anthropic reply sends them as an object, which can't keep
/// the text.
pub fn call_text(id: &str, name: &str, text: &str) -> Reply {
  let args = serde_json::from_str(text).expect("the arguments are JSON");
  Reply {
    calls: vec![CallReply {
      id: id.into(),
      name: name.into(),
      args,
      text: Some(text.into()),
    }],
    ..Reply::default()
  }
}

/// A text reply, streamed a word at a time.
pub fn text(s: &str) -> Reply {
  Reply {
    text: words(s),
    ..Reply::default()
  }
}

/// A reasoning reply, streamed a word at a time.
pub fn reasoning(s: &str) -> Reply {
  Reply {
    reasoning: words(s),
    ..Reply::default()
  }
}

/// `s` in chunks of a word each, keeping its leading whitespace, so the
/// chunks concatenate back to `s`.
fn words(s: &str) -> Vec<String> {
  let mut chunks = Vec::new();
  let mut start = 0;
  for (i, c) in s.char_indices() {
    if c.is_whitespace() && i > start && !s[start..i].trim().is_empty() {
      chunks.push(s[start..i].to_string());
      start = i;
    }
  }
  if start < s.len() {
    chunks.push(s[start..].to_string());
  }
  chunks
}

/// One tool execution, held open until the case decides its outcome
/// ([`crate::case::Case::tool`]). Dropping it without deciding fails the
/// stub with a 500.
pub struct ToolExec {
  name: String,
  args: Value,
  reply: oneshot::Sender<ToolOutcome>,
}

/// An expected tool execution, for [`crate::case::Case::tools`]; give it
/// an outcome with [`Expected::returns`] and friends.
pub fn exec(name: &str, args: Value) -> Expected {
  Expected {
    name: name.into(),
    args,
  }
}

pub struct Expected {
  name: String,
  args: Value,
}

impl Expected {
  pub fn returns(self, value: Value) -> ToolAnswer {
    self.answer(ToolOutcome::Return(value))
  }

  pub fn raises(self, message: &str) -> ToolAnswer {
    self.answer(ToolOutcome::Raise(message.into()))
  }

  pub fn exits(self, code: i32) -> ToolAnswer {
    self.answer(ToolOutcome::Exit(code))
  }

  /// An MCP tool's result, exactly: these text blocks, and optionally
  /// `structuredContent`.
  pub fn returns_mcp(self, text: &[&str], structured: Option<Value>) -> ToolAnswer {
    self.answer(ToolOutcome::Mcp {
      text: text.iter().map(|t| (*t).to_string()).collect(),
      structured,
    })
  }

  fn answer(self, outcome: ToolOutcome) -> ToolAnswer {
    ToolAnswer {
      name: self.name,
      args: self.args,
      outcome,
    }
  }
}

/// An expected execution and the outcome to give it.
pub struct ToolAnswer {
  pub name: String,
  pub args: Value,
  pub outcome: ToolOutcome,
}

/// What the tool stub does with an execution.
#[derive(Debug, Clone)]
pub enum ToolOutcome {
  Return(Value),
  /// Raise an exception with this message.
  Raise(String),
  /// `os._exit` with this code: the runner dies mid-tool.
  Exit(i32),
  /// An MCP tool's result as given (a stub can't return one). `Return`
  /// gives an MCP tool one text block instead.
  Mcp {
    text: Vec<String>,
    structured: Option<Value>,
  },
}

impl ToolExec {
  pub fn name(&self) -> &str {
    &self.name
  }

  /// The arguments the stub was called with.
  pub fn args(&self) -> &Value {
    &self.args
  }

  pub(crate) fn decide(self, outcome: ToolOutcome) -> Result<()> {
    self
      .reply
      .send(outcome)
      .map_err(|_| anyhow!("the agent hung up before the tool returned"))
  }
}

// ---- the server -------------------------------------------------------------

/// A request parsed by the run's endpoint.
struct ModelRequest {
  system: Vec<String>,
  history: Vec<Msg>,
  /// The offered tools' names, in order.
  tools: Vec<String>,
  /// Whether the client asked for a stream. A subagent's run often
  /// doesn't (openai-agents `Runner.run`, pydantic-ai `run()`).
  stream: bool,
  raw: Value,
  /// The body as sent: `raw` sorts object keys, this keeps their order.
  text: String,
}

async fn model_request(
  State(shared): State<Arc<Shared>>,
  Path((run, path)): Path<(String, String)>,
  body: Bytes,
) -> Response {
  let Some(RunState {
    tx,
    endpoint,
    signatures,
    replies,
    ..
  }) = shared.runs.lock().unwrap().get(&run).cloned()
  else {
    return error(
      StatusCode::NOT_FOUND,
      &format!("no case run `{run}` is registered"),
    );
  };
  let path = format!("/v1/{path}");
  let parsed = if path == endpoint.path() {
    serde_json::from_slice::<Value>(&body)
      .map_err(|e| format!("request body is not JSON: {e}"))
      .and_then(|raw| match endpoint {
        Endpoint::OpenAiChat | Endpoint::OpenRouter => {
          chat_completions::parse(raw, endpoint, &signatures.lock().unwrap())
        }
        Endpoint::OpenAiResponses => responses::parse(raw, &signatures.lock().unwrap()),
        Endpoint::Anthropic => anthropic::parse(raw, &signatures.lock().unwrap()),
      })
  } else {
    Err(format!(
      "a request to {path}, but this run serves the {} endpoint ({})",
      endpoint.id(),
      endpoint.path()
    ))
  };
  let request = match parsed {
    Ok(request) => ModelRequest {
      text: String::from_utf8_lossy(&body).into_owned(),
      ..request
    },
    Err(reason) => {
      let _ = tx.send(Err(reason.clone()));
      return model_error(endpoint, StatusCode::BAD_REQUEST, &reason);
    }
  };
  let (raw, stream) = (request.raw.clone(), request.stream);

  let (reply_tx, reply_rx) = oneshot::channel();
  if tx
    .send(Ok(Request::Model(ModelCall {
      request,
      endpoint,
      one_call_per_message: false,
      reply: reply_tx,
    })))
    .is_err()
  {
    return error(
      StatusCode::NOT_FOUND,
      &format!("case run `{run}` has ended"),
    );
  }
  let (reply, end) = match reply_rx.await {
    Ok(Answer::Reply(reply)) => (reply, End::Finish),
    Ok(Answer::Fault(Fault::Status(code))) => {
      let status = StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
      return model_error(endpoint, status, &format!("mock fault: HTTP {code}"));
    }
    Ok(Answer::Fault(Fault::Truncated(partial))) => (partial, End::Truncate),
    Ok(Answer::Fault(Fault::Reset(partial))) => (partial, End::Reset),
    Ok(Answer::Fault(Fault::ErrorChunk(partial))) => (partial, End::ErrorChunk),
    Err(_) => {
      return error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "the case dropped this model request without replying",
      );
    }
  };
  // Opaque and unique per reply, like a real one: OpenRouter's or
  // Anthropic's signature, or the Responses reasoning item's
  // `encrypted_content`.
  let signature = (!reply.reasoning.is_empty()).then(|| {
    let mut issued = signatures.lock().unwrap();
    let signature = format!("mock-signature-{run}-{}", issued.len() + 1);
    issued.push(signature.clone());
    signature
  });
  let n = replies.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
  if !stream {
    if end != End::Finish {
      return model_error(
        endpoint,
        StatusCode::INTERNAL_SERVER_ERROR,
        "the mock only cuts streamed replies short",
      );
    }
    let body = match endpoint {
      Endpoint::OpenAiChat | Endpoint::OpenRouter => {
        chat_completions::complete(&reply, &raw, endpoint, signature.as_deref(), n)
      }
      Endpoint::OpenAiResponses => responses::complete(&reply, &raw, signature.as_deref(), n),
      Endpoint::Anthropic => anthropic::complete(&reply, &raw, signature.as_deref(), n),
    };
    return axum::Json(body).into_response();
  }
  match endpoint {
    Endpoint::OpenAiChat | Endpoint::OpenRouter => {
      chat_completions::stream(&reply, &raw, endpoint, signature.as_deref(), end, n)
    }
    Endpoint::OpenAiResponses => responses::stream(&reply, &raw, signature.as_deref(), end, n),
    Endpoint::Anthropic => anthropic::stream(&reply, &raw, signature.as_deref(), end, n),
  }
}

/// An SSE response of `frames`, then a connection reset if `reset` (after
/// the content has gone out).
fn sse(frames: Vec<String>, reset: bool) -> Response {
  use futures::StreamExt;
  let content = futures::stream::iter(
    frames
      .into_iter()
      .map(|f| Ok::<_, std::io::Error>(Bytes::from(f))),
  );
  let body = if reset {
    let reset = futures::stream::once(async {
      tokio::time::sleep(std::time::Duration::from_millis(100)).await;
      Err(std::io::Error::new(
        std::io::ErrorKind::ConnectionReset,
        "mock fault: connection reset",
      ))
    });
    axum::body::Body::from_stream(content.chain(reset))
  } else {
    axum::body::Body::from_stream(content)
  };
  (
    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
    body,
  )
    .into_response()
}

/// How a streamed reply ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
  /// `finish_reason`, usage, `[DONE]`.
  Finish,
  /// A clean close right after the content.
  Truncate,
  /// A connection reset right after the content.
  Reset,
  /// An error chunk after the content, then a clean close.
  ErrorChunk,
}

/// A tool stub's request: its arguments as a JSON object. Answers
/// `{"return": v}`, `{"raise": message}` or `{"exit": code}`.
async fn tool(
  State(shared): State<Arc<Shared>>,
  Path((run, name)): Path<(String, String)>,
  body: Bytes,
) -> Response {
  let Some(RunState { tx, ahead, .. }) = shared.runs.lock().unwrap().get(&run).cloned() else {
    return error(
      StatusCode::NOT_FOUND,
      &format!("no case run `{run}` is registered"),
    );
  };
  let args = match serde_json::from_slice::<Value>(&body) {
    Ok(args) => args,
    Err(e) => {
      let reason = format!("tool `{name}`: arguments are not JSON: {e}");
      let _ = tx.send(Err(reason.clone()));
      return error(StatusCode::BAD_REQUEST, &reason);
    }
  };
  match execute(&tx, &ahead, name, args).await {
    Ok(ToolOutcome::Mcp { .. }) => error(
      StatusCode::INTERNAL_SERVER_ERROR,
      "the case gave a tool stub an MCP result",
    ),
    Ok(outcome) => axum::Json(outcome_body(outcome)).into_response(),
    Err(Unanswered::Ended) => error(
      StatusCode::NOT_FOUND,
      &format!("case run `{run}` has ended"),
    ),
    Err(Unanswered::Dropped) => error(
      StatusCode::INTERNAL_SERVER_ERROR,
      "the case dropped this tool execution without deciding it",
    ),
  }
}

/// Why an execution got no outcome.
enum Unanswered {
  /// The case run has ended.
  Ended,
  /// The case dropped the execution without deciding it.
  Dropped,
}

/// A tool execution, from a stub or over MCP: answered ahead if an answer
/// matches ([`Run::answer_ahead`]), otherwise handed to the case and held
/// until it decides.
async fn execute(
  tx: &mpsc::UnboundedSender<Incoming>,
  ahead: &Mutex<Vec<ToolAnswer>>,
  name: String,
  args: Value,
) -> std::result::Result<ToolOutcome, Unanswered> {
  let decided = {
    let mut ahead = ahead.lock().unwrap();
    let i = ahead.iter().position(|a| a.name == name && a.args == args);
    i.map(|i| ahead.remove(i).outcome)
  };
  if let Some(outcome) = decided {
    return Ok(outcome);
  }
  let (reply_tx, reply_rx) = oneshot::channel();
  let exec = ToolExec {
    name,
    args,
    reply: reply_tx,
  };
  if tx.send(Ok(Request::Tool(exec))).is_err() {
    return Err(Unanswered::Ended);
  }
  reply_rx.await.map_err(|_| Unanswered::Dropped)
}

fn outcome_body(outcome: ToolOutcome) -> Value {
  match outcome {
    ToolOutcome::Return(value) => json!({"return": value}),
    ToolOutcome::Raise(message) => json!({"raise": message}),
    ToolOutcome::Exit(code) => json!({"exit": code}),
    ToolOutcome::Mcp { .. } => unreachable!("answered as an error"),
  }
}

/// A model request's error response, in the endpoint's shape.
fn model_error(endpoint: Endpoint, status: StatusCode, message: &str) -> Response {
  match endpoint {
    Endpoint::OpenAiChat | Endpoint::OpenRouter | Endpoint::OpenAiResponses => {
      error(status, message)
    }
    Endpoint::Anthropic => {
      let kind = match status.as_u16() {
        400 => "invalid_request_error",
        404 => "not_found_error",
        429 => "rate_limit_error",
        529 => "overloaded_error",
        _ => "api_error",
      };
      let body = json!({"type": "error", "error": {"type": kind, "message": message}});
      (status, axum::Json(body)).into_response()
    }
  }
}

/// An OpenAI-style error response.
fn error(status: StatusCode, message: &str) -> Response {
  let body = json!({"error": {"message": message, "type": "invalid_request_error"}});
  (status, axum::Json(body)).into_response()
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::msg::{assistant, thought, tool_call, tool_result, user};
  use chat_completions::CLAUDE_FORMAT;

  fn model(incoming: Option<Incoming>) -> ModelCall {
    match incoming.unwrap() {
      Ok(Request::Model(call)) => call,
      Ok(other) => panic!("expected a model request, got {}", other.describe()),
      Err(reason) => panic!("rejected: {reason}"),
    }
  }

  /// POST `body` and return the response text, in the background.
  fn spawn_post(url: String, body: Value) -> tokio::task::JoinHandle<String> {
    tokio::spawn(async move {
      let resp = reqwest::Client::new()
        .post(url)
        .json(&body)
        .send()
        .await
        .unwrap();
      resp.text().await.unwrap()
    })
  }

  /// The chunks of an SSE body, without the final `[DONE]`.
  fn chunks(body: &str) -> Vec<Value> {
    let lines = data_lines(body);
    assert_eq!(lines.last().unwrap(), "[DONE]");
    lines[..lines.len() - 1]
      .iter()
      .map(|l| serde_json::from_str(l).unwrap())
      .collect()
  }

  async fn post(run: &Run, body: Value) -> reqwest::Response {
    reqwest::Client::new()
      .post(format!("{}/v1/chat/completions", run.base_url()))
      .json(&body)
      .send()
      .await
      .unwrap()
  }

  fn request(messages: Value) -> Value {
    json!({"model": "mock", "stream": true, "messages": messages})
  }

  /// The `data:` payloads of an SSE body.
  fn data_lines(body: &str) -> Vec<String> {
    body
      .lines()
      .filter_map(|l| l.strip_prefix("data: "))
      .map(str::to_owned)
      .collect()
  }

  #[test]
  fn and_puts_text_before_calls() {
    let r = call("c1", "f", json!({}))
      .and(text("Hi there"))
      .and(call("c2", "g", json!(1)));
    assert_eq!(r.text, ["Hi", " there"]);
    let ids: Vec<&str> = r.calls.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, ["c1", "c2"]);
  }

  #[test]
  fn text_chunks_by_word_and_concatenates_back() {
    let r = text("Hello, there.  How are you?");
    assert_eq!(r.text, ["Hello,", " there.", "  How", " are", " you?"]);
    assert_eq!(r.text.concat(), "Hello, there.  How are you?");
  }

  #[tokio::test]
  async fn a_request_reaches_the_case_and_the_reply_streams_back() {
    let server = MockServer::start().await.unwrap();
    let mut run = server.register("r1", Endpoint::OpenAiChat, &[]);
    let body = request(json!([
      {"role": "system", "content": "Be brief."},
      {"role": "user", "content": "hi"},
      {"role": "assistant", "content": [{"type": "text", "text": "Hello"}]},
      {"role": "user", "content": "again"},
    ]));
    let client = tokio::spawn({
      let url = format!("{}/v1/chat/completions", run.base_url());
      async move {
        let resp = reqwest::Client::new()
          .post(url)
          .json(&body)
          .send()
          .await
          .unwrap();
        assert_eq!(resp.headers()["content-type"], "text/event-stream");
        resp.text().await.unwrap()
      }
    });

    let call = model(run.next().await);
    assert_eq!(call.system(), ["Be brief."]);
    call
      .expect_history(&[user("hi"), assistant("Hello"), user("again")])
      .unwrap()
      .reply(text("Hi again."))
      .await
      .unwrap();

    let lines = data_lines(&client.await.unwrap());
    assert_eq!(lines.last().unwrap(), "[DONE]");
    let chunks: Vec<Value> = lines[..lines.len() - 1]
      .iter()
      .map(|l| serde_json::from_str(l).unwrap())
      .collect();
    let content: String = chunks
      .iter()
      .filter_map(|c| c.pointer("/choices/0/delta/content")?.as_str())
      .collect();
    assert_eq!(content, "Hi again.");
    assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
    assert_eq!(
      chunks.last().unwrap()["choices"][0]["finish_reason"],
      "stop"
    );
  }

  #[tokio::test]
  async fn usage_chunk_only_when_asked_for() {
    let server = MockServer::start().await.unwrap();
    let mut run = server.register("r1", Endpoint::OpenAiChat, &[]);
    let mut body = request(json!([{"role": "user", "content": "hi"}]));
    body["stream_options"] = json!({"include_usage": true});
    let url = format!("{}/v1/chat/completions", run.base_url());
    let client = tokio::spawn(async move {
      let resp = reqwest::Client::new()
        .post(url)
        .json(&body)
        .send()
        .await
        .unwrap();
      resp.text().await.unwrap()
    });
    let call = model(run.next().await);
    call.reply(text("ok")).await.unwrap();
    let lines = data_lines(&client.await.unwrap());
    let usage: Value = serde_json::from_str(&lines[lines.len() - 2]).unwrap();
    assert_eq!(usage["choices"], json!([]));
    assert!(usage["usage"].is_object());
  }

  #[tokio::test]
  async fn rejected_requests_reach_the_case_as_errors() {
    let server = MockServer::start().await.unwrap();
    let mut run = server.register("r1", Endpoint::OpenAiChat, &[]);
    let resp = post(&run, json!({"model": "mock", "stream": true})).await;
    assert_eq!(resp.status(), 400);
    let reason = run.next().await.unwrap().err().unwrap();
    assert!(reason.contains("`messages`"), "{reason}");

    let unknown = json!([{"role": "function", "name": "f", "content": "x"}]);
    assert_eq!(post(&run, request(unknown)).await.status(), 400);
    assert!(run.next().await.unwrap().is_err());

    // A client on another API than the run's endpoint.
    let resp = reqwest::Client::new()
      .post(format!("{}/v1/responses", run.base_url()))
      .json(&json!({"model": "mock", "stream": true, "input": "hi"}))
      .send()
      .await
      .unwrap();
    assert_eq!(resp.status(), 400);
    let reason = run.next().await.unwrap().err().unwrap();
    assert!(
      reason.contains("serves the openai_chat endpoint"),
      "{reason}"
    );
  }

  #[tokio::test]
  async fn unknown_run_is_404_and_a_dropped_call_is_500() {
    let server = MockServer::start().await.unwrap();
    let mut run = server.register("r1", Endpoint::OpenAiChat, &[]);
    let other = server.register("r2", Endpoint::OpenAiChat, &[]);
    let url_other = format!("{}/v1/chat/completions", other.base_url());
    drop(other);
    let resp = reqwest::Client::new()
      .post(url_other)
      .json(&request(json!([])))
      .send()
      .await
      .unwrap();
    assert_eq!(resp.status(), 404);

    let url = format!("{}/v1/chat/completions", run.base_url());
    let client = tokio::spawn(async move {
      let body = request(json!([{"role": "user", "content": "hi"}]));
      reqwest::Client::new()
        .post(url)
        .json(&body)
        .send()
        .await
        .unwrap()
    });
    drop(model(run.next().await));
    assert_eq!(client.await.unwrap().status(), 500);
  }

  #[tokio::test]
  async fn tool_calls_parse_into_history_and_stream_back_in_fragments() {
    let server = MockServer::start().await.unwrap();
    let mut run = server.register("r1", Endpoint::OpenAiChat, &[]);
    let mut body = request(json!([
      {"role": "user", "content": "where is 42?"},
      {"role": "assistant", "content": null, "tool_calls": [{
        "id": "c1", "type": "function",
        "function": {"name": "lookup_order", "arguments": "{\"order_id\": \"42\"}"},
      }]},
      {"role": "tool", "tool_call_id": "c1", "content": "shipped"},
    ]));
    body["tools"] = json!([{"type": "function", "function": {"name": "lookup_order"}}]);
    let client = spawn_post(format!("{}/v1/chat/completions", run.base_url()), body);

    let call = model(run.next().await);
    assert_eq!(call.tools(), ["lookup_order"]);
    call
      .expect_history(&[
        user("where is 42?"),
        tool_call("c1", "lookup_order", json!({"order_id": "42"})),
        tool_result("c1", "shipped"),
      ])
      .unwrap()
      .reply(call_reply())
      .await
      .unwrap();

    let chunks = chunks(&client.await.unwrap());
    assert_eq!(chunks[0]["choices"][0]["delta"]["content"], Value::Null);
    let deltas: Vec<&Value> = chunks
      .iter()
      .filter_map(|c| c.pointer("/choices/0/delta/tool_calls/0"))
      .collect();
    assert_eq!(deltas[0]["id"], "c2");
    assert_eq!(deltas[0]["function"]["name"], "lookup_order");
    let arguments: String = deltas
      .iter()
      .filter_map(|d| d.pointer("/function/arguments")?.as_str())
      .collect();
    assert_eq!(deltas.len(), 3, "a header and two argument fragments");
    assert_eq!(arguments, r#"{"order_id":"43"}"#);
    assert_eq!(
      chunks.last().unwrap()["choices"][0]["finish_reason"],
      "tool_calls"
    );
  }

  fn call_reply() -> Reply {
    call("c2", "lookup_order", json!({"order_id": "43"}))
  }

  #[tokio::test]
  async fn tool_executions_reach_the_case_and_return_its_outcome() {
    let server = MockServer::start().await.unwrap();
    let mut run = server.register("r1", Endpoint::OpenAiChat, &[]);
    let url = format!("{}/tools/lookup_order", run.base_url());
    for (outcome, expected) in [
      (
        ToolOutcome::Return(json!("shipped")),
        json!({"return": "shipped"}),
      ),
      (ToolOutcome::Raise("boom".into()), json!({"raise": "boom"})),
      (ToolOutcome::Exit(3), json!({"exit": 3})),
    ] {
      let client = spawn_post(url.clone(), json!({"order_id": "42"}));
      let Some(Ok(Request::Tool(exec))) = run.next().await else {
        panic!("expected a tool execution");
      };
      assert_eq!(exec.name(), "lookup_order");
      assert_eq!(exec.args(), &json!({"order_id": "42"}));
      exec.decide(outcome).unwrap();
      let got: Value = serde_json::from_str(&client.await.unwrap()).unwrap();
      assert_eq!(got, expected);
    }
  }

  #[tokio::test]
  async fn openrouter_streams_signed_reasoning_and_checks_signatures_sent_back() {
    let server = MockServer::start().await.unwrap();
    let mut run = server.register("r1", Endpoint::OpenRouter, &[]);
    let url = format!("{}/v1/chat/completions", run.base_url());
    let client = spawn_post(
      url.clone(),
      request(json!([{"role": "user", "content": "hi"}])),
    );
    model(run.next().await)
      .reply(reasoning("Just a greeting.").and(call_reply()))
      .await
      .unwrap();
    let body = client.await.unwrap();
    assert!(body.starts_with(": OPENROUTER PROCESSING"), "{body}");
    let chunks = chunks(&body);
    let deltas: Vec<&Value> = chunks.iter().map(|c| &c["choices"][0]["delta"]).collect();
    assert!(deltas.iter().all(|d| d["role"] == "assistant"));
    let reasoning: String = deltas
      .iter()
      .filter_map(|d| d["reasoning"].as_str())
      .collect();
    assert_eq!(reasoning, "Just a greeting.");
    let details: Vec<&Value> = deltas
      .iter()
      .filter_map(|d| d["reasoning_details"].as_array())
      .flatten()
      .collect();
    let detail_text: String = details.iter().filter_map(|d| d["text"].as_str()).collect();
    assert_eq!(detail_text, "Just a greeting.");
    let signature = details.last().unwrap()["signature"]
      .as_str()
      .unwrap()
      .to_owned();
    assert!(details.iter().all(|d| d["format"] == CLAUDE_FORMAT));
    assert!(chunks.last().unwrap()["usage"].is_object());

    // The tool-loop continuation: the signature intact, then modified.
    let continued = |signature: &str| {
      request(json!([
        {"role": "user", "content": "hi"},
        {"role": "assistant", "content": null,
         "reasoning_details": [{"type": "reasoning.text", "text": "Just a greeting.",
                                "signature": signature, "format": CLAUDE_FORMAT, "index": 0}],
         "tool_calls": [{"id": "c2", "type": "function",
                         "function": {"name": "lookup_order", "arguments": "{}"}}]},
        {"role": "tool", "tool_call_id": "c2", "content": "shipped"},
      ]))
    };
    let client = spawn_post(url.clone(), continued(&signature));
    model(run.next().await)
      .expect_history(&[
        user("hi"),
        crate::msg::thought("Just a greeting.", true),
        tool_call("c2", "lookup_order", json!({})),
        tool_result("c2", "shipped"),
      ])
      .unwrap()
      .reply(text("ok"))
      .await
      .unwrap();
    client.await.unwrap();
    assert_eq!(
      post(&run, continued(&format!("{signature}x")))
        .await
        .status(),
      400
    );
    let reason = run.next().await.unwrap().err().unwrap();
    assert!(reason.contains("Invalid `signature`"), "{reason}");
  }

  #[tokio::test]
  async fn openai_chat_refuses_a_reply_with_reasoning() {
    let server = MockServer::start().await.unwrap();
    let mut run = server.register("r1", Endpoint::OpenAiChat, &[]);
    let _client = spawn_post(
      format!("{}/v1/chat/completions", run.base_url()),
      request(json!([{"role": "user", "content": "hi"}])),
    );
    let err = model(run.next().await)
      .reply(reasoning("hmm"))
      .await
      .unwrap_err();
    assert!(err.to_string().contains("returns no reasoning"), "{err}");
  }

  #[tokio::test]
  async fn faults_fail_requests_as_providers_do() {
    let server = MockServer::start().await.unwrap();
    let mut run = server.register("r1", Endpoint::OpenAiChat, &[]);
    let url = format!("{}/v1/chat/completions", run.base_url());
    let body = || request(json!([{"role": "user", "content": "hi"}]));
    let send = |url: String| {
      let body = body();
      tokio::spawn(async move { reqwest::Client::new().post(url).json(&body).send().await })
    };

    let client = send(url.clone());
    model(run.next().await).fail(status(429)).await.unwrap();
    assert_eq!(client.await.unwrap().unwrap().status(), 429);

    let client = send(url.clone());
    model(run.next().await)
      .fail(truncated(text("Partial")))
      .await
      .unwrap();
    let streamed = client.await.unwrap().unwrap().text().await.unwrap();
    assert!(streamed.contains("Partial") && !streamed.contains("[DONE]"));
    assert!(!streamed.contains("finish_reason\":\""), "{streamed}");

    let client = send(url.clone());
    model(run.next().await)
      .fail(error_chunk(text("Partial")))
      .await
      .unwrap();
    let streamed = client.await.unwrap().unwrap().text().await.unwrap();
    let last: Value = serde_json::from_str(data_lines(&streamed).last().unwrap()).unwrap();
    assert_eq!(last["error"]["type"], "server_error");

    let client = send(url);
    model(run.next().await)
      .fail(reset(text("Partial")))
      .await
      .unwrap();
    let response = client.await.unwrap().unwrap();
    assert!(
      response.text().await.is_err(),
      "a reset fails the body read"
    );
  }

  #[tokio::test]
  async fn responses_stream_items_in_the_captured_layout() {
    let server = MockServer::start().await.unwrap();
    let mut run = server.register("r1", Endpoint::OpenAiResponses, &[]);
    let body = json!({"model": "m", "stream": true, "instructions": "Be brief.",
      "tools": [{"type": "function", "name": "lookup_order"}],
      "input": [{"role": "user", "content": "hi"}]});
    let client = spawn_post(format!("{}/v1/responses", run.base_url()), body);
    let call = model(run.next().await);
    assert_eq!(call.system(), ["Be brief."]);
    assert_eq!(call.tools(), ["lookup_order"]);
    call
      .reply(reasoning("Thinking.").and(text("Hi.")).and(call_reply()))
      .await
      .unwrap();
    let body = client.await.unwrap();
    let events: Vec<Value> = data_lines(&body)
      .iter()
      .map(|l| serde_json::from_str(l).unwrap())
      .collect();
    let types: Vec<&str> = events.iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert_eq!(types[..2], ["response.created", "response.in_progress"]);
    assert_eq!(*types.last().unwrap(), "response.completed");
    let output = &events.last().unwrap()["response"]["output"];
    let kinds: Vec<&str> = output
      .as_array()
      .unwrap()
      .iter()
      .map(|o| o["type"].as_str().unwrap())
      .collect();
    assert_eq!(kinds, ["reasoning", "message", "function_call"]);
    assert_eq!(output[0]["summary"][0]["text"], "Thinking.");
    assert!(output[0]["encrypted_content"].is_string());
    assert_eq!(output[1]["content"][0]["text"], "Hi.");
    assert_eq!(output[2]["call_id"], "c2");
    assert_eq!(output[2]["arguments"], r#"{"order_id":"43"}"#);
    let seqs: Vec<u64> = events
      .iter()
      .map(|e| e["sequence_number"].as_u64().unwrap())
      .collect();
    assert!(
      seqs.windows(2).all(|w| w[1] == w[0] + 1),
      "sequence numbers count up"
    );
  }

  #[tokio::test]
  async fn anthropic_streams_signed_thinking_and_parses_it_back() {
    let server = MockServer::start().await.unwrap();
    let mut run = server.register("r1", Endpoint::Anthropic, &[]);
    let url = format!("{}/v1/messages", run.base_url());
    let ask = json!({"role": "user", "content": [{"type": "text", "text": "hi"},
      {"type": "text", "text": "again"}]});
    let body = json!({"model": "m", "stream": true, "system": "Be brief.",
      "tools": [{"name": "lookup_order", "input_schema": {"type": "object"}}],
      "messages": [ask]});
    let client = spawn_post(url.clone(), body);
    let call = model(run.next().await);
    assert_eq!(call.system(), ["Be brief."]);
    assert_eq!(call.tools(), ["lookup_order"]);
    // One message per text block.
    let call = call.expect_history(&[user("hi"), user("again")]).unwrap();
    call
      .reply(reasoning("Thinking.").and(text("Hi.")).and(call_reply()))
      .await
      .unwrap();
    let events: Vec<Value> = data_lines(&client.await.unwrap())
      .iter()
      .map(|l| serde_json::from_str(l).unwrap())
      .collect();
    let types: Vec<&str> = events.iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert_eq!(types[..3], ["message_start", "content_block_start", "ping"]);
    assert_eq!(types[types.len() - 2..], ["message_delta", "message_stop"]);
    assert_eq!(events[types.len() - 2]["delta"]["stop_reason"], "tool_use");
    // Rebuild the content blocks from the deltas.
    let mut blocks: Vec<Value> = Vec::new();
    for e in &events {
      match e["type"].as_str().unwrap() {
        "content_block_start" => blocks.push(e["content_block"].clone()),
        "content_block_delta" => {
          let (b, d) = (blocks.last_mut().unwrap(), &e["delta"]);
          for (kind, field) in [
            ("thinking_delta", "thinking"),
            ("signature_delta", "signature"),
            ("text_delta", "text"),
            ("input_json_delta", "partial_json"),
          ] {
            if d["type"] == kind {
              let joined = format!(
                "{}{}",
                b[field].as_str().unwrap_or(""),
                d[field].as_str().unwrap()
              );
              b[field] = json!(joined);
            }
          }
        }
        _ => {}
      }
    }
    assert_eq!(blocks[0]["thinking"], "Thinking.");
    assert_eq!(blocks[0]["signature"], "mock-signature-r1-1");
    assert_eq!(blocks[1]["text"], "Hi.");
    assert_eq!(blocks[2]["partial_json"], r#"{"order_id":"43"}"#);

    // The thinking sent back, intact, parses as signed reasoning.
    let mut tool_use = blocks[2].clone();
    tool_use["input"] = json!({"order_id": "43"});
    tool_use.as_object_mut().unwrap().remove("partial_json");
    let continuation = |thinking: Value| {
      json!({"model": "m", "stream": true, "messages": [ask,
        {"role": "assistant", "content": [thinking, blocks[1].clone(), tool_use.clone()]},
        {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "c2", "content": "ok"}]}]})
    };
    let client = spawn_post(url.clone(), continuation(blocks[0].clone()));
    model(run.next().await)
      .expect_history(&[
        user("hi"),
        user("again"),
        thought("Thinking.", true),
        assistant("Hi."),
        tool_call("c2", "lookup_order", json!({"order_id": "43"})),
        tool_result("c2", "ok"),
      ])
      .unwrap()
      .reply(text("Done."))
      .await
      .unwrap();
    client.await.unwrap();

    // A changed signature is rejected, in Anthropic's error shape.
    let mut tampered = blocks[0].clone();
    tampered["signature"] = json!("mock-signature-r1-X");
    let resp = reqwest::Client::new()
      .post(url)
      .json(&continuation(tampered))
      .send()
      .await
      .unwrap();
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert!(run.next().await.unwrap().is_err());
  }

  #[tokio::test]
  async fn anthropic_faults_cut_the_stream_inside_the_last_block() {
    let server = MockServer::start().await.unwrap();
    let mut run = server.register("r1", Endpoint::Anthropic, &[]);
    let url = format!("{}/v1/messages", run.base_url());
    let body = json!({"model": "m", "stream": true,
      "messages": [{"role": "user", "content": "hi"}]});
    for (fault, last) in [
      (truncated(text("The capital")), "content_block_delta"),
      (error_chunk(text("The capital")), "error"),
    ] {
      let client = spawn_post(url.clone(), body.clone());
      model(run.next().await).fail(fault).await.unwrap();
      let events: Vec<Value> = data_lines(&client.await.unwrap())
        .iter()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
      let types: Vec<&str> = events.iter().map(|e| e["type"].as_str().unwrap()).collect();
      assert_eq!(*types.last().unwrap(), last);
      assert!(!types.contains(&"content_block_stop"));
      assert!(!types.contains(&"message_stop"));
    }
  }
}
