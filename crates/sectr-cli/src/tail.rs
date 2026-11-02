//! `sectr tail` — follow a session's journal. Never stops until ctrl-c
//! (unlike `sectr send`, which stops when the invocation it caused is
//! over). Grepping / journal searching land here later; a future
//! `--cloud` connects the same client to a cloud gateway.

use std::path::PathBuf;

use clap::Args;

use crate::client::{GatewayClient, StreamMode};

#[derive(Args, Debug)]
pub struct TailArgs {
  /// Project directory (must contain a `.agent/dev.json` from `sectr dev`).
  #[arg(long, default_value = ".")]
  pub path: PathBuf,
  /// Session to follow (all-sessions tail is future work).
  #[arg(long, required = true)]
  pub session: Option<uuid::Uuid>,
  /// CLOUD mode: follow a session on a sectr-server (auth/server
  /// from --api/--key, the env, or `sectr auth login`).
  #[arg(long)]
  pub cloud: bool,
  /// Server base (cloud mode). Defaults through `sectr auth login`.
  #[arg(long)]
  pub api: Option<String>,
  /// Platform key (cloud mode). Defaults through `sectr auth login`.
  #[arg(long)]
  pub key: Option<String>,
  /// Raw event envelopes (one JSON per line) instead of the human renderer.
  #[arg(long)]
  pub json: bool,
}

pub async fn run_tail(args: TailArgs) -> anyhow::Result<()> {
  let client = if args.cloud {
    let target = crate::auth::resolve(args.api.as_deref(), args.key.as_deref())?;
    GatewayClient::for_cloud(&target.api, &target.key)
  } else {
    GatewayClient::discover_local(&args.path)?
  };
  let sid = args.session.expect("clap: --session is required");
  if !args.json {
    eprintln!("following session {sid} — ctrl-c to stop");
  }
  // Full history from seq 0, then live — never returns on its own.
  client
    .stream_session(sid, 0, StreamMode::Tail, args.json)
    .await
}
