//! Test support: a fake invoke-contract runner — a python3-stdlib HTTP
//! server that replays a scripted list of runner-event frames as an SSE
//! body, then exits. `SECTR_FRAMES_FILE` points at the script (JSON array
//! of runner-event JSON strings), `SECTR_PORT` at the port to serve.
//!
//! Shared by dispatch tests (worker) and `sectr send` tests (CLI) — one
//! copy so the two harnesses can't drift.

use sectr_common::events::{
  EventPayload, MessageDelta, NoMoreActions, RunnerEvent, ToolApprovalRequired, TypedDelta,
};
use std::path::Path;

/// Write the fake runner script to a fresh temp path and return the
/// runner command (`python3 <script>`) — the SubprocessDriver command
/// shape. The frames file is passed separately via `script_turn`.
pub fn fake_runner_command() -> Vec<String> {
  let script = std::env::temp_dir().join(format!("sectr-fake-runner-{}.py", uuid::Uuid::now_v7()));
  std::fs::write(
    &script,
    r#"
import http.server, json, os

class Handler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        self.rfile.read(length)
        with open(os.environ["SECTR_FRAMES_FILE"]) as f:
            frames = json.load(f)
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        for frame in frames:
            self.wfile.write(b"data: " + frame.encode() + b"\n\n")
        self.wfile.flush()

http.server.HTTPServer(("127.0.0.1", int(os.environ["SECTR_PORT"])), Handler).serve_forever()
"#,
  )
  .unwrap();
  vec!["python3".into(), script.to_string_lossy().into_owned()]
}

/// Runner-event frame as an SSE `data:` string (what the fake runner
/// writes into its response body).
pub fn frame_json(payload: EventPayload) -> String {
  RunnerEvent::new(payload).unwrap().to_json_string()
}

pub fn delta_frame(text: &str) -> String {
  frame_json(EventPayload::MessageDelta(MessageDelta {
    message_id: "m1".into(),
    delta: TypedDelta::text(text),
    extra: Default::default(),
  }))
}

pub fn approval_frame() -> String {
  frame_json(EventPayload::ToolApprovalRequired(ToolApprovalRequired {
    tool_call_id: "call_1".into(),
    name: "issue_refund".into(),
    arguments: serde_json::json!({}),
    reason: None,
    extra: Default::default(),
  }))
}

pub fn final_frame() -> String {
  frame_json(EventPayload::NoMoreActions(NoMoreActions::default()))
}

/// Script the frames the fake runner will replay.
pub fn script_turn(path: &Path, frames: &[String]) {
  std::fs::write(path, serde_json::to_string(frames).unwrap()).unwrap();
}
