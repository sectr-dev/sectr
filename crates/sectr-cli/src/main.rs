//! `sectr` — the developer-facing CLI. Deliberately thin: embeds the
//! sectr-worker engine for `sectr dev`, shells out to the user's Python for
//! deploy-time inspection.

mod auth;
mod auth_cmd;
mod browser;
mod client;
mod deploy;
mod dev;
mod init;
mod secrets;
mod send;
mod tail;

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
  /// Scaffold a new agent project: framework and frontend pickers (or
  /// --template / --frontend), file emission, `uv sync`, and a smoke
  /// inspect. See `init --help`.
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
  /// and activates. See `deploy --help`.
  Deploy(deploy::DeployArgs),
  /// Manage the app's server-side secrets (write-only: set/list/rm).
  Secrets(secrets::SecretsArgs),
}

#[tokio::main]
async fn main() {
  let cli = Cli::parse();
  match cli.command {
    Commands::Auth(args) => {
      if let Err(e) = auth_cmd::run_auth(args).await {
        eprintln!("sectr auth: {e:#}");
        std::process::exit(1);
      }
    }
    Commands::Init(args) => {
      if let Err(e) = init::run_init(args).await {
        eprintln!("sectr init: {e:#}");
        std::process::exit(1);
      }
    }
    Commands::Dev(args) => {
      if let Err(e) = dev::run_dev(args).await {
        eprintln!("sectr dev: {e:#}");
        std::process::exit(1);
      }
    }
    Commands::Send(args) => {
      if let Err(e) = send::run_send(args).await {
        eprintln!("sectr send: {e:#}");
        std::process::exit(1);
      }
    }
    Commands::Tail(args) => {
      if let Err(e) = tail::run_tail(args).await {
        eprintln!("sectr tail: {e:#}");
        std::process::exit(1);
      }
    }
    Commands::Deploy(args) => {
      if let Err(e) = deploy::run_deploy(args).await {
        eprintln!("sectr deploy: {e:#}");
        std::process::exit(1);
      }
    }
    Commands::Secrets(args) => {
      if let Err(e) = secrets::run_secrets(args).await {
        eprintln!("sectr secrets: {e:#}");
        std::process::exit(1);
      }
    }
  }
}
