//! What each framework sends back on its next turn, run natively (no Sectr,
//! keeping the conversation its own way): the reference for the `reasoning`
//! scenario's later turns. For every adapter on each endpoint
//! that returns reasoning, it runs `calibrate/native_history.py` against
//! the mock, answers turn 1 with reasoning (then text, or a call and an
//! answer), and prints the turn-2 request's history:
//!
//!     cargo run -q -p sectr-conformance --example native_history
//!
//! Needs the generated projects' Python env (`just conformance` syncs it).

use std::path::Path;
use std::process::Stdio;

use anyhow::{Result, bail};
use sectr_conformance::adapter::Adapter;
use sectr_conformance::endpoint::Endpoint;
use sectr_conformance::mock::{MockServer, Reply, Request, call, reasoning, text};
use serde_json::json;
use tokio::io::AsyncReadExt;

const ENDPOINTS: [Endpoint; 3] = [
  Endpoint::OpenRouter,
  Endpoint::OpenAiResponses,
  Endpoint::Anthropic,
];

#[tokio::main]
async fn main() -> Result<()> {
  let root = Path::new(env!("CARGO_MANIFEST_DIR"));
  let python = root.join("../../sdk-python/tests/conformance/gen/.venv/bin/python");
  let script = root.join("calibrate/native_history.py");
  let mock = MockServer::start().await?;
  for shape in ["text", "tool"] {
    for adapter in Adapter::ALL {
      for endpoint in ENDPOINTS {
        let id = format!("{}-{}-{shape}", adapter.dir(), endpoint.id());
        let mut run = mock.register(&id, endpoint, &[]);
        let mut child = tokio::process::Command::new(&python)
          .arg("-I")
          .arg(&script)
          .args([adapter.dir(), shape])
          .env("SECTR_CONFORMANCE_URL", run.base_url())
          .env("SECTR_CONFORMANCE_ENDPOINT", endpoint.id())
          .stdout(Stdio::null())
          .stderr(Stdio::piped())
          .kill_on_drop(true)
          .spawn()?;
        // Turn 1's responses, then turn 2's, whose history is printed.
        let mut replies: Vec<Reply> = match shape {
          "text" => vec![reasoning("Just a greeting.").and(text("Hello! How can I help?"))],
          _ => vec![
            reasoning("They want order 42's status.").and(call(
              "call_1",
              "lookup_order",
              json!({"order_id": "42"}),
            )),
            text("It has shipped."),
          ],
        };
        replies.reverse();
        let mut stderr = child.stderr.take().expect("piped");
        loop {
          let incoming = tokio::select! {
            incoming = run.next() => incoming,
            status = child.wait() => {
              let mut err = String::new();
              stderr.read_to_string(&mut err).await?;
              println!("{id}: exited {} before turn 2: {}", status?, err.lines().last().unwrap_or(""));
              break;
            }
          };
          let model = match incoming {
            Some(Ok(Request::Model(model))) => model,
            Some(Ok(Request::Tool(exec))) => {
              bail!("{id}: unexpected tool execution {}", exec.name())
            }
            Some(Err(rejected)) => {
              println!("{id}: REJECTED {rejected}");
              break;
            }
            None => bail!("{id}: the mock stopped"),
          };
          match replies.pop() {
            Some(reply) => model.reply(reply).await?,
            None => {
              println!("{id}:");
              for msg in model.history() {
                println!("  {msg:?}");
              }
              model.reply(text("Have a nice day.")).await?;
              child.wait().await?;
              break;
            }
          }
        }
      }
    }
  }
  Ok(())
}
