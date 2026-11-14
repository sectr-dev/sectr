//! `sectr send` — send a message (or a decision) to the running `sectr
//! dev` gateway. Thin [`GatewayClient`] wrapper: no engine code, no
//! auto-start.
//!
//! Shape (CLI design discussion, 2026-09-11):
//! - `sectr send "msg"` — new session, follow the turn until it's over
//!   (turn end or approval pause)
//! - `sectr send "msg" --session <id>` — follow-up turn, same stop rules
//! - `sectr send` / `sectr send --session <id>` — message prompted on the
//!   terminal (one line), or read from all of stdin when piped
//! - `--no-tail` — send, print the session id, exit (scripting)
//! - `--approve/--deny --session <id>` — decide the pending approval
//! - `sectr tail` is the dedicated observer (see tail.rs)

use std::io::Write as _;
use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::Args;

use crate::client::{GatewayClient, StreamMode};

#[derive(Args, Debug)]
pub struct SendArgs {
  /// Project directory (must contain a `.agent/dev.json` from `sectr dev`).
  #[arg(long, default_value = ".")]
  pub path: PathBuf,
  /// The message (positional). Omitted: prompted on the terminal, or
  /// read from all of stdin when piped.
  #[arg(conflicts_with_all = ["approve", "deny"])]
  pub message: Option<String>,
  /// Existing session (a plain `sectr send "msg"` creates one).
  #[arg(long)]
  pub session: Option<uuid::Uuid>,
  /// Approve the session's pending tool call (resumes the turn).
  #[arg(long, requires = "session", conflicts_with = "deny")]
  pub approve: bool,
  /// Deny the session's pending tool call (resumes with the rejection).
  #[arg(long, requires = "session", conflicts_with = "approve")]
  pub deny: bool,
  /// Don't follow the turn's output — send, print the session id, exit.
  #[arg(long)]
  pub no_tail: bool,
  /// Raw event envelopes (one JSON per line) instead of the human renderer.
  #[arg(long)]
  pub json: bool,
  /// CLOUD mode: talk to a sectr-server instead of the local dev
  /// gateway. Requires --app; auth/server come from --api/--key, the
  /// env, or `sectr auth login` (shared resolution).
  #[arg(long, requires = "app")]
  pub cloud: bool,
  /// The app slug (cloud mode) — as `sectr deploy` created it.
  #[arg(long)]
  pub app: Option<String>,
  /// Route path (cloud mode). Default: the app's first route.
  #[arg(long)]
  pub route: Option<String>,
  /// Server base (cloud mode). Defaults through `sectr auth login`.
  #[arg(long)]
  pub api: Option<String>,
  /// Platform key (cloud mode). Defaults through `sectr auth login`.
  #[arg(long)]
  pub key: Option<String>,
}

pub async fn run_send(args: SendArgs) -> anyhow::Result<()> {
  let client = if args.cloud {
    let target = crate::auth::resolve(args.api.as_deref(), args.key.as_deref())?;
    GatewayClient::for_cloud(&target.api, &target.key)
  } else {
    GatewayClient::discover_local(&args.path)?
  };

  // A decision: resume the suspended turn. Same tail rules as a message.
  if args.approve || args.deny {
    let sid = args.session.expect("clap enforces requires = session");
    if !args.json {
      println!("session: {sid}");
    }
    let baseline = client.session_event_count(sid).await?;
    client.decide(sid, args.approve).await?;
    return finish(&client, sid, baseline, &args).await;
  }

  let message = match &args.message {
    Some(m) => m.clone(),
    None => prompt_message()?,
  };

  let (sid, baseline) = match args.session {
    Some(sid) => {
      let baseline = client.session_event_count(sid).await?;
      client.send_input(sid, &message).await?;
      (sid, baseline)
    }
    None if args.cloud => {
      // Cloud session: the app is addressed by slug. With --route
      // the session is created DIRECTLY (no control-plane call —
      // invocation keys are per-app data-plane credentials and get
      // 403 on GET /apps); without one, the platform key resolves
      // the app's first manifest route.
      let slug = args.app.as_deref().expect("clap: --cloud requires --app");
      let route = match &args.route {
        Some(r) => r.clone(),
        None => {
          let (_id, routes) = client.cloud_find_app(slug).await?;
          routes
            .first()
            .context("the app has no routes — deploy one first")?
            .clone()
        }
      };
      let sid = client.create_session(slug, &route, &message).await?;
      (sid, 0)
    }
    None => {
      let status = client.dev_status().await?;
      let route = status
        .routes
        .first()
        .context("the app has no routes — add one to the AgentApp")?;
      let sid = client.create_session(&status.app, route, &message).await?;
      (sid, 0)
    }
  };
  if !args.json {
    println!("session: {sid}");
  }
  finish(&client, sid, baseline, &args).await
}

/// Follow the turn unless `--no-tail` was given.
async fn finish(
  client: &GatewayClient,
  sid: uuid::Uuid,
  baseline: i64,
  args: &SendArgs,
) -> anyhow::Result<()> {
  if args.no_tail {
    return Ok(());
  }
  client
    .stream_session(sid, baseline, StreamMode::Run, args.json)
    .await
}

/// The message prompt: on a TTY, one line after a stderr prompt (stdout
/// stays clean for event output / --json); piped, read all of stdin.
fn prompt_message() -> anyhow::Result<String> {
  use std::io::IsTerminal;
  let stdin = std::io::stdin();
  let tty = stdin.is_terminal();
  if tty {
    eprint!("message: ");
    std::io::stderr().flush().ok();
  }
  read_message(&mut stdin.lock(), tty)
}

/// Read the message: one line on a TTY, everything until EOF when piped.
fn read_message(input: &mut dyn std::io::BufRead, tty: bool) -> anyhow::Result<String> {
  let mut buf = String::new();
  if tty {
    input.read_line(&mut buf)?;
  } else {
    input.read_to_string(&mut buf)?;
  }
  let message = buf.trim().to_string();
  if message.is_empty() {
    bail!("empty message");
  }
  Ok(message)
}

#[cfg(test)]
mod tests;
