//! Golden tests for the journal → AG-UI projection: every
//! `schema/fixtures/valid/agui/*.json` is (journal, request) → expected
//! AG-UI stream — a run's, or (`"kind": "connect"`) a connect's.
//! `BLESS_AGUI_FIXTURES=1 cargo test -p sectr-common agui`
//! rewrites the `expected` arrays from the current implementation — review
//! the diff like any golden change (the Python/Node conformance suites then
//! validate the blessed streams against the upstream AG-UI schema/verifier).

use std::path::PathBuf;

use serde_json::{Value, json};
use uuid::Uuid;

use super::*;
use crate::events::{
  Error, Event, EventPayload, Extra, InvocationStarted, MessageDelta, MessageEnd, MessageStart,
  NoMoreActions, SessionError, ToolApprovalRequired, ToolCall, ToolResult, TypedDelta,
};

/// The fold as wire JSON, for indexing by field.
fn fold(events: &[Event]) -> Vec<Value> {
  fold_messages(events)
    .iter()
    .map(|m| serde_json::to_value(m).unwrap())
    .collect()
}

fn fixtures_dir() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../schema/fixtures/valid/agui")
}

struct Fixture {
  path: PathBuf,
  doc: Value,
}

impl Fixture {
  fn load_all() -> Vec<Fixture> {
    let mut out: Vec<Fixture> = std::fs::read_dir(fixtures_dir())
      .expect("schema/fixtures/valid/agui exists")
      .map(|e| e.unwrap().path())
      .filter(|p| p.extension().is_some_and(|x| x == "json"))
      .map(|path| {
        let doc = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        Fixture { path, doc }
      })
      .collect();
    out.sort_by(|a, b| a.path.cmp(&b.path));
    assert!(!out.is_empty(), "no agui fixtures found");
    out
  }

  fn name(&self) -> String {
    self.path.file_stem().unwrap().to_string_lossy().into()
  }

  /// A connect fixture: `request` is `{threadId}` and the stream
  /// re-streams the journal's latest invocation.
  fn is_connect(&self) -> bool {
    self.doc["kind"] == "connect"
  }

  fn journal(&self) -> Vec<Event> {
    self.doc["journal"]
      .as_array()
      .unwrap()
      .iter()
      .map(|v| Event::from_json_value(v.clone()).expect("journal envelope parses"))
      .collect()
  }

  /// Run the projection exactly as the endpoint does.
  fn project(&self) -> (Vec<Value>, bool) {
    let events = self.journal();
    let thread_id = self.doc["request"]["threadId"].as_str().unwrap();
    if self.is_connect() {
      let (idx, ctx) = connect_context(thread_id, &events).expect("connect fixture has a run");
      return Self::feed(Projector::new(ctx), &events[idx..]);
    }
    let invocation_id: Uuid = self.doc["invocation_id"].as_str().unwrap().parse().unwrap();
    let idx = events
      .iter()
      .position(|e| {
        matches!(&e.payload, EventPayload::InvocationStarted(i) if i.invocation_id == invocation_id)
      })
      .expect("fixture invocation is in the journal");
    let created = self.doc["created"].as_bool().unwrap();
    let snapshot = (!created).then(|| fold_messages(&events[..=idx]));
    let projector = Projector::new(RunContext {
      thread_id: thread_id.into(),
      run_id: self.doc["request"]["runId"].as_str().unwrap().into(),
      input: Some(self.doc["request"].clone()),
      snapshot,
    });
    Self::feed(projector, &events[idx..])
  }

  fn feed(mut projector: Projector, events: &[Event]) -> (Vec<Value>, bool) {
    let mut out = Vec::new();
    for event in events {
      for e in projector.push(event) {
        out.push(serde_json::to_value(&e).unwrap());
      }
    }
    (out, projector.is_done())
  }
}

#[test]
fn golden_streams_match_the_journal_projection() {
  let bless = std::env::var("BLESS_AGUI_FIXTURES").is_ok();
  for fixture in Fixture::load_all() {
    let (stream, done) = fixture.project();
    assert!(
      done,
      "{}: every golden run must reach its terminal event",
      fixture.name()
    );
    // What a client holds after applying the run (the thread's messages):
    // the fold of the WHOLE journal. The Node suite checks the client
    // reducer's state against it (snapshot and stream must agree).
    let final_messages = serde_json::to_value(fold_messages(&fixture.journal())).unwrap();
    if bless {
      let mut doc = fixture.doc.clone();
      doc["expected"] = Value::Array(stream);
      doc["final_messages"] = final_messages;
      std::fs::write(
        &fixture.path,
        serde_json::to_string_pretty(&doc).unwrap() + "\n",
      )
      .unwrap();
      continue;
    }
    assert_eq!(
      Value::Array(stream),
      fixture.doc["expected"],
      "{}: projection differs from the golden stream (BLESS_AGUI_FIXTURES=1 to regenerate)",
      fixture.name()
    );
    assert_eq!(
      final_messages,
      fixture.doc["final_messages"],
      "{}: journal fold differs from `final_messages` (BLESS_AGUI_FIXTURES=1 to regenerate)",
      fixture.name()
    );
  }
}

#[test]
fn every_golden_request_deserializes_as_run_agent_input() {
  for fixture in Fixture::load_all().into_iter().filter(|f| !f.is_connect()) {
    let input: RunAgentInput = serde_json::from_value(fixture.doc["request"].clone())
      .unwrap_or_else(|e| panic!("{}: {e}", fixture.name()));
    // The echo is the request, field for field (no declared field is lost).
    assert_eq!(
      serde_json::to_value(&input).unwrap(),
      fixture.doc["request"],
      "{}",
      fixture.name()
    );
  }
}

/// Stream discipline (what the upstream client verifier enforces): every
/// bracket a run opens is closed before RUN_FINISHED / RUN_ERROR, and the
/// first and only terminal event is last.
#[test]
fn golden_streams_obey_ag_ui_stream_discipline() {
  for fixture in Fixture::load_all() {
    let (stream, done) = fixture.project();
    assert_eq!(stream[0]["type"], "RUN_STARTED", "{}", fixture.name());
    let mut text: Vec<String> = Vec::new();
    let mut reasoning: Vec<String> = Vec::new();
    let mut spans: Vec<String> = Vec::new();
    for (i, e) in stream.iter().enumerate() {
      let id = e["messageId"].as_str().map(str::to_owned);
      match e["type"].as_str().unwrap() {
        "TEXT_MESSAGE_START" => text.push(id.unwrap()),
        "TEXT_MESSAGE_CONTENT" => assert!(text.contains(&id.unwrap())),
        "TEXT_MESSAGE_END" => text.retain(|t| *t != id.clone().unwrap()),
        "REASONING_START" => spans.push(id.unwrap()),
        "REASONING_MESSAGE_START" => reasoning.push(id.unwrap()),
        "REASONING_MESSAGE_CONTENT" => assert!(reasoning.contains(&id.unwrap())),
        "REASONING_MESSAGE_END" => reasoning.retain(|t| *t != id.clone().unwrap()),
        "REASONING_END" => spans.retain(|t| *t != id.clone().unwrap()),
        "RUN_FINISHED" | "RUN_ERROR" => {
          assert!(
            text.is_empty() && reasoning.is_empty() && spans.is_empty(),
            "{}: terminal event with open brackets",
            fixture.name()
          );
          assert_eq!(
            i,
            stream.len() - 1,
            "{}: terminal event is last",
            fixture.name()
          );
        }
        _ => {}
      }
    }
    if done {
      let last = stream.last().unwrap()["type"].as_str().unwrap();
      assert!(
        matches!(last, "RUN_FINISHED" | "RUN_ERROR"),
        "{}",
        fixture.name()
      );
    }
  }
}

#[test]
fn every_event_is_stamped_with_timestamp_and_seq() {
  for fixture in Fixture::load_all() {
    let (stream, _) = fixture.project();
    for e in stream {
      assert!(e["timestamp"].is_i64(), "{}: {e}", fixture.name());
      assert!(e["metadata"]["seq"].is_i64(), "{}: {e}", fixture.name());
    }
  }
}

// ---------------------------------------------------------------------------
// Targeted unit tests
// ---------------------------------------------------------------------------

fn ev(seq: i64, payload: EventPayload) -> Event {
  Event::new(
    Uuid::now_v7(),
    Uuid::nil(),
    Uuid::nil(),
    seq,
    chrono::DateTime::from_timestamp(1_700_000_000 + seq, 0).unwrap(),
    payload,
  )
}

fn invocation_started(seq: i64, input: &str) -> Event {
  ev(
    seq,
    EventPayload::InvocationStarted(InvocationStarted {
      invocation_id: Uuid::now_v7(),
      route: "/chat".into(),
      deployment_id: Uuid::nil(),
      reason: InvocationReason::UserMessage {
        input: input.to_string().try_into().unwrap(),
        extra: Extra::default(),
      },
      extra: Extra::default(),
    }),
  )
}

fn start(seq: i64, id: &str) -> Event {
  ev(
    seq,
    EventPayload::MessageStart(MessageStart {
      role: "assistant".into(),
      message_id: id.into(),
      extra: Extra::default(),
    }),
  )
}

fn delta(seq: i64, id: &str, d: TypedDelta) -> Event {
  ev(
    seq,
    EventPayload::MessageDelta(MessageDelta {
      message_id: id.into(),
      delta: d,
      extra: Extra::default(),
    }),
  )
}

fn end(seq: i64, id: &str) -> Event {
  ev(
    seq,
    EventPayload::MessageEnd(MessageEnd {
      message_id: id.into(),
      finish_reason: None,
      framework_state: None,
      extra: Extra::default(),
    }),
  )
}

fn projector() -> Projector {
  Projector::new(RunContext {
    thread_id: "t".into(),
    run_id: "r".into(),
    input: None,
    snapshot: None,
  })
}

fn types(events: &[AgUiEvent]) -> Vec<String> {
  events
    .iter()
    .map(|e| {
      serde_json::to_value(e).unwrap()["type"]
        .as_str()
        .unwrap()
        .to_owned()
    })
    .collect()
}

fn run(events: Vec<Event>) -> (Vec<AgUiEvent>, Projector) {
  let mut p = projector();
  let mut out = Vec::new();
  for e in &events {
    out.extend(p.push(e));
  }
  (out, p)
}

#[test]
fn reasoning_never_shares_the_assistant_message_id() {
  let (out, _) = run(vec![
    invocation_started(1, "hi"),
    start(2, "m1"),
    delta(3, "m1", TypedDelta::reasoning("hmm")),
    delta(4, "m1", TypedDelta::text("answer")),
    end(5, "m1"),
  ]);
  let wire: Vec<Value> = out
    .iter()
    .map(|e| serde_json::to_value(e).unwrap())
    .collect();
  // The reasoning bracket opens with its own id and closes BEFORE the text
  // delta (clients key messages by id: a shared id would merge reasoning
  // into the assistant's visible text).
  assert_eq!(
    types(&out),
    [
      "RUN_STARTED",
      "TEXT_MESSAGE_START",
      "REASONING_START",
      "REASONING_MESSAGE_START",
      "REASONING_MESSAGE_CONTENT",
      "REASONING_MESSAGE_END",
      "REASONING_END",
      "TEXT_MESSAGE_CONTENT",
      "TEXT_MESSAGE_END",
    ]
  );
  assert_eq!(wire[3]["messageId"], "m1:reasoning");
  assert_eq!(wire[3]["role"], "reasoning");
  assert_eq!(wire[7]["messageId"], "m1");
}

#[test]
fn tool_call_attaches_to_the_latest_assistant_message() {
  let (out, _) = run(vec![
    invocation_started(1, "hi"),
    start(2, "m1"),
    end(3, "m1"),
    ev(
      4,
      EventPayload::ToolCall(ToolCall {
        tool_call_id: "c1".into(),
        name: "t".into(),
        arguments: json!({"a": 1}),
        extra: Extra::default(),
      }),
    ),
  ]);
  let wire = serde_json::to_value(&out[3]).unwrap();
  assert_eq!(wire["type"], "TOOL_CALL_START");
  assert_eq!(wire["toolCallName"], "t");
  assert_eq!(wire["parentMessageId"], "m1");
  assert_eq!(serde_json::to_value(&out[4]).unwrap()["delta"], "{\"a\":1}");
}

#[test]
fn tool_result_carries_a_minted_message_id() {
  let (out, _) = run(vec![
    invocation_started(1, "hi"),
    ev(
      2,
      EventPayload::ToolResult(ToolResult {
        tool_call_id: "c1".into(),
        result: json!("plain"),
        extra: Extra::default(),
      }),
    ),
  ]);
  let wire = serde_json::to_value(&out[1]).unwrap();
  assert_eq!(wire["type"], "TOOL_CALL_RESULT");
  assert_eq!(wire["messageId"], "tool-result-c1");
  assert_eq!(wire["content"], "plain");
  assert_eq!(wire["role"], "tool");
}

#[test]
fn approval_required_is_an_interrupt_keyed_by_the_journal_row_id() {
  let approval = ev(
    2,
    EventPayload::ToolApprovalRequired(ToolApprovalRequired {
      tool_call_id: "c1".into(),
      name: "refund".into(),
      arguments: json!({"amount": 42}),
      reason: Some("needs a human".into()),
      extra: Extra::default(),
    }),
  );
  let id = approval.id;
  let (out, p) = run(vec![invocation_started(1, "hi"), approval]);
  assert!(p.is_done());
  let wire = serde_json::to_value(out.last().unwrap()).unwrap();
  assert_eq!(wire["type"], "RUN_FINISHED");
  assert_eq!(wire["outcome"]["type"], "interrupt");
  let interrupt = &wire["outcome"]["interrupts"][0];
  assert_eq!(interrupt["id"], id.to_string());
  assert_eq!(interrupt["reason"], "tool_approval");
  assert_eq!(interrupt["message"], "needs a human");
  assert_eq!(interrupt["toolCallId"], "c1");
  assert_eq!(interrupt["responseSchema"], json!({"type": "boolean"}));
  assert_eq!(interrupt["metadata"]["toolName"], "refund");
}

#[test]
fn terminal_events_stop_the_run_and_later_events_are_ignored() {
  let mut p = projector();
  p.push(&invocation_started(1, "hi"));
  let out = p.push(&ev(
    2,
    EventPayload::NoMoreActions(NoMoreActions::default()),
  ));
  assert_eq!(types(&out), ["RUN_FINISHED"]);
  assert!(p.is_done());
  assert!(p.push(&start(3, "late")).is_empty());
}

#[test]
fn runner_error_and_session_error_end_the_run_with_run_error() {
  let (out, p) = run(vec![
    invocation_started(1, "hi"),
    ev(
      2,
      EventPayload::Error(Error {
        message: "boom".into(),
        code: Some("E1".into()),
        transient: None,
        extra: Extra::default(),
      }),
    ),
  ]);
  assert!(p.is_done());
  let wire = serde_json::to_value(out.last().unwrap()).unwrap();
  assert_eq!(wire["type"], "RUN_ERROR");
  assert_eq!(wire["message"], "boom");
  assert_eq!(wire["code"], "E1");

  let (out, _) = run(vec![
    invocation_started(1, "hi"),
    start(2, "m"),
    ev(
      3,
      EventPayload::SessionError(SessionError {
        message: "lease expired".into(),
        extra: Extra::default(),
      }),
    ),
  ]);
  // The partial message is closed before the error.
  assert_eq!(
    types(&out),
    [
      "RUN_STARTED",
      "TEXT_MESSAGE_START",
      "TEXT_MESSAGE_END",
      "RUN_ERROR"
    ]
  );
}

#[test]
fn delta_without_start_still_yields_a_well_formed_message() {
  let (out, _) = run(vec![
    invocation_started(1, "hi"),
    delta(2, "orphan", TypedDelta::text("x")),
    end(3, "orphan"),
  ]);
  assert_eq!(
    types(&out),
    [
      "RUN_STARTED",
      "TEXT_MESSAGE_START",
      "TEXT_MESSAGE_CONTENT",
      "TEXT_MESSAGE_END"
    ]
  );
}

#[test]
fn a_newer_invocation_supersedes_the_run() {
  let (out, p) = run(vec![
    invocation_started(1, "hi"),
    start(2, "m"),
    invocation_started(3, "again"),
  ]);
  assert!(p.is_done());
  let wire = serde_json::to_value(out.last().unwrap()).unwrap();
  assert_eq!(wire["type"], "RUN_ERROR");
  assert_eq!(wire["code"], "superseded");
}

#[test]
fn abort_always_terminates_the_stream() {
  let mut p = projector();
  let out = p.abort("stream ended", "stream_ended");
  // A run that never saw its first event still opens before it errors.
  assert_eq!(types(&out), ["RUN_STARTED", "RUN_ERROR"]);

  let mut p = projector();
  p.push(&invocation_started(1, "hi"));
  p.push(&start(2, "m"));
  let out = p.abort("stream ended", "stream_ended");
  assert_eq!(types(&out), ["TEXT_MESSAGE_END", "RUN_ERROR"]);
  assert!(p.abort("again", "x").is_empty());
}

#[test]
fn events_before_invocation_started_are_not_part_of_the_run() {
  let mut p = projector();
  assert!(p.push(&start(1, "stray")).is_empty());
  assert!(!p.push(&invocation_started(2, "hi")).is_empty());
}

#[test]
fn run_started_echoes_the_request_and_declares_the_protocol_version() {
  let mut p = Projector::new(RunContext {
    thread_id: "t".into(),
    run_id: "r".into(),
    input: Some(json!({"threadId": "t", "runId": "r", "messages": []})),
    snapshot: Some(vec![Message::User {
      id: "u".into(),
      content: "hi".into(),
    }]),
  });
  let out = p.push(&invocation_started(1, "hi"));
  assert_eq!(types(&out), ["RUN_STARTED", "MESSAGES_SNAPSHOT"]);
  let wire = serde_json::to_value(&out[0]).unwrap();
  assert_eq!(wire["protocolVersion"], "1.0");
  assert_eq!(wire["input"]["runId"], "r");
}

#[test]
fn any_1_x_protocol_version_is_accepted() {
  for ok in ["1", "1.0", "1.1", "1.0.1", "1.12"] {
    assert!(supports_protocol_version(ok), "{ok}");
  }
  for bad in ["2.0", "0.9", "10.0", "", "1.", "1.x", "v1.0", " 1.0"] {
    assert!(!supports_protocol_version(bad), "{bad:?}");
  }
}

fn session_created() -> Event {
  ev(
    0,
    EventPayload::SessionCreated(crate::events::SessionCreated {
      app_id: Uuid::nil(),
      route: "/chat".into(),
      external_user_id: None,
      extra: Extra::default(),
    }),
  )
}

fn approval_resumed(seq: i64, run_id: &str) -> Event {
  ev(
    seq,
    EventPayload::InvocationStarted(InvocationStarted {
      invocation_id: Uuid::now_v7(),
      route: "/chat".into(),
      deployment_id: Uuid::nil(),
      reason: InvocationReason::ApprovalDecision {
        decision: crate::events::ApprovalDecision {
          approved: true,
          approver: None,
          extra: Extra::default(),
        },
        extra: Extra::default(),
      },
      extra: invocation_extra(run_id, None, None, None),
    }),
  )
}

#[test]
fn latest_invocation_is_the_last_invocation_started_of_any_reason() {
  // No turn yet: only SESSION_CREATED.
  assert_eq!(latest_invocation(&[session_created()]), None);
  // One user-message turn.
  let one = vec![
    session_created(),
    invocation_started(1, "hi"),
    start(2, "m1"),
  ];
  assert_eq!(latest_invocation(&one), Some(1));
  // Several turns: the last one wins.
  let several = vec![
    session_created(),
    invocation_started(1, "hi"),
    end(2, "m1"),
    invocation_started(3, "again"),
    start(4, "m2"),
  ];
  assert_eq!(latest_invocation(&several), Some(3));
  // An approval resume is an invocation too.
  let resumed = vec![
    session_created(),
    invocation_started(1, "refund it"),
    approval_resumed(2, "run-2"),
    start(3, "m1"),
  ];
  assert_eq!(latest_invocation(&resumed), Some(2));
}

#[test]
fn connect_reuses_the_journaled_run_id_else_the_invocation_id() {
  let resumed = approval_resumed(1, "run-2");
  assert_eq!(invocation_run_id(&resumed).as_deref(), Some("run-2"));
  // A turn started outside AG-UI (no `ag_ui` extra): the invocation id.
  let plain = invocation_started(1, "hi");
  let EventPayload::InvocationStarted(inv) = &plain.payload else {
    unreachable!()
  };
  assert_eq!(
    invocation_run_id(&plain),
    Some(inv.invocation_id.to_string())
  );
  assert_eq!(invocation_run_id(&start(2, "m1")), None);
}

#[test]
fn connect_context_snapshots_through_the_latest_invocation_without_input() {
  assert!(connect_context("t", &[session_created()]).is_none());
  let events = vec![
    session_created(),
    invocation_started(1, "hi"),
    start(2, "m1"),
    delta(3, "m1", TypedDelta::text("hello")),
    end(4, "m1"),
    invocation_started(5, "again"),
    start(6, "m2"),
  ];
  let (idx, ctx) = connect_context("t", &events).unwrap();
  assert_eq!(idx, 5);
  assert_eq!(ctx.thread_id, "t");
  assert!(ctx.input.is_none());
  // Earlier turns + this turn's user message; not this turn's reply.
  let snapshot = serde_json::to_value(ctx.snapshot.clone().unwrap()).unwrap();
  let contents: Vec<&str> = snapshot
    .as_array()
    .unwrap()
    .iter()
    .map(|m| m["content"].as_str().unwrap())
    .collect();
  assert_eq!(contents, ["hi", "hello", "again"]);
  // RUN_STARTED omits `input` (optional in the 1.0 schema) rather than
  // sending null.
  let mut projector = Projector::new(ctx);
  let started = serde_json::to_value(&projector.push(&events[5])[0]).unwrap();
  assert_eq!(started["type"], "RUN_STARTED");
  assert!(started.get("input").is_none(), "{started}");
}

#[test]
fn fold_uses_the_client_message_id_and_falls_back_to_the_event_id() {
  let mut with_id = invocation_started(1, "hello");
  if let EventPayload::InvocationStarted(i) = &mut with_id.payload {
    i.extra = invocation_extra("r1", Some("client-1".into()), None, None);
  }
  let without = invocation_started(2, "again");
  let fallback = format!("user-{}", without.id);
  let messages = fold(&[with_id, without]);
  assert_eq!(messages[0]["id"], "client-1");
  assert_eq!(messages[1]["id"], fallback);
}

#[test]
fn ag_ui_extras_are_namespaced() {
  let extra = invocation_extra(
    "r1",
    Some("u1".into()),
    Some(json!({"theme": "dark"})),
    Some(vec![]),
  );
  // Exactly one top-level key: everything AG-UI lives under `ag_ui`.
  assert_eq!(
    serde_json::to_value(&extra.map).unwrap(),
    json!({"ag_ui": {"run_id": "r1", "client_message_id": "u1", "forwarded_props": {"theme": "dark"}}})
  );
  let ns = invocation_namespace(&extra).unwrap();
  assert_eq!(ns[RUN_ID_KEY], "r1");
  // Empty `forwardedProps` (HttpAgent's default `{}`) and `context` are not
  // journaled.
  let empty = invocation_extra("r2", None, Some(json!({})), Some(vec![]));
  assert_eq!(
    serde_json::to_value(&empty.map).unwrap(),
    json!({"ag_ui": {"run_id": "r2"}})
  );
  assert!(invocation_namespace(&crate::events::Extra::default()).is_none());
}

/// `end`, with a framework state holding `secret`.
fn end_with_state(seq: i64, id: &str, secret: &str) -> Event {
  let state = serde_json::json!({"adapter": "x", "v": 1, "data": {"signature": secret}});
  ev(
    seq,
    EventPayload::MessageEnd(MessageEnd {
      message_id: id.into(),
      finish_reason: None,
      framework_state: Some(serde_json::from_value(state).unwrap()),
      extra: Extra::default(),
    }),
  )
}

#[test]
fn framework_state_never_reaches_ag_ui() {
  let events = vec![
    invocation_started(1, "hi"),
    start(2, "m1"),
    delta(3, "m1", TypedDelta::reasoning("think")),
    delta(4, "m1", TypedDelta::text("say")),
    end_with_state(5, "m1", "sig-secret"),
  ];
  let (out, _) = run(events.clone());
  let wire = serde_json::to_string(&out).unwrap();
  assert!(wire.contains("TEXT_MESSAGE_END"));
  assert!(!wire.contains("sig-secret"), "{wire}");
  // Nor the connect snapshot.
  let snapshot = serde_json::to_string(&fold_messages(&events)).unwrap();
  assert!(!snapshot.contains("sig-secret"), "{snapshot}");
}

#[test]
fn fold_keeps_reasoning_out_of_assistant_content() {
  let messages = fold(&[
    invocation_started(1, "hi"),
    start(2, "m1"),
    delta(3, "m1", TypedDelta::reasoning("think")),
    delta(4, "m1", TypedDelta::text("say")),
    end(5, "m1"),
  ]);
  let assistant = messages.iter().find(|m| m["role"] == "assistant").unwrap();
  let reasoning = messages.iter().find(|m| m["role"] == "reasoning").unwrap();
  assert_eq!(assistant["content"], "say");
  assert_eq!(reasoning["content"], "think");
  assert_eq!(reasoning["id"], "m1:reasoning");
}

#[test]
fn fold_mints_an_assistant_message_for_a_parentless_tool_call() {
  let messages = fold(&[
    invocation_started(1, "hi"),
    ev(
      2,
      EventPayload::ToolCall(ToolCall {
        tool_call_id: "c1".into(),
        name: "t".into(),
        arguments: json!("{}"),
        extra: Extra::default(),
      }),
    ),
  ]);
  // Same id an AG-UI client mints for a TOOL_CALL_START without a parent.
  assert_eq!(messages[1]["id"], "c1");
  assert_eq!(messages[1]["toolCalls"][0]["function"]["arguments"], "{}");
}

#[test]
fn fold_never_attaches_a_tool_call_to_an_earlier_turns_message() {
  let call = |seq, id: &str| {
    ev(
      seq,
      EventPayload::ToolCall(ToolCall {
        tool_call_id: id.into(),
        name: "t".into(),
        arguments: json!({}),
        extra: Extra::default(),
      }),
    )
  };
  // Turn 1 ends with assistant text; turn 2 opens with a tool-only response
  // (no MESSAGE_START — the openai-agents shape), then its result.
  let messages = fold(&[
    invocation_started(1, "status?"),
    start(2, "m1"),
    delta(3, "m1", TypedDelta::text("Shipped.")),
    end(4, "m1"),
    invocation_started(5, "refund it"),
    call(6, "c2"),
    ev(
      7,
      EventPayload::ToolResult(ToolResult {
        tool_call_id: "c2".into(),
        result: json!("ok"),
        extra: Extra::default(),
      }),
    ),
  ]);
  let shape: Vec<_> = messages
    .iter()
    .map(|m| (m["role"].as_str().unwrap(), m["id"].as_str().unwrap()))
    .collect();
  // The call gets its own message after turn 2's user message (what the live
  // projector's parent-less TOOL_CALL_START makes a client build), and its
  // result follows it — nothing moves above the user's question.
  assert_eq!(shape[1], ("assistant", "m1"));
  assert_eq!(shape[2].0, "user");
  assert_eq!(
    &shape[3..],
    [("assistant", "c2"), ("tool", "tool-result-c2")]
  );
  assert!(messages[1].get("toolCalls").is_none());
}

#[test]
fn resume_entry_and_input_round_trip() {
  let input: RunAgentInput = serde_json::from_value(json!({
    "threadId": "t", "runId": "r", "messages": [],
    "resume": [{"interruptId": "i", "status": "cancelled"}],
  }))
  .unwrap();
  assert!(input.is_resume());
  assert_eq!(input.resume.unwrap()[0].status, ResumeStatus::Cancelled);
}

/// An empty user message is rejected before it reaches a runner: some
/// framework and provider pairs can't send one.
#[test]
fn an_empty_trailing_user_message_is_rejected() {
  for content in [json!(""), json!("  \n\t")] {
    let input: RunAgentInput = serde_json::from_value(json!({
      "threadId": "t", "runId": "r",
      "messages": [{"id": "u1", "role": "user", "content": content}],
    }))
    .unwrap();
    let err = input.trailing_user_input().unwrap_err();
    assert!(err.contains("empty"), "{content}: {err}");
  }
}

#[test]
fn trailing_user_input_rules() {
  let mk = |messages: Value| -> RunAgentInput {
    serde_json::from_value(json!({"threadId": "t", "runId": "r", "messages": messages})).unwrap()
  };
  let ok = mk(json!([{"id": "u1", "role": "user", "content": "hi"}]))
    .trailing_user_input()
    .unwrap();
  assert_eq!(ok.input.as_str(), "hi");
  assert_eq!(ok.message_id.as_deref(), Some("u1"));
  assert!(!ok.has_history);

  // Prior conversation messages are history; system/developer prompts are not.
  let with_system = mk(json!([
    {"id": "s", "role": "system", "content": "be brief"},
    {"id": "d", "role": "developer", "content": "be nice"},
    {"id": "u1", "role": "user", "content": "hi"}
  ]));
  assert!(!with_system.trailing_user_input().unwrap().has_history);
  for role in ["user", "assistant", "tool", "reasoning"] {
    let with_history = mk(json!([
      {"id": "p", "role": role, "content": "earlier"},
      {"id": "u2", "role": "user", "content": "hi"}
    ]));
    assert!(
      with_history.trailing_user_input().unwrap().has_history,
      "{role}"
    );
  }

  let parts = json!([{"type": "text", "text": "hi"}]);
  let err = mk(json!([{"id": "u1", "role": "user", "content": parts}]))
    .trailing_user_input()
    .unwrap_err();
  assert!(err.contains("content parts are not supported"), "{err}");

  assert!(mk(json!([])).trailing_user_input().is_err());
  assert!(
    mk(json!([{"id": "a", "role": "assistant", "content": "x"}]))
      .trailing_user_input()
      .is_err()
  );
  assert!(
    mk(json!([{"id": "u", "role": "user"}]))
      .trailing_user_input()
      .is_err()
  );
}
