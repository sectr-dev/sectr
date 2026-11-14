//! [`GatewayClient`] — the CLI's HTTP client of a **sectr gateway**
//! (local `sectr dev` today; a cloud gateway later). Deliberately dumb:
//! a base URL + reqwest and the protocol methods — no retries beyond a
//! busy app's `429`, no token
//! lifecycle, no endpoint catalog (that's what would make it an SDK
//! `Client`; this stays a thin wrapper).
//!
//! Environments differ ONLY in construction; methods are protocol and
//! carry no environment logic (no `is_cloud`, no conditional auth):
//! - [`GatewayClient::discover_local`] — reads `.agent/dev.json`, no
//!   auth; fails fast with "run `sectr dev` first".
//! - (future) `for_cloud` — base + auth headers from `sectr login`
//!   state; the SAME method set against the public sessions api.
//!
//! Known rough edge to restructure when `--cloud` lands: `sectr send`
//! resolves app + route via [`GatewayClient::dev_status`], which exists
//! only in `dev-api.yaml` (the public contract has no such endpoint).
//! A cloud send will resolve them differently (a per-app route catalog
//! from the control plane), so `dev_status` is dev-named on purpose and
//! sits outside the protocol-core method set.
//!
//! The SSE machinery ([`SseParser`], `print_event`, [`Step`]) is pure
//! rendering, free of client state.

use std::io::Write as _;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, bail};
use futures::StreamExt;
use sectr_common::api::{
  ApprovalDecisionRequest, DevStatus, ErrorBody, NewSessionBody, SessionCreated, SessionInputBody,
};
use sectr_common::events::{Event, EventPayload};

pub(crate) struct GatewayClient {
  base: String,
  http: reqwest::Client,
  /// Bearer token for CLOUD mode (`for_cloud`); None on the dev
  /// gateway (which has no auth). Attached to every request.
  auth: Option<String>,
}

impl GatewayClient {
  /// Discover the LOCAL dev gateway from the project's
  /// `.agent/dev.json` (what `sectr dev` leaves behind). Fails fast —
  /// never starts anything. Reads the `gateway_url` the running gateway
  /// records (`https://local.sectr.dev:<port>` when TLS is on, else
  /// `http://127.0.0.1:<port>`); it's cleared on shutdown.
  pub(crate) fn discover_local(project: &Path) -> anyhow::Result<Self> {
    let state_path = project.join(".agent/dev.json");
    let raw = std::fs::read_to_string(&state_path).with_context(|| {
      format!(
        "no dev state at {} — run `sectr dev` first",
        state_path.display()
      )
    })?;
    let state = serde_json::from_str::<serde_json::Value>(&raw)?;
    let base = state
      .get("gateway_url")
      .and_then(|v| v.as_str())
      .map(str::to_string)
      .context("no gateway recorded in .agent/dev.json — run `sectr dev` first")?;
    Ok(GatewayClient {
      base,
      http: reqwest::Client::new(),
      auth: None,
    })
  }

  /// CLOUD mode: the same protocol method set against a
  /// sectr-server, with the platform key (or an invocation key) on
  /// every request. Construction is the ONLY difference from dev —
  /// the methods carry no environment logic.
  pub(crate) fn for_cloud(base: &str, key: &str) -> Self {
    GatewayClient {
      base: base.trim_end_matches('/').to_owned(),
      http: reqwest::Client::new(),
      auth: Some(key.to_owned()),
    }
  }

  /// A request builder with the auth header attached (cloud) or not (dev).
  fn request(&self, method: reqwest::Method, url: &str) -> reqwest::RequestBuilder {
    let req = self.http.request(method, url);
    match &self.auth {
      Some(key) => req.bearer_auth(key),
      None => req,
    }
  }

  /// CLOUD app/route resolution: `GET /apps` matched by slug or
  /// name (the prod contract's app detail is id-addressed; the slug is
  /// what a human types). Returns (id, first-routes).
  pub(crate) async fn cloud_find_app(&self, slug: &str) -> anyhow::Result<(String, Vec<String>)> {
    let resp = self
      .request(reqwest::Method::GET, &format!("{}/apps", self.base))
      .send()
      .await
      .map_err(|e| anyhow::anyhow!("server unreachable: {e}"))?;
    if !resp.status().is_success() {
      bail!("the server returned {} for GET /apps", resp.status());
    }
    let apps: serde_json::Value = resp.json().await?;
    for app in apps.as_array().context("GET /apps returned a non-array")? {
      let matches = app.get("slug").and_then(|v| v.as_str()) == Some(slug)
        || app.get("name").and_then(|v| v.as_str()) == Some(slug);
      if matches {
        let id = app
          .get("id")
          .and_then(|v| v.as_str())
          .context("app entry has no id")?
          .to_owned();
        let routes = app
          .get("routes")
          .and_then(|r| r.as_array())
          .map(|list| {
            list
              .iter()
              .filter_map(|r| r.get("path").and_then(|p| p.as_str()))
              .map(str::to_owned)
              .collect()
          })
          .unwrap_or_default();
        return Ok((id, routes));
      }
    }
    bail!("no app matching '{slug}' on this server — `sectr deploy` first")
  }

  /// `GET /dev/status` — liveness probe + dev-only facts (app name,
  /// manifest routes). DEV-ONLY: see the module docs for how a cloud
  /// client replaces this role.
  pub(crate) async fn dev_status(&self) -> anyhow::Result<DevStatus> {
    let resp = self
      .http
      .get(format!("{}/dev/status", self.base))
      .send()
      .await
      .map_err(|e| {
        anyhow::anyhow!(
          "sectr dev isn't running (gateway {}: {e}) — run `sectr dev` first",
          self.base
        )
      })?;
    if !resp.status().is_success() {
      bail!("gateway returned {} for /dev/status", resp.status());
    }
    Ok(resp.json().await?)
  }

  /// `POST /sessions` — create a session and start its first turn.
  pub(crate) async fn create_session(
    &self,
    app: &str,
    route: &str,
    input: &str,
  ) -> anyhow::Result<uuid::Uuid> {
    let created = self
      .post(
        "/sessions",
        &NewSessionBody {
          app: app.to_owned(),
          route: route.to_owned(),
          input: input.into(),
        },
      )
      .await?;
    let created: SessionCreated =
      serde_json::from_value(created).context("gateway returned no session_id")?;
    Ok(created.session_id)
  }

  /// `POST /sessions/{id}/input` — a follow-up turn.
  pub(crate) async fn send_input(&self, sid: uuid::Uuid, input: &str) -> anyhow::Result<()> {
    self
      .post(
        &format!("/sessions/{sid}/input"),
        &SessionInputBody {
          input: input.into(),
        },
      )
      .await?;
    Ok(())
  }

  /// `POST /sessions/{id}/approval` — decide the pending approval;
  /// 202 once journaled (the resumed turn streams via `stream_session`).
  pub(crate) async fn decide(&self, sid: uuid::Uuid, approved: bool) -> anyhow::Result<()> {
    self
      .post(
        &format!("/sessions/{sid}/approval"),
        &ApprovalDecisionRequest {
          approved,
          approver: Some("sectr send".into()),
        },
      )
      .await?;
    Ok(())
  }

  /// Current `event_count` of a session — the SSE baseline (only newer
  /// events print). Also validates the session exists (404 → readable
  /// error).
  pub(crate) async fn session_event_count(&self, sid: uuid::Uuid) -> anyhow::Result<i64> {
    let resp = self
      .request(
        reqwest::Method::GET,
        &format!("{}/sessions/{sid}", self.base),
      )
      .send()
      .await
      .map_err(|e| anyhow::anyhow!("gateway unreachable: {e}"))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
      bail!("no such session {sid}");
    }
    if !resp.status().is_success() {
      bail!("gateway returned {} for the session lookup", resp.status());
    }
    let summary: serde_json::Value = resp.json().await?;
    Ok(summary["event_count"].as_i64().unwrap_or(0))
  }

  /// Follow a session's journal tail. Prints events (human renderer, or
  /// raw envelopes with `json`), skipping everything below `baseline`.
  /// Resumes across connection loss via `Last-Event-ID`: a few
  /// reconnects in `Run` mode, forever in `Tail` mode.
  pub(crate) async fn stream_session(
    &self,
    sid: uuid::Uuid,
    baseline: i64,
    mode: StreamMode,
    json: bool,
  ) -> anyhow::Result<()> {
    let url = format!("{}/sessions/{sid}/events", self.base);
    let mut renderer = Renderer::default();
    let mut last_id: Option<String> = None;
    let mut reconnects = 0u32;
    loop {
      let mut req = self.request(reqwest::Method::GET, &url);
      if let Some(id) = &last_id {
        req = req.header("Last-Event-ID", id);
      }
      let resp = req
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("gateway unreachable: {e} — is `sectr dev` still running?"))?;
      if !resp.status().is_success() {
        bail!("gateway returned {} for the event stream", resp.status());
      }
      let mut bytes = resp.bytes_stream();
      let mut sse = SseParser::default();
      let mut line_buf = String::new();
      let mut got_frame = false;

      while let Some(chunk) = bytes.next().await {
        line_buf.push_str(&String::from_utf8_lossy(&chunk?));
        while let Some(pos) = line_buf.find('\n') {
          let line: String = line_buf.drain(..=pos).collect();
          let line = line.trim_end_matches(['\n', '\r']);
          if let Some((id, data)) = sse.feed(line) {
            last_id = id;
            got_frame = true;
            match print_event(&data, baseline, json, &mut renderer)? {
              Step::Continue => {}
              Step::TurnEnded => {
                if mode == StreamMode::Run {
                  return Ok(());
                }
              }
              Step::Suspended { tool } => {
                if mode == StreamMode::Run {
                  if !json {
                    eprintln!(
                      "⏸  awaiting approval: {tool} — decide with \
                       `sectr send --approve|--deny --session {sid}`"
                    );
                  }
                  return Ok(());
                } else if !json {
                  eprintln!("⏸  approval required: {tool} (still following)");
                }
              }
            }
          }
        }
      }

      // The connection closed without the stream being done.
      if got_frame {
        reconnects = 0;
      }
      reconnects += 1;
      if mode == StreamMode::Run && reconnects > 5 {
        bail!("event stream ended before the turn completed");
      }
      tokio::time::sleep(Duration::from_secs(if mode == StreamMode::Tail {
        1
      } else {
        0
      }))
      .await;
      // Loop reconnects with `last_id` (Last-Event-ID resumption).
    }
  }

  /// POST a JSON body to a gateway path; maps error statuses to a
  /// readable message. A `429` (the app is at its concurrent-turn limit)
  /// is retried after its `Retry-After`, saying so on stderr: the server
  /// leaves nothing behind when it refuses, so a retry is safe.
  async fn post(
    &self,
    path: &str,
    body: &impl serde::Serialize,
  ) -> anyhow::Result<serde_json::Value> {
    let mut retries = 0;
    let (status, text) = loop {
      let resp = self
        .request(reqwest::Method::POST, &format!("{}{path}", self.base))
        .json(body)
        .send()
        .await?;
      let status = resp.status();
      let retry_after = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
      let text = resp.text().await.unwrap_or_default();
      let wait = Duration::from_secs(retry_after.unwrap_or(5).max(1));
      if status == reqwest::StatusCode::TOO_MANY_REQUESTS
        && retries < BUSY_RETRIES
        && wait <= MAX_BUSY_WAIT
      {
        retries += 1;
        eprintln!(
          "{}; retrying in {}s ({retries}/{BUSY_RETRIES})",
          server_error(&text).unwrap_or_else(|| "the server is busy".into()),
          wait.as_secs()
        );
        tokio::time::sleep(wait).await;
        continue;
      }
      break (status, text);
    };
    if !status.is_success() {
      bail!("{status}: {text}");
    }
    // 202s (input/approval) have empty bodies; 201 has {session_id}.
    if text.trim().is_empty() {
      return Ok(serde_json::Value::Null);
    }
    serde_json::from_str(&text)
      .with_context(|| format!("unexpected gateway response from {path}: {text}"))
  }
}

/// How many times a `429` is retried before giving up.
const BUSY_RETRIES: u32 = 6;

/// A `429` that asks for a longer wait than this (a rate limit, rather
/// than a turn finishing) isn't retried.
const MAX_BUSY_WAIT: Duration = Duration::from_secs(30);

/// The `error` of a server's JSON error body.
fn server_error(text: &str) -> Option<String> {
  serde_json::from_str::<ErrorBody>(text)
    .ok()
    .map(|body| body.error)
}

/// What the streaming loop does when the journal shows a turn boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamMode {
  /// `sectr send`: stop when the invocation is over (turn end or
  /// approval pause); bounded reconnects.
  Run,
  /// `sectr tail`: never stop (until ctrl-c); reconnect forever.
  Tail,
}

/// One journal event's effect on the stream.
pub(crate) enum Step {
  Continue,
  TurnEnded,
  /// TOOL_APPROVAL_REQUIRED — the turn suspends for a human decision.
  Suspended {
    tool: String,
  },
}

#[derive(Default)]
pub(crate) struct SseParser {
  data: String,
  id: Option<String>,
}

impl SseParser {
  /// Feed one complete line; on the frame-terminating blank line returns
  /// `(last id, data payload)`.
  pub(crate) fn feed(&mut self, line: &str) -> Option<(Option<String>, String)> {
    if let Some(rest) = line.strip_prefix("data:") {
      let rest = rest.strip_prefix(' ').unwrap_or(rest);
      self.data.push_str(rest);
      self.data.push('\n');
      return None;
    }
    if let Some(rest) = line.strip_prefix("id:") {
      self.id = Some(rest.trim().to_string());
      return None;
    }
    // "event:", "retry:", comments (":...") — irrelevant for us.
    if line.is_empty() && !self.data.is_empty() {
      let data = std::mem::take(&mut self.data);
      return Some((self.id.take(), data.trim_end().to_string()));
    }
    None
  }
}

/// Parse one SSE `data:` payload as a journal event envelope and render
/// it. Events below `baseline` (already-seen) are skipped silently; with
/// `json`, the raw envelope prints instead of the human renderer.
fn print_event(
  data: &str,
  baseline: i64,
  json: bool,
  renderer: &mut Renderer,
) -> anyhow::Result<Step> {
  let event: Event =
    serde_json::from_str(data).with_context(|| format!("unparseable event frame: {data}"))?;
  if event.seq < baseline {
    return Ok(Step::Continue);
  }
  if json {
    println!("{data}");
  } else {
    print!("{}", renderer.render(&event));
    std::io::stdout().flush().ok();
  }
  Ok(render_step(&event))
}

/// The human renderer. It keeps track of the cursor, so every line other
/// than message text starts on a line of its own, and turns are separated
/// by exactly one blank line.
#[derive(Default)]
pub(crate) struct Renderer {
  /// Anything printed yet (no blank line before the first turn).
  printed: bool,
  /// The cursor is mid-line (message text without its newline yet).
  mid_line: bool,
}

impl Renderer {
  /// What to print for `event`, possibly nothing.
  pub(crate) fn render(&mut self, event: &Event) -> String {
    use sectr_common::events::{InvocationReason, TypedDelta};
    let mut out = String::new();
    match &event.payload {
      EventPayload::MessageDelta(d) => match &d.delta {
        TypedDelta::Text { text, .. } if !text.is_empty() => {
          out.push_str(text);
          self.printed = true;
          self.mid_line = !text.ends_with('\n');
        }
        // Reasoning deltas stay out of the way for now.
        _ => {}
      },
      // A message without text (only tool calls) prints nothing.
      EventPayload::MessageEnd(_) => self.end_line(&mut out),
      EventPayload::MessageStart(_) | EventPayload::SessionCreated(_) => {}
      EventPayload::InvocationStarted(s) => match &s.reason {
        InvocationReason::UserMessage { input, .. } => {
          self.end_line(&mut out);
          if self.printed {
            out.push('\n');
          }
          let text = input.as_str().trim().replace('\n', "\n  ");
          self.line(&mut out, &format!("› {text}"));
        }
        InvocationReason::ApprovalDecision { decision, .. } => {
          let what = if decision.approved {
            "approved"
          } else {
            "denied"
          };
          let line = match &decision.approver {
            Some(by) => format!("· resumed: {what} by {by}"),
            None => format!("· resumed: {what}"),
          };
          self.line(&mut out, &line);
        }
        InvocationReason::ToolResult { .. } => {
          self.line(&mut out, "· resumed with a tool result");
        }
      },
      EventPayload::ToolCall(c) => {
        self.line(&mut out, &format!("→ {} {}", c.name, trunc(&c.arguments)));
      }
      EventPayload::ToolResult(r) => self.line(&mut out, &format!("← {}", result_text(&r.result))),
      // The stream loop says it's paused, on stderr.
      EventPayload::ToolApprovalRequired(_) => self.end_line(&mut out),
      EventPayload::ToolApprovalGranted(_) => self.line(&mut out, "✓ approved"),
      EventPayload::ToolApprovalDenied(_) => self.line(&mut out, "✗ denied"),
      EventPayload::Error(e) => self.line(&mut out, &format!("✗ runner error: {}", e.message)),
      EventPayload::SessionError(e) => {
        self.line(&mut out, &format!("✗ session error: {}", e.message));
      }
      EventPayload::NoMoreActions(_) => self.line(&mut out, "✓ turn complete"),
      EventPayload::TranscriptCompacted(_) => self.line(&mut out, "[context compacted]"),
    }
    out
  }

  /// Finish a line of message text, if one is open.
  fn end_line(&mut self, out: &mut String) {
    if self.mid_line {
      out.push('\n');
      self.mid_line = false;
    }
  }

  /// A line of its own.
  fn line(&mut self, out: &mut String, text: &str) {
    self.end_line(out);
    out.push_str(text);
    out.push('\n');
    self.printed = true;
  }
}

/// The turn-boundary classification, independent of rendering.
fn render_step(event: &Event) -> Step {
  match &event.payload {
    EventPayload::ToolApprovalRequired(a) => Step::Suspended {
      tool: a.name.clone(),
    },
    EventPayload::Error(_) | EventPayload::SessionError(_) | EventPayload::NoMoreActions(_) => {
      Step::TurnEnded
    }
    _ => Step::Continue,
  }
}

/// A tool result as one line. A string prints as its text, not as a JSON
/// string: adapters journal results as the text the model saw (a dict
/// tool's result is its JSON text), so quoting it would escape every `"`.
/// Its newlines print as `\n` to keep it on one line.
fn result_text(v: &serde_json::Value) -> String {
  match v {
    serde_json::Value::String(s) => trunc_str(&s.replace('\n', "\\n")),
    other => trunc(other),
  }
}

fn trunc(v: &serde_json::Value) -> String {
  trunc_str(&v.to_string())
}

fn trunc_str(s: &str) -> String {
  let chars: Vec<char> = s.chars().collect();
  if chars.len() > 120 {
    let head: String = chars.iter().take(120).collect();
    format!("{head}…")
  } else {
    s.to_owned()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use sectr_common::events::{
    Event, EventPayload, MessageDelta, NoMoreActions, ToolApprovalRequired, TypedDelta,
  };

  /// A wire-valid envelope string (serialized from the real types, so
  /// the tests track the schema).
  fn envelope(seq: i64, payload: EventPayload) -> String {
    serde_json::to_string(&Event::new(
      uuid::Uuid::now_v7(),
      uuid::Uuid::now_v7(),
      uuid::Uuid::now_v7(),
      seq,
      chrono::Utc::now(),
      payload,
    ))
    .unwrap()
  }

  fn delta(text: &str) -> EventPayload {
    EventPayload::MessageDelta(MessageDelta {
      message_id: "m1".into(),
      delta: TypedDelta::text(text),
      extra: Default::default(),
    })
  }

  fn approval() -> EventPayload {
    EventPayload::ToolApprovalRequired(ToolApprovalRequired {
      tool_call_id: "c1".into(),
      name: "refund".into(),
      arguments: serde_json::json!({}),
      reason: None,
      extra: Default::default(),
    })
  }

  #[test]
  fn baseline_skips_old_events_in_both_modes() {
    let data = envelope(3, delta("old"));
    assert!(matches!(
      print_event(&data, 4, false, &mut Renderer::default()).unwrap(),
      Step::Continue
    ));
    assert!(matches!(
      print_event(&data, 4, true, &mut Renderer::default()).unwrap(),
      Step::Continue
    ));
  }

  /// Json mode classifies turn boundaries the same as the human
  /// renderer — the stream's stop/continue behavior must not depend on
  /// the output format.
  #[test]
  fn json_classification_matches_human() {
    let cases = [
      (
        approval(),
        Step::Suspended {
          tool: "refund".into(),
        },
      ),
      (
        EventPayload::NoMoreActions(NoMoreActions::default()),
        Step::TurnEnded,
      ),
      (delta("hi"), Step::Continue),
    ];
    for (payload, expected) in cases {
      let data = envelope(0, payload);
      let human = print_event(&data, 0, false, &mut Renderer::default()).unwrap();
      let json = print_event(&data, 0, true, &mut Renderer::default()).unwrap();
      assert_eq!(step_name(&human), step_name(&expected));
      assert_eq!(step_name(&json), step_name(&expected));
      assert_eq!(
        step_name(&human),
        step_name(&json),
        "json mode diverged from human mode"
      );
    }
  }

  /// Two turns, the second paused for an approval and resumed: the
  /// layout a person reads.
  #[test]
  fn the_human_renderer_lays_turns_out_cleanly() {
    use sectr_common::events::{
      ApprovalDecision, InvocationReason, InvocationStarted, MessageEnd, MessageStart,
      ToolApprovalDecision, ToolCall, ToolResult,
    };
    let user = |text: &str| {
      EventPayload::InvocationStarted(InvocationStarted {
        invocation_id: uuid::Uuid::now_v7(),
        route: "/chat".into(),
        deployment_id: uuid::Uuid::nil(),
        reason: InvocationReason::UserMessage {
          input: text.to_string().try_into().unwrap(),
          extra: Default::default(),
        },
        extra: Default::default(),
      })
    };
    let start = || {
      EventPayload::MessageStart(MessageStart {
        role: "assistant".into(),
        message_id: "m1".into(),
        extra: Default::default(),
      })
    };
    let end = || {
      EventPayload::MessageEnd(MessageEnd {
        message_id: "m1".into(),
        finish_reason: None,
        framework_state: None,
        extra: Default::default(),
      })
    };
    let call = |name: &str, args: serde_json::Value| {
      EventPayload::ToolCall(ToolCall {
        tool_call_id: "c1".into(),
        name: name.into(),
        arguments: args,
        extra: Default::default(),
      })
    };
    let result = |value: serde_json::Value| {
      EventPayload::ToolResult(ToolResult {
        tool_call_id: "c1".into(),
        result: value,
        extra: Default::default(),
      })
    };
    let done = || EventPayload::NoMoreActions(NoMoreActions::default());
    let journal = vec![
      user("where is order 42?"),
      start(),
      delta("Let me check "),
      delta("that order."),
      end(),
      start(),
      end(),
      call("lookup_order", serde_json::json!({"order_id": "42"})),
      result(serde_json::json!({"status": "shipped"})),
      start(),
      delta("It shipped."),
      end(),
      done(),
      user("please refund it"),
      // A response with only a tool call: no text, no blank lines.
      start(),
      end(),
      call("refund", serde_json::json!({"order_id": "42"})),
      approval(),
      EventPayload::ToolApprovalGranted(ToolApprovalDecision {
        tool_call_id: "c1".into(),
        approver: Some("sectr send".into()),
        extra: Default::default(),
      }),
      EventPayload::InvocationStarted(InvocationStarted {
        invocation_id: uuid::Uuid::now_v7(),
        route: "/chat".into(),
        deployment_id: uuid::Uuid::nil(),
        reason: InvocationReason::ApprovalDecision {
          decision: ApprovalDecision {
            approved: true,
            approver: Some("sectr send".into()),
            extra: Default::default(),
          },
          extra: Default::default(),
        },
        extra: Default::default(),
      }),
      result(serde_json::json!("refunded")),
      start(),
      // Text without a trailing newline, then the turn's end.
      delta("Refunded."),
      done(),
    ];
    let mut renderer = Renderer::default();
    let out: String = journal
      .into_iter()
      .enumerate()
      .map(|(seq, payload)| {
        let event: Event = serde_json::from_str(&envelope(seq as i64, payload)).unwrap();
        renderer.render(&event)
      })
      .collect();
    assert_eq!(
      out,
      "› where is order 42?
Let me check that order.
→ lookup_order {\"order_id\":\"42\"}
← {\"status\":\"shipped\"}
It shipped.
✓ turn complete

› please refund it
→ refund {\"order_id\":\"42\"}
✓ approved
· resumed: approved by sectr send
← refunded
Refunded.
✓ turn complete
"
    );
  }

  #[test]
  fn a_string_result_prints_as_its_text() {
    // What the Pydantic AI adapter journals for a dict-returning tool.
    let journaled = serde_json::Value::String(r#"{"order_id":"42","status":"shipped"}"#.into());
    assert_eq!(
      result_text(&journaled),
      r#"{"order_id":"42","status":"shipped"}"#
    );
    assert_eq!(
      result_text(&"line one\nline two".into()),
      r"line one\nline two"
    );
    assert_eq!(
      result_text(&"x".repeat(121).into()),
      format!("{}…", "x".repeat(120))
    );
    // A non-string result still prints as JSON.
    assert_eq!(result_text(&serde_json::Value::Null), "null");
  }

  /// A stub `POST /sessions` answering each request from `answers` in
  /// turn: (status, Retry-After, body).
  async fn sessions_stub(
    answers: Vec<(u16, Option<&'static str>, serde_json::Value)>,
  ) -> (
    GatewayClient,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
  ) {
    use axum::response::IntoResponse as _;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let calls = std::sync::Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let app = axum::Router::new().route(
      "/sessions",
      axum::routing::post(move || {
        let n = seen.fetch_add(1, Ordering::SeqCst);
        let (status, retry_after, body) = answers[n.min(answers.len() - 1)].clone();
        async move {
          let mut response = (
            axum::http::StatusCode::from_u16(status).unwrap(),
            axum::Json(body),
          )
            .into_response();
          if let Some(after) = retry_after {
            response
              .headers_mut()
              .insert("retry-after", after.parse().unwrap());
          }
          response
        }
      }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (
      GatewayClient::for_cloud(&format!("http://{addr}"), "skp_x"),
      calls,
    )
  }

  fn busy() -> serde_json::Value {
    serde_json::json!({
      "code": "concurrency_limit",
      "error": "this app is running its maximum of 3 turns at once",
    })
  }

  #[tokio::test]
  async fn a_busy_app_is_retried_after_its_retry_after() {
    let sid = uuid::Uuid::now_v7();
    let (client, calls) = sessions_stub(vec![
      (429, Some("1"), busy()),
      (201, None, serde_json::json!({"session_id": sid})),
    ])
    .await;
    let got = client.create_session("a", "/chat", "hi").await.unwrap();
    assert_eq!(got, sid);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
  }

  #[tokio::test]
  async fn a_long_retry_after_is_not_waited_for() {
    let (client, calls) = sessions_stub(vec![(429, Some("3600"), busy())]).await;
    let err = client
      .create_session("a", "/chat", "hi")
      .await
      .unwrap_err()
      .to_string();
    assert!(err.starts_with("429"), "{err}");
    assert!(err.contains("maximum of 3 turns"), "{err}");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
  }

  #[test]
  fn the_servers_error_text_is_read_from_its_body() {
    assert_eq!(
      server_error(&busy().to_string()).as_deref(),
      Some("this app is running its maximum of 3 turns at once")
    );
    assert_eq!(server_error("not json"), None);
  }

  fn step_name(s: &Step) -> &'static str {
    match s {
      Step::Continue => "continue",
      Step::TurnEnded => "ended",
      Step::Suspended { .. } => "suspended",
    }
  }

  fn project_with_state(state: serde_json::Value) -> std::path::PathBuf {
    let project = std::env::temp_dir().join(format!("sectr-discover-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(project.join(".agent")).unwrap();
    std::fs::write(project.join(".agent/dev.json"), state.to_string()).unwrap();
    project
  }

  #[test]
  fn discover_local_reads_the_gateway_url() {
    let project =
      project_with_state(serde_json::json!({ "gateway_url": "https://local.sectr.dev:8420" }));
    let client = GatewayClient::discover_local(&project).unwrap();
    assert_eq!(client.base, "https://local.sectr.dev:8420");
  }

  /// `gateway_port` is gone: a file with only that has no gateway.
  #[test]
  fn discover_local_ignores_the_old_gateway_port() {
    let project = project_with_state(serde_json::json!({ "gateway_port": 52817 }));
    let err = GatewayClient::discover_local(&project)
      .err()
      .unwrap()
      .to_string();
    assert!(err.contains("no gateway recorded"), "{err}");
  }
}
