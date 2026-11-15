//! `sectr` — the developer-facing CLI. Deliberately thin: embeds the
//! sectr-worker engine for `sectr dev`, shells out to the user's Python for
//! deploy-time inspection.

#![cfg_attr(
  test,
  allow(
    clippy::disallowed_macros,
    reason = "tests build JSON fixtures with `json!`"
  )
)]

mod auth;
mod auth_cmd;
mod browser;
mod client;
mod deploy;
mod dev;
mod endpoint;
mod init;
mod secrets;
mod send;
mod tail;
mod telemetry;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "sectr", version, about = "Host AI agents — starting locally")]
struct Cli {
  #[command(subcommand)]
  command: Commands,
}

#[derive(Subcommand)]
enum Commands {
  /// Store the platform key for cloud commands (login/whoami/logout).
  Auth(auth_cmd::AuthArgs),
  /// Scaffold a new agent project: framework, frontend and where-it-runs
  /// pickers (or --template / --frontend / --endpoint), file emission, `uv
  /// sync`, and a smoke inspect. `--endpoint` in an existing project adds
  /// the files to run it on your own platform. See `init --help`.
  Init(init::InitArgs),
  /// Run the app locally end-to-end: SQLite journal, subprocess runner,
  /// gateway for clients. See `dev --help`.
  Dev(dev::DevArgs),
  /// Send a message / decide an approval through the running `sectr dev`
  /// gateway (a thin client — it never starts the engine).
  Send(send::SendArgs),
  /// Follow a session's journal (never stops until ctrl-c).
  Tail(tail::TailArgs),
  /// Ship the project to a server: package → upload → the server builds
  /// and activates. With `[deploy] endpoint` in sectr.toml, register your
  /// own compute endpoint instead. See `deploy --help`.
  Deploy(deploy::DeployArgs),
  /// Manage the app's server-side secrets (write-only: set/list/rm).
  Secrets(secrets::SecretsArgs),
  /// Anonymous usage pings: turn them on or off, or see exactly what one
  /// sends (on/off/status).
  Telemetry(telemetry::TelemetryArgs),
}

#[tokio::main]
async fn main() {
  let cli = Cli::parse();
  // The anonymous usage ping, when one is due, goes out while the command
  // runs. `sectr telemetry` itself never pings. A `--json` run prints
  // nothing about it: a program is reading that output.
  let ping = match &cli.command {
    Commands::Telemetry(_) => None,
    Commands::Send(args) => telemetry::prepare(args.json).map(telemetry::spawn),
    Commands::Tail(args) => telemetry::prepare(args.json).map(telemetry::spawn),
    _ => telemetry::prepare(false).map(telemetry::spawn),
  };
  let (name, result) = match cli.command {
    Commands::Auth(args) => ("auth", auth_cmd::run_auth(args).await),
    Commands::Init(args) => ("init", init::run_init(args).await),
    Commands::Dev(args) => ("dev", dev::run_dev(args).await),
    Commands::Send(args) => ("send", send::run_send(args).await),
    Commands::Tail(args) => ("tail", tail::run_tail(args).await),
    Commands::Deploy(args) => ("deploy", deploy::run_deploy(args).await),
    Commands::Secrets(args) => ("secrets", secrets::run_secrets(args).await),
    Commands::Telemetry(args) => ("telemetry", telemetry::run_telemetry(args)),
  };
  // One exit point, after the ping had its moment: exiting inside an arm
  // would cut it off.
  if let Some(ping) = ping {
    ping.finish().await;
  }
  if let Err(e) = result {
    eprintln!("sectr {name}: {e:#}");
    std::process::exit(1);
  }
}
