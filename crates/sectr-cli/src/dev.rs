//! `sectr dev` — the local engine + gateway (schema/dev-api.yaml).
//!
//! `sectr dev` is LONG-RUNNING and
//! is the ONLY process that embeds the engine — every other surface
//! (`sectr send`, the dashboard, CI scripts) is a pure HTTP client of this
//! gateway. One terminal, one engine; ctrl-c drains in-flight turns, then
//! exits.
//!
//! Shape per invocation:
//! - parse `sectr.toml` → manifest via `sectr.inspect` (real import)
//! - idempotent bootstrap (`.agent/dev.json` + `.agent/dev.db`)
//! - engine + shared sessions-api router (`sectr_common::api`) + `/dev`
//!   extras, bound to 127.0.0.1 on a stable port (`--port` /
//!   `[dev] port` pinned, else this project's last port, else 8420), with
//!   the gateway URL persisted in `dev.json` for client discovery
//! - reconciler (lease reaping) spawned alongside
//! - ctrl-c → stop accepting, drain in-flight turn tasks, exit
//! - on start (unless --no-open): opens the dashboard pointed at this
//!   gateway (`?gateway=<base>` — the dashboard persists it; the gateway
//!   serves CORS so the browser can reach localhost directly)

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use axum::http::{HeaderName, Method, header::CONTENT_TYPE};
use axum::response::IntoResponse;
use clap::Args;
use sectr_common::api::agui::{agui_cors_layer, agui_router};
use sectr_common::api::{DevStatus, sessions_router};
use sectr_common::drivers::{EnvFileSecretSource, InMemorySecretSource, SubprocessDriver};
use sectr_common::inspect;
use sectr_common::journal::{Journal, SqliteJournal};
use sectr_common::reconcile::Reconciler;
use sectr_common::registry::{DeploymentStatus, Registry, SqliteRegistry};
use sectr_common::sidecar::{APPROVAL_POLICY_ENV, APPROVAL_POLICY_ENV_IS_RESERVED};
use sectr_common::store::SqliteStore;
use sectr_worker::dispatch::{DevDispatch, DispatchConfig};
use sectr_worker::service::{AppInfo, DevSessionService};
use serde::{Deserialize, Serialize};
use tower_http::cors::CorsLayer;

#[derive(Args, Debug)]
pub struct DevArgs {
  /// Project directory (must contain sectr.toml).
  #[arg(long, default_value = ".")]
  pub path: PathBuf,
  /// Config file path (relative to --path).
  #[arg(long, default_value = "sectr.toml")]
  pub config: PathBuf,
  /// Don't open the dashboard (CI / containers).
  #[arg(long)]
  pub no_open: bool,
  /// Dashboard to open. Default: the saved login's dashboard, else
  /// https://app.sectr.dev. Point it at a local `next dev` when working on
  /// the dashboard itself.
  #[arg(long, env = "SECTR_DASHBOARD_URL")]
  pub dashboard: Option<String>,
  /// Gateway port. Default: the port this project used last, else 8420.
  /// A port given here is strict: if it's busy, `sectr dev` exits.
  #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
  pub port: Option<u16>,
}

#[derive(Debug, Deserialize)]
struct SectrToml {
  #[serde(default)]
  app: Option<AppSection>,
  agent: AgentSection,
  #[serde(default)]
  dev: Option<DevSection>,
  #[serde(default)]
  deploy: Option<DeploySection>,
}

#[derive(Debug, Deserialize)]
struct AppSection {
  #[serde(default)]
  name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AgentSection {
  /// The AgentApp location, `module:attribute` or bare module.
  entry: String,
  /// Informational (enforced by the runner env); not used by the CLI.
  #[serde(default)]
  #[allow(dead_code)]
  python: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DevSection {
  /// Override the runner command (default: `uv run python -m sectr.server
  /// <entry>`).
  #[serde(default)]
  command: Option<Vec<String>>,
  /// .env path relative to the project (default `.env`; missing file = no
  /// secrets).
  #[serde(default)]
  env_file: Option<String>,
  /// Gateway port, strict like `--port` (which overrides it).
  #[serde(default)]
  port: Option<u16>,
}

#[derive(Debug, Deserialize)]
struct DeploySection {
  /// Tools that need approval whatever the code says: the deployment's
  /// approval policy, applied in dev as it is deployed.
  #[serde(default)]
  approval_tools: Vec<String>,
}

/// Persisted state: bootstrap ids across invocations (the Registry has no
/// by-slug lookup in MVP), the port this project bound last, and the live
/// gateway coordinates, so `sectr send` and the dashboard can discover the
/// gateway. `gateway_*` fields exist only while a dev process owns the
/// project.
#[derive(Debug, Serialize, Deserialize)]
struct DevState {
  org_id: uuid::Uuid,
  app_id: uuid::Uuid,
  deployment_id: uuid::Uuid,
  version: i64,
  /// The manifest this deployment was created from — a changed manifest
  /// bumps the version on the next invocation.
  manifest: serde_json::Value,
  /// The gateway port this project bound last; preferred on the next run
  ///. Survives shutdown.
  #[serde(default)]
  port: Option<u16>,
  /// Live only (cleared on shutdown): the running gateway's pid.
  #[serde(default)]
  gateway_pid: Option<u32>,
  /// Live only: the gateway's base URL (`https://local.sectr.dev:<port>`
  /// with TLS, else `http://127.0.0.1:<port>`) — what clients read.
  #[serde(default)]
  gateway_url: Option<String>,
}

/// `sectr dev`'s port when nothing pins it and the project has no
/// remembered port. Off the common dev ports (3000, 4321, 5173,
/// 8000, 8080, 8787, …), so docs and the starter can hard-code it.
const DEFAULT_GATEWAY_PORT: u16 = 8420;
/// How many ports a preferred (unpinned) port scans before falling back
/// to an OS-assigned one.
const PORT_SCAN: u16 = 20;

struct ParsedConfig {
  agent: AgentSection,
  app: Option<AppSection>,
  dev: Option<DevSection>,
  /// `[deploy] approval_tools`.
  approval_tools: Vec<String>,
}

pub async fn run_dev(args: DevArgs) -> anyhow::Result<()> {
  let project = args.path.clone();
  let config = parse_sectr_toml(&project.join(&args.config))?;
  // `sectr dev` works with no login, or a broken credentials file.
  let saved_login = crate::auth::load().ok().flatten();
  let dashboard = choose_dashboard(
    args.dashboard.as_deref(),
    saved_login.as_ref().map(|c| c.dashboard.as_str()),
  );
  let env_rel = config
    .dev
    .as_ref()
    .and_then(|d| d.env_file.clone())
    .unwrap_or_else(|| ".env".to_string());
  let env_path = project.join(&env_rel);
  check_approval_policy_env(std::env::var_os(APPROVAL_POLICY_ENV).is_some(), &env_path)?;

  // Single-engine guard: another `sectr dev` already owns this project?
  // Checked on the state as the last run left it — before bootstrap,
  // which rewrites dev.json without the live coordinates.
  let previous = read_state(&project.join(".agent/dev.json"))?;
  check_not_running(previous.as_ref())?;
  let port_req = port_request(&args, &config, previous.as_ref());

  // Manifest via the SDK inspector: a real import of the user's module —
  // validates the entry and yields routes + env names.
  let manifest = run_inspect(&project, &config.agent.entry).await?;
  let app_name = manifest
    .get("name")
    .and_then(|v| v.as_str())
    .map(str::to_string)
    .or_else(|| config.app.as_ref().and_then(|a| a.name.clone()))
    .context("manifest has no app name")?;
  println!("sectr dev: app {app_name:?} entry {}", config.agent.entry);
  if !config.approval_tools.is_empty() {
    println!(
      "sectr dev: tools needing approval ([deploy] approval_tools): {}",
      config.approval_tools.join(", ")
    );
  }
  // Static-scan findings from the SDK inspector (e.g. LangGraph's native
  // `interrupt()`, which sectr cannot resume).
  for warning in manifest
    .get("warnings")
    .and_then(|v| v.as_array())
    .into_iter()
    .flatten()
    .filter_map(|w| w.as_str())
  {
    eprintln!("sectr dev: warning: {warning}");
  }

  // Local store: everything dev persists lives in .agent/ (gitignored).
  let agent_dir = project.join(".agent");
  std::fs::create_dir_all(&agent_dir)?;
  let store = Arc::new(SqliteStore::open(&agent_dir.join("dev.db").to_string_lossy()).await?);
  let mut state = bootstrap(store.as_ref(), &agent_dir, &manifest, &app_name).await?;

  // The runner: the SDK's invoke-contract server for the user's app
  // (`sectr.server` resolves the entry, serves POST /invocations on
  // $SECTR_PORT). [dev] command overrides for non-SDK runners.
  let runner_command = config
    .dev
    .as_ref()
    .and_then(|d| d.command.clone())
    .unwrap_or_else(|| {
      vec![
        "uv".into(),
        "run".into(),
        "python".into(),
        "-m".into(),
        "sectr.server".into(),
        config.agent.entry.clone(),
      ]
    });

  let journal: Arc<dyn Journal> = Arc::new(SqliteJournal::new(store.pool().clone()));
  let registry: Arc<dyn Registry> = Arc::new(SqliteRegistry::new(store.pool().clone()));

  // Liveness: the reconciler owns expired leases (skeleton no-op for now).
  let reconciler = Reconciler::new(registry.clone(), journal.clone(), Duration::from_secs(15));
  tokio::spawn(reconciler.run());

  let secret_source: Arc<dyn sectr_common::drivers::SecretSource> = if env_path.exists() {
    Arc::new(EnvFileSecretSource::new(&env_path))
  } else {
    Arc::new(InMemorySecretSource::new())
  };

  // One wiring point: the ids live in AppInfo; the dispatch takes them
  // from it, and the service takes its store handles from the dispatch.
  let app_info = AppInfo {
    app_id: state.app_id,
    app_name: app_name.clone(),
    deployment_id: state.deployment_id,
    deployment_version: state.version,
    manifest: manifest.clone(),
  };
  let dispatch = Arc::new(DevDispatch::new(
    journal.clone(),
    registry.clone(),
    Arc::new(sectr_worker::turn::TurnEngine::new()),
    Arc::new(SubprocessDriver::new()),
    secret_source,
    DispatchConfig {
      lease_ttl: chrono::Duration::seconds(60),
      ready_deadline: Duration::from_secs(30),
      ready_poll_interval: Duration::from_millis(250),
      runner_command,
      runner_workdir: Some(project.clone()),
      runner_env: vec![],
      runner_port: None,
      approval_tools: config.approval_tools.clone(),
      env_file: Some(env_path),
    },
    app_info.app_id,
    app_info.deployment_id,
  ));

  let service = Arc::new(DevSessionService::new(dispatch, app_info));

  // A stable port, remembered per project and its URL persisted
  // for client discovery. TLS when a `local.sectr.dev` cert is installed
  // (browser-friendly: the deployed https console may not fetch
  // http://localhost — Safari blocks mixed content); plain HTTP otherwise.
  let tls = load_tls().await;
  let listener = bind_gateway(&port_req)?;
  let port = listener.local_addr()?.port();
  let gateway_url = match &tls {
    Some(_) => format!("https://local.sectr.dev:{port}"),
    None => format!("http://127.0.0.1:{port}"),
  };
  if let Some(warning) = port_warning(&port_req, port) {
    eprintln!("{warning}");
  }
  state.port = Some(port);
  state.gateway_pid = Some(std::process::id());
  state.gateway_url = Some(gateway_url.clone());
  write_state(&agent_dir.join("dev.json"), &state)?;

  let router = sessions_router(service.clone())
    .merge(dev_extras_router(
      store.clone(),
      port,
      &project,
      &app_name,
      &state,
      manifest_routes(&manifest),
    ))
    .layer(gateway_cors(dashboard_origin(&dashboard)))
    // AG-UI: the same endpoint as prod, no auth (like every dev
    // route). Merged AFTER the dashboard-only CORS layer — a frontend
    // under `next dev` (any localhost origin) calls it with a
    // preflighted POST, which that layer would answer wrongly.
    .merge(agui_router(service.clone(), journal.clone()).layer(agui_cors_layer()));

  let dashboard_url = dashboard_open_url(
    &dashboard,
    &state.app_id.to_string(),
    manifest_routes(&manifest).first().map(String::as_str),
    &gateway_url,
  );
  println!("gateway: {gateway_url}  (sessions api + /dev + AG-UI; ctrl-c to stop)");
  for route in manifest_routes(&manifest) {
    let endpoint = format!(
      "{gateway_url}/ag-ui/{app_name}/{}",
      route.trim_start_matches('/')
    );
    println!("- 🔌 connect via AG-UI at: {endpoint}");
  }

  if tls.is_none() {
    println!("note: no TLS certificate for local.sectr.dev, so the gateway is using plain HTTP");
  }
  println!("dashboard: {dashboard_url}");
  if !args.no_open {
    if crate::browser::open_browser(&dashboard_url) {
      println!("opened the dashboard pointed at this gateway");
    } else {
      println!("could not spawn a browser — open the dashboard URL above");
    }
  }

  let server = tokio::spawn(async move {
    match tls {
      Some(config) => {
        axum_server::from_tcp_rustls(listener, config)
          .serve(router.into_make_service())
          .await
      }
      None => {
        // tokio's from_std PANICS on a blocking socket — the std
        // bind above is blocking by default (panic seen on first
        // runs without a cert: "Registering a blocking socket with
        // the tokio runtime is unsupported"). The TLS branch is
        // unaffected (axum-server drives the std listener itself).
        listener
          .set_nonblocking(true)
          .expect("listener non-blocking");
        axum::serve(
          tokio::net::TcpListener::from_std(listener).expect("listener"),
          router,
        )
        .await
      }
    }
    .expect("gateway server");
  });

  // SIGINT (ctrl-c) or SIGTERM (kill/pkill/systemd) — both drain.
  #[cfg(unix)]
  let term = async {
    use tokio::signal::unix::{SignalKind, signal};
    signal(SignalKind::terminate())
      .expect("install SIGTERM handler")
      .recv()
      .await;
  };
  #[cfg(not(unix))]
  let term = std::future::pending::<()>();

  tokio::select! {
      res = tokio::signal::ctrl_c() => {
          if let Err(e) = res {
              eprintln!("signal error: {e}");
          }
      }
      () = term => {}
  }
  println!("\nshutting down — draining in-flight turns…");
  // Turn-aware shutdown: let running turns finish (the journal must not be
  // abandoned mid-write; a killed process recovers via lease TTL +
  // reconciler instead).
  service.shutdown().await;
  server.abort();
  // Release the discovery coordinates so a client doesn't find a stale
  // gateway; the remembered port stays for the next run.
  stopped(&mut state);
  write_state(&agent_dir.join("dev.json"), &state)?;
  println!("sectr dev stopped.");
  Ok(())
}

// ---- dev-only extras (/dev namespace — dev-api.yaml, NOT public) --------

/// Manifest routes as plain paths (`{"routes":[{"path":"/chat"}]}` →
/// `["/chat"]`) — surfaced via `/dev/status` so clients (`sectr send`, the
/// dashboard) can create sessions without re-inspecting the app.
fn manifest_routes(manifest: &serde_json::Value) -> Vec<String> {
  manifest
    .get("routes")
    .and_then(|v| v.as_array())
    .map(|a| {
      a.iter()
        .filter_map(|r| r.get("path").and_then(|p| p.as_str()).map(str::to_string))
        .collect()
    })
    .unwrap_or_default()
}

/// The app's framework adapter — the SDK inspector records each route
/// handler's `sectr_framework` marker (e.g. openai-agents); the first one
/// found stands for the app. None when handlers carry no marker.
fn manifest_framework(manifest: &serde_json::Value) -> Option<String> {
  manifest
    .get("routes")
    .and_then(|v| v.as_array())
    .and_then(|a| {
      a.iter().find_map(|r| {
        r.get("framework")
          .and_then(|f| f.as_str())
          .map(str::to_string)
      })
    })
}

fn dev_extras_router(
  store: Arc<SqliteStore>,
  port: u16,
  project: &Path,
  app_name: &str,
  state: &DevState,
  routes: Vec<String>,
) -> axum::Router {
  // Label: the project's basename. `file_name()` is None for path forms
  // like "." (the `--path` default), so canonicalize as a fallback — that
  // resolves "." to the actual working directory — and keep the raw path
  // only as a last resort.
  let project_label = project
    .file_name()
    .map(|s| s.to_string_lossy().into_owned())
    .or_else(|| {
      project
        .canonicalize()
        .ok()
        .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
    })
    .unwrap_or_else(|| project.to_string_lossy().into_owned());
  let status = DevStatus {
    project: project_label,
    app: app_name.to_string(),
    app_id: state.app_id.to_string(),
    gateway_port: port,
    framework: manifest_framework(&state.manifest),
    routes,
  };
  let reset_store = store.clone();
  axum::Router::new()
    .route(
      "/dev/status",
      axum::routing::get(move || {
        let status = status.clone();
        async move { axum::Json(status) }
      }),
    )
    .route(
      "/dev/reset",
      axum::routing::post(move || {
        let store = reset_store.clone();
        async move {
          // Sessions, events, leases go; bootstrap ids and
          // deployments stay (dev-api.yaml).
          let pool = store.pool();
          let result: Result<(), sqlx::Error> = async {
            sqlx::query("DELETE FROM events").execute(pool).await?;
            sqlx::query("DELETE FROM leases").execute(pool).await?;
            sqlx::query("DELETE FROM sessions").execute(pool).await?;
            Ok(())
          }
          .await;
          match result {
            Ok(()) => axum::http::StatusCode::NO_CONTENT.into_response(),
            Err(e) => {
              (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
            }
          }
        }
      }),
    )
}

// ---- gateway port ---------------------------------------------

/// Which port to bind: a pin (strict — busy is an error) or a preference
/// (busy moves up, then falls back to an OS-assigned port).
#[derive(Debug, PartialEq)]
enum PortRequest {
  Strict {
    port: u16,
    source: &'static str,
  },
  Preferred {
    port: u16,
    /// From `dev.json` (this project's last port), not the default.
    remembered: bool,
  },
}

/// First match wins: `--port`, `[dev] port`, the remembered port, 8420.
fn port_request(args: &DevArgs, config: &ParsedConfig, previous: Option<&DevState>) -> PortRequest {
  if let Some(port) = args.port {
    return PortRequest::Strict {
      port,
      source: "--port",
    };
  }
  if let Some(port) = config.dev.as_ref().and_then(|d| d.port) {
    return PortRequest::Strict {
      port,
      source: "[dev] port in sectr.toml",
    };
  }
  match previous.and_then(|s| s.port) {
    Some(port) => PortRequest::Preferred {
      port,
      remembered: true,
    },
    None => PortRequest::Preferred {
      port: DEFAULT_GATEWAY_PORT,
      remembered: false,
    },
  }
}

/// Something already accepts on 127.0.0.1:`port`. Binding alone can't tell:
/// std sets SO_REUSEADDR, and on macOS that lets a 127.0.0.1 bind succeed
/// next to a `0.0.0.0` / `[::]` listener (next dev, Docker, …), silently
/// shadowing it.
fn port_in_use(port: u16) -> bool {
  let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
  std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok()
}

/// Bind the port if nothing else is serving it; `Ok(None)` = busy.
fn try_bind(port: u16) -> std::io::Result<Option<std::net::TcpListener>> {
  if port_in_use(port) {
    return Ok(None);
  }
  match std::net::TcpListener::bind(("127.0.0.1", port)) {
    Ok(listener) => Ok(Some(listener)),
    Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => Ok(None),
    Err(e) => Err(e),
  }
}

fn bind_gateway(req: &PortRequest) -> anyhow::Result<std::net::TcpListener> {
  match *req {
    PortRequest::Strict { port, source } => try_bind(port)
      .with_context(|| format!("binding 127.0.0.1:{port} ({source})"))?
      .with_context(|| {
        format!(
          "port {port} ({source}) is already in use. Find what's using it with \
           lsof -nP -iTCP:{port} -sTCP:LISTEN, or pick another port."
        )
      }),
    PortRequest::Preferred { port, .. } => {
      let last = port.saturating_add(PORT_SCAN - 1);
      for candidate in port..=last {
        // Errors other than "in use" (e.g. a privileged port) just skip.
        if let Ok(Some(listener)) = try_bind(candidate) {
          return Ok(listener);
        }
      }
      std::net::TcpListener::bind("127.0.0.1:0").context("binding the gateway")
    }
  }
}

/// Printed when a preferred port was busy and the gateway moved.
fn port_warning(req: &PortRequest, bound: u16) -> Option<String> {
  let PortRequest::Preferred { port, remembered } = *req else {
    return None;
  };
  if bound == port {
    return None;
  }
  let what = if remembered {
    format!("port {port} (this project's last port) is in use")
  } else {
    format!("port {port} is in use (another sectr dev?)")
  };
  Some(format!(
    "⚠ {what}; using {bound}. The local URL changed: update anything that saved \
     the old one (e.g. SECTR_AGUI_URL). Pin a port with [dev] port in sectr.toml."
  ))
}

/// The port in a gateway base URL (`https://local.sectr.dev:8420`).
fn url_port(url: &str) -> Option<u16> {
  url.rsplit_once(':')?.1.trim_end_matches('/').parse().ok()
}

/// Refuse to start while the previous run's gateway is still up: its pid
/// is alive AND something answers on its port. A live pid with nothing
/// listening is a stale file (kill -9, then pid reuse).
fn check_not_running(previous: Option<&DevState>) -> anyhow::Result<()> {
  let Some(DevState {
    gateway_pid: Some(pid),
    gateway_url: Some(url),
    ..
  }) = previous
  else {
    return Ok(());
  };
  if pid_alive(*pid) && url_port(url).is_some_and(port_in_use) {
    bail!(
      "already running for this project (pid {pid}, gateway {url}); \
       stop it before starting another"
    );
  }
  Ok(())
}

/// Shutdown: drop the live coordinates, keep the remembered port.
fn stopped(state: &mut DevState) {
  state.gateway_pid = None;
  state.gateway_url = None;
}

fn pid_alive(pid: u32) -> bool {
  #[cfg(unix)]
  unsafe {
    libc::kill(pid as libc::pid_t, 0) == 0
  }
  #[cfg(not(unix))]
  {
    let _ = pid;
    false
  }
}

// ---- config ------------------------------------------------------------

/// Parse + validate `sectr.toml`.
fn parse_sectr_toml(path: &Path) -> anyhow::Result<ParsedConfig> {
  let raw = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
  let parsed: SectrToml =
    toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
  if parsed.agent.entry.is_empty() {
    bail!("sectr.toml: [agent] entry is required");
  }
  if parsed.dev.as_ref().and_then(|d| d.port) == Some(0) {
    bail!("sectr.toml: [dev] port must be 1–65535");
  }
  Ok(ParsedConfig {
    agent: parsed.agent,
    app: parsed.app,
    dev: parsed.dev,
    approval_tools: parsed.deploy.map(|d| d.approval_tools).unwrap_or_default(),
  })
}

/// `[deploy] approval_tools` is the one way to set the approval policy, so
/// `SECTR_APPROVAL_POLICY` set by the user (`in_environment`, or in `.env`)
/// is an error rather than an override. A `.env` that doesn't parse is left
/// to the first turn, which reports it.
fn check_approval_policy_env(in_environment: bool, env_path: &Path) -> anyhow::Result<()> {
  if in_environment {
    bail!("{APPROVAL_POLICY_ENV} is set in the environment: {APPROVAL_POLICY_ENV_IS_RESERVED}");
  }
  let in_env_file = std::fs::read_to_string(env_path)
    .ok()
    .and_then(|content| EnvFileSecretSource::parse(env_path, &content).ok())
    .is_some_and(|pairs| pairs.iter().any(|(key, _)| key == APPROVAL_POLICY_ENV));
  if in_env_file {
    bail!(
      "{} sets {APPROVAL_POLICY_ENV}: {APPROVAL_POLICY_ENV_IS_RESERVED}",
      env_path.display()
    );
  }
  Ok(())
}

/// Run the SDK inspector in the project: `uv run python -m sectr.inspect`
/// (syncing the project's dependencies first, as `uv run` does) → manifest
/// JSON. Failure = the user's code doesn't import; stop with the end of
/// the inspector's stderr. Shared with `sectr init`'s post-scaffold smoke
/// check, and the same launcher the sidecar's serve mode uses.
pub(crate) async fn run_inspect(project: &Path, entry: &str) -> anyhow::Result<serde_json::Value> {
  let command = ["uv", "run", "python", "-m", "sectr.inspect"].map(String::from);
  // No timeout: the first `uv run` may sync dependencies for minutes.
  inspect::run_import(&command, project, entry, None, tokio::io::sink())
    .await
    .context("manifest inspection failed")
}

// ---- bootstrap ---------------------------------------------------------

/// Org/app/deployment bootstrap, idempotent across invocations: ids persist
/// in `.agent/dev.json`; a changed manifest creates the next deployment
/// version. The remembered port carries over; the live gateway coordinates
/// stay unset until the gateway binds. Returns the state (also written
/// back).
async fn bootstrap(
  store: &SqliteStore,
  agent_dir: &Path,
  manifest: &serde_json::Value,
  app_name: &str,
) -> anyhow::Result<DevState> {
  let state_path = agent_dir.join("dev.json");
  let registry = store.registry();
  let existing = read_state(&state_path)?;

  let (org_id, app_id) = match &existing {
    // Trust but verify: `.agent/` is disposable; the DB may have been
    // wiped while the state file survived.
    Some(state) if registry.get_app(state.app_id).await.is_ok() => (state.org_id, state.app_id),
    _ => create_org_app(&registry, app_name).await?,
  };

  // Deployment: reuse when the manifest is unchanged; else next version.
  let (deployment_id, version) = bootstrap_deployment(&registry, app_id, manifest).await?;

  let state = DevState {
    org_id,
    app_id,
    deployment_id,
    version,
    manifest: manifest.clone(),
    port: existing.as_ref().and_then(|s| s.port),
    gateway_pid: None,
    gateway_url: None,
  };
  write_state(&state_path, &state)?;
  Ok(state)
}

fn read_state(path: &Path) -> anyhow::Result<Option<DevState>> {
  if !path.exists() {
    return Ok(None);
  }
  Ok(Some(serde_json::from_str(&std::fs::read_to_string(path)?)?))
}

fn write_state(path: &Path, state: &DevState) -> anyhow::Result<()> {
  std::fs::write(path, serde_json::to_string_pretty(state)?)?;
  Ok(())
}

async fn create_org_app(
  registry: &SqliteRegistry,
  app_name: &str,
) -> anyhow::Result<(uuid::Uuid, uuid::Uuid)> {
  let org = registry.create_org("local", "local").await?;
  let app = registry
    .create_app(sectr_common::registry::NewApp {
      org_id: org.id,
      name: app_name.into(),
      slug: app_name.into(),
    })
    .await?;
  Ok((org.id, app.id))
}

async fn bootstrap_deployment(
  registry: &SqliteRegistry,
  app_id: uuid::Uuid,
  manifest: &serde_json::Value,
) -> anyhow::Result<(uuid::Uuid, i64)> {
  match registry.active_deployment(app_id).await? {
    Some(d) if d.manifest == *manifest => Ok((d.id, d.version)),
    existing => {
      let version = existing.map(|d| d.version).unwrap_or(0) + 1;
      let deployment = registry
        .create_deployment(sectr_common::registry::NewDeployment {
          endpoint_url: None,
          app_id,
          version,
          manifest: manifest.clone(),
        })
        .await?;
      registry
        .set_deployment_status(deployment.id, DeploymentStatus::Active)
        .await?;
      Ok((deployment.id, version))
    }
  }
}

/// The hosted dashboard, opened when nothing else names one.
const HOSTED_DASHBOARD: &str = "https://app.sectr.dev";

/// Which dashboard `sectr dev` opens: `--dashboard` or `SECTR_DASHBOARD_URL`,
/// then the saved login's, then the hosted one. Reads no network: `sectr dev`
/// needs neither a server nor a login.
fn choose_dashboard(explicit: Option<&str>, saved_login: Option<&str>) -> String {
  explicit
    .filter(|url| !url.trim().is_empty())
    .or(saved_login)
    .unwrap_or(HOSTED_DASHBOARD)
    .trim_end_matches('/')
    .to_owned()
}

/// Dev-gateway CORS: the console (deployed on the Workers domain, local
/// `next dev`, or the local.sectr.dev gateway itself) is cross-origin to
/// the gateway port — POST with a JSON content-type preflights, and
/// EventSource reconnections send `Last-Event-ID` cross-origin. Tightened
/// from `Any`: only console origins we know about (plus the --dashboard
/// override) may call in. DEV ONLY: the prod server is same-origin (the
/// dashboard's rewrite) and mounts `sessions_router` bare, so the layer
/// lives here in the CLI composition, never in the shared router. The dev
/// gateway has no credentials, which is why cookie-less origins suffice.
fn gateway_cors(dashboard_origin: Option<&str>) -> CorsLayer {
  let mut origins = vec![
    "https://app.sectr.dev".to_string(),
    "https://local.sectr.dev".to_string(),
    "http://local.sectr.dev".to_string(),
    "http://localhost:3000".to_string(),
    "http://127.0.0.1:3000".to_string(),
  ];
  if let Some(origin) = dashboard_origin
    && !origins.iter().any(|o| o == origin)
  {
    origins.push(origin.to_string());
  }
  let list: Vec<_> = origins
    .iter()
    .map(|o| o.parse::<axum::http::HeaderValue>().expect("origin header"))
    .collect();
  CorsLayer::new()
    .allow_origin(list)
    .allow_methods([Method::GET, Method::POST])
    .allow_headers([CONTENT_TYPE, HeaderName::from_static("last-event-id")])
}

/// The `Origin` of the configured dashboard base (scheme://host[:port]).
fn dashboard_origin(dashboard: &str) -> Option<&str> {
  let (scheme, rest) = dashboard.split_once("://")?;
  let host = rest.split('/').next()?;
  Some(&dashboard[..scheme.len() + 3 + host.len()])
}

/// `{dashboard}/apps/{app}/playground?route={route}&gateway={gateway_url}`
/// — landing straight in the playground (the thing you're developing), on
/// the app's first route. The query params are what the dashboard
/// understands: `gateway` is persisted by GatewayInit (localStorage) so
/// navigations keep the gateway without the param; `route` preselects the
/// playground's route picker. Route paths are `_ROUTE_RE`-simple
/// (`/customer-support`) — only `/` needs percent-encoding. The dashboard
/// base is an origin (no query of its own — the path must precede it).
fn dashboard_open_url(
  dashboard: &str,
  app_id: &str,
  route: Option<&str>,
  gateway_url: &str,
) -> String {
  let route_q = route
    .map(|r| format!("route={}&", r.replace('/', "%2F")))
    .unwrap_or_default();
  format!("{dashboard}/apps/{app_id}/playground?{route_q}gateway={gateway_url}")
}

/// TLS material for the `local.sectr.dev` trick: a publicly trusted cert
/// for a domain that resolves to 127.0.0.1 (DNS A/AAAA records), so the
/// deployed https console can fetch the local https gateway without mixed
/// content (Safari blocks http→localhost from https pages).
///
/// Distribution is DYNAMIC (decided 2026-09-11): the cert+key are fetched
/// from the hosted location (default `https://tls.sectr.dev`) at startup
/// (fetched fresh every start — the
/// files are a few KB) and cached in `$SECTR_TLS_CERT_DIR` or
/// `~/.sectr/certs/` — renewal (CI DNS-01, backlogged) converges to every
/// CLI on next start, no CLI update needed. SECURITY NOTE: the key
/// is effectively PUBLIC (anyone can download it); the scheme's security
/// rests on loopback DNS (traffic never leaves the machine, so the key
/// enables nothing off-box) + the victim-navigates assumption. Changing
/// backends: `SECTR_TLS_URL` overrides the hosted location (self-host/
/// enterprise); `SECTR_TLS_CERT_DIR` also FORCES local files and skips
/// the network (airgapped/testing). Plain-HTTP fallback when no cert is
/// available (Chrome/Firefox still work; Safari needs the cert).
async fn load_tls() -> Option<axum_server::tls_rustls::RustlsConfig> {
  // reqwest's rustls pulls in BOTH ring and aws-lc-rs, so rustls can't
  // auto-pick a provider — install ring explicitly.
  let _ = rustls::crypto::ring::default_provider().install_default();
  let forced_dir = std::env::var("SECTR_TLS_CERT_DIR")
    .map(std::path::PathBuf::from)
    .ok();
  let dir = forced_dir.clone().unwrap_or_else(default_cert_dir);
  let cert_path = dir.join(format!("{TLS_HOST}.crt"));
  let key_path = dir.join(format!("{TLS_HOST}.key"));

  if forced_dir.is_none() {
    refresh_tls_cache(&dir).await;
  }

  let (Ok(cert_pem), Ok(key_pem)) = (std::fs::read(&cert_path), std::fs::read(&key_path)) else {
    return None;
  };
  axum_server::tls_rustls::RustlsConfig::from_pem(cert_pem, key_pem)
    .await
    .map_err(|e| {
      eprintln!(
        "tls: bad cert/key in {} ({e}) — serving plain HTTP",
        dir.display()
      );
      e
    })
    .ok()
}

const TLS_HOST: &str = "local.sectr.dev";
const TLS_URL_DEFAULT: &str = "https://tls.sectr.dev";

fn default_cert_dir() -> std::path::PathBuf {
  let mut home = std::env::var_os("HOME")
    .or_else(|| std::env::var_os("USERPROFILE"))
    .map(std::path::PathBuf::from)
    .unwrap_or_default();
  home.push(".sectr/certs");
  home
}

/// Refresh the cached cert/key from the hosted location: fetch both files
/// (a few KB each) and rewrite the cache only on change. Every failure is
/// non-fatal — the cache (or plain HTTP) covers offline. Integrity comes
/// from the https channel; a same-origin hash file would add a request,
/// not security.
async fn refresh_tls_cache(dir: &std::path::Path) {
  let base = std::env::var("SECTR_TLS_URL").unwrap_or_else(|_| TLS_URL_DEFAULT.to_string());
  let client = match reqwest::Client::builder()
    .timeout(Duration::from_secs(2))
    .build()
  {
    Ok(c) => c,
    Err(_) => return,
  };

  let (Ok(cert), Ok(key)) = (
    client.get(format!("{base}/{TLS_HOST}.crt")).send().await,
    client.get(format!("{base}/{TLS_HOST}.key")).send().await,
  ) else {
    return; // offline / not deployed yet — cache or plain HTTP
  };
  let (Ok(cert), Ok(key)) = (cert.bytes().await, key.bytes().await) else {
    return;
  };
  let (cert, key) = (cert.to_vec(), key.to_vec());

  // Rewrite only on change (a byte-equal cache means the cert is current).
  let cert_path = dir.join(format!("{TLS_HOST}.crt"));
  let key_path = dir.join(format!("{TLS_HOST}.key"));
  let unchanged = std::fs::read(&cert_path).is_ok_and(|c| c == cert)
    && std::fs::read(&key_path).is_ok_and(|k| k == key);
  if unchanged {
    return;
  }
  if std::fs::create_dir_all(dir).is_err() {
    return;
  }
  let write = |path: &std::path::Path, bytes: &[u8]| -> bool {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).is_ok() && std::fs::rename(&tmp, path).is_ok()
  };
  if write(&cert_path, &cert) && write(&key_path, &key) {
    println!("tls: refreshed the {TLS_HOST} certificate from {base}");
  }
}

// ---- tests -------------------------------------------------------------

#[cfg(test)]
mod tests {
  use super::*;
  use axum::http::StatusCode;
  use axum::routing::get;
  use tower::ServiceExt as _;

  /// Cross-origin access the dashboard depends on: a JSON POST preflights
  /// (OPTIONS) with the method/header list, including Last-Event-ID for
  /// EventSource reconnections.
  #[tokio::test]
  async fn dev_gateway_preflights_post() {
    let app = axum::Router::new()
      .route("/ping", get(|| async { "ok" }))
      .layer(gateway_cors(Some("https://app.sectr.dev")));

    // Preflight for POST /sessions (application/json is not safelisted).
    let res = app
      .clone()
      .oneshot(
        axum::http::Request::builder()
          .method("OPTIONS")
          .uri("/sessions")
          .header("origin", "https://app.sectr.dev")
          .header("access-control-request-method", "POST")
          .header("access-control-request-headers", "content-type")
          .body(axum::body::Body::empty())
          .unwrap(),
      )
      .await
      .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
      res.headers().get("access-control-allow-origin").unwrap(),
      "https://app.sectr.dev"
    );
    assert_eq!(
      res.headers().get("access-control-allow-methods").unwrap(),
      "GET,POST"
    );
    assert_eq!(
      res.headers().get("access-control-allow-headers").unwrap(),
      "content-type,last-event-id"
    );

    // Simple GET carries the header on the actual response.
    let res = app
      .oneshot(
        axum::http::Request::builder()
          .uri("/ping")
          .header("origin", "https://app.sectr.dev")
          .body(axum::body::Body::empty())
          .unwrap(),
      )
      .await
      .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
      res.headers().get("access-control-allow-origin").unwrap(),
      "https://app.sectr.dev"
    );
  }

  #[test]
  fn dashboard_url_lands_on_the_playground() {
    assert_eq!(
      dashboard_open_url(
        "https://app.sectr.dev",
        "0196b8f0-0000-7000-8000-000000000000",
        Some("/chat"),
        "https://local.sectr.dev:47331"
      ),
      "https://app.sectr.dev/apps/0196b8f0-0000-7000-8000-000000000000/playground?route=%2Fchat&gateway=https://local.sectr.dev:47331"
    );
    // a dashboard base carries no query (it's an origin — the path
    // must precede any query)
    assert_eq!(
      dashboard_open_url(
        "http://localhost:3000",
        "0196b8f0-0000-7000-8000-000000000000",
        Some("/customer-support"),
        "http://127.0.0.1:8080"
      ),
      "http://localhost:3000/apps/0196b8f0-0000-7000-8000-000000000000/playground?route=%2Fcustomer-support&gateway=http://127.0.0.1:8080"
    );
    // no routes in the manifest — still lands on the playground
    assert_eq!(
      dashboard_open_url(
        "https://app.sectr.dev",
        "0196b8f0",
        None,
        "http://127.0.0.1:8080"
      ),
      "https://app.sectr.dev/apps/0196b8f0/playground?gateway=http://127.0.0.1:8080"
    );
  }

  #[test]
  fn sectr_dev_opens_the_explicit_then_the_saved_then_the_hosted_dashboard() {
    let explicit = Some("http://localhost:4000/");
    let saved = Some("https://sectr.example.com");
    assert_eq!(choose_dashboard(explicit, saved), "http://localhost:4000");
    assert_eq!(choose_dashboard(None, saved), "https://sectr.example.com");
    assert_eq!(choose_dashboard(None, None), "https://app.sectr.dev");
    assert_eq!(
      choose_dashboard(Some(""), saved),
      "https://sectr.example.com",
      "an empty env value counts as unset"
    );
    assert_eq!(
      dashboard_origin(&choose_dashboard(None, saved)),
      Some("https://sectr.example.com"),
      "the chosen dashboard's origin is what CORS allows"
    );
  }

  #[test]
  fn dashboard_origin_is_scheme_and_host() {
    assert_eq!(
      dashboard_origin("https://app.sectr.dev"),
      Some("https://app.sectr.dev")
    );
    assert_eq!(
      dashboard_origin("http://localhost:3000/console"),
      Some("http://localhost:3000")
    );
    assert_eq!(dashboard_origin("not-a-url"), None);
  }

  /// The tightened origin list: known console origins allowed, unknown
  /// ones not, --dashboard override appended.
  #[tokio::test]
  async fn dev_gateway_allows_known_origins_only() {
    let app = axum::Router::new()
      .route("/ping", get(|| async { "ok" }))
      .layer(gateway_cors(Some("http://localhost:5173")));

    let check = |origin: &'static str| {
      let app = app.clone();
      async move {
        app
          .oneshot(
            axum::http::Request::builder()
              .uri("/ping")
              .header("origin", origin)
              .body(axum::body::Body::empty())
              .unwrap(),
          )
          .await
          .unwrap()
          .headers()
          .get("access-control-allow-origin")
          .map(|v| v.to_str().unwrap().to_string())
      }
    };

    assert_eq!(
      check("https://app.sectr.dev").await.as_deref(),
      Some("https://app.sectr.dev")
    );
    assert_eq!(
      check("http://localhost:5173").await.as_deref(),
      Some("http://localhost:5173")
    );
    assert_eq!(check("https://evil.example").await, None);
  }

  #[test]
  fn parses_sectr_toml() {
    let dir = std::env::temp_dir().join(format!("sectr-toml-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("sectr.toml");
    std::fs::write(
      &path,
      "[app]\nname = \"support-agent\"\n\n[agent]\nentry = \"main:app\"\npython = \"3.12\"\n",
    )
    .unwrap();
    let parsed = parse_sectr_toml(&path).unwrap();
    assert_eq!(parsed.agent.entry, "main:app");
    assert_eq!(parsed.app.unwrap().name.unwrap(), "support-agent");
    assert!(parsed.dev.is_none());
    assert!(parsed.approval_tools.is_empty());
  }

  #[test]
  fn reads_the_approval_policy_from_deploy() {
    let dir = std::env::temp_dir().join(format!("sectr-toml-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("sectr.toml");
    std::fs::write(
      &path,
      "[agent]\nentry = \"main:app\"\n\n[deploy]\napproval_tools = [\"refund\", \"delete_account\"]\n",
    )
    .unwrap();
    let parsed = parse_sectr_toml(&path).unwrap();
    assert_eq!(parsed.approval_tools, ["refund", "delete_account"]);
  }

  #[test]
  fn a_user_set_approval_policy_env_is_rejected() {
    let dir = std::env::temp_dir().join(format!("sectr-env-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    let env_path = dir.join(".env");

    // No .env, nothing in the environment: fine.
    check_approval_policy_env(false, &env_path).unwrap();

    let err = check_approval_policy_env(true, &env_path)
      .unwrap_err()
      .to_string();
    assert!(err.contains("set in the environment"), "{err}");
    assert!(err.contains("[deploy] approval_tools"), "{err}");

    std::fs::write(&env_path, "OPENAI_API_KEY=sk\n").unwrap();
    check_approval_policy_env(false, &env_path).unwrap();

    std::fs::write(
      &env_path,
      "OPENAI_API_KEY=sk\nSECTR_APPROVAL_POLICY='[\"refund\"]'\n",
    )
    .unwrap();
    let err = check_approval_policy_env(false, &env_path)
      .unwrap_err()
      .to_string();
    assert!(err.contains(".env sets SECTR_APPROVAL_POLICY"), "{err}");

    // A .env that doesn't parse is left to the first turn to report.
    std::fs::write(&env_path, "not a pair\n").unwrap();
    check_approval_policy_env(false, &env_path).unwrap();
  }

  #[test]
  fn rejects_missing_entry() {
    let dir = std::env::temp_dir().join(format!("sectr-toml-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("sectr.toml");
    std::fs::write(&path, "[agent]\npython = \"3.12\"\n").unwrap();
    assert!(parse_sectr_toml(&path).is_err());
  }

  fn manifest(tag: &str) -> serde_json::Value {
    serde_json::json!({
        "name": "a",
        "entry": "main:app",
        "python": "3.12",
        "routes": [{"path": "/test"}],
        "env": [],
        "tag": tag,
    })
  }

  async fn temp_store() -> (SqliteStore, PathBuf) {
    let dir = std::env::temp_dir().join(format!("sectr-bootstrap-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = SqliteStore::open(&dir.join("dev.db").to_string_lossy())
      .await
      .unwrap();
    (store, dir)
  }

  #[tokio::test]
  async fn bootstrap_is_idempotent_and_bumps_versions() {
    let (store, agent_dir) = temp_store().await;

    let first = bootstrap(&store, &agent_dir, &manifest("v1"), "a")
      .await
      .unwrap();
    let second = bootstrap(&store, &agent_dir, &manifest("v1"), "a")
      .await
      .unwrap();
    assert_eq!(
      first.deployment_id, second.deployment_id,
      "same manifest reuses the deployment"
    );
    assert_eq!(second.version, 1);

    let third = bootstrap(&store, &agent_dir, &manifest("v2"), "a")
      .await
      .unwrap();
    assert_ne!(
      third.deployment_id, first.deployment_id,
      "a changed manifest bumps the version"
    );
    assert_eq!(third.version, 2);

    // State file round-trips.
    let persisted: DevState =
      serde_json::from_str(&std::fs::read_to_string(agent_dir.join("dev.json")).unwrap()).unwrap();
    assert_eq!(persisted.app_id, third.app_id);
    assert_eq!(persisted.gateway_url, None, "no gateway running");
  }

  #[tokio::test]
  async fn bootstrap_keeps_the_remembered_port() {
    let (store, agent_dir) = temp_store().await;
    let mut first = bootstrap(&store, &agent_dir, &manifest("v1"), "a")
      .await
      .unwrap();
    first.port = Some(8425);
    first.gateway_pid = Some(std::process::id());
    first.gateway_url = Some("https://local.sectr.dev:8425".into());
    write_state(&agent_dir.join("dev.json"), &first).unwrap();

    let second = bootstrap(&store, &agent_dir, &manifest("v1"), "a")
      .await
      .unwrap();
    let persisted = read_state(&agent_dir.join("dev.json")).unwrap().unwrap();
    for state in [&second, &persisted] {
      assert_eq!(state.port, Some(8425));
      assert_eq!(state.gateway_pid, None);
      assert_eq!(state.gateway_url, None);
    }
  }

  #[test]
  fn state_file_tolerates_pre_gateway_contents() {
    let old: DevState = serde_json::from_str(
      r#"{
                "org_id": "0192f3a4-0000-7000-8000-000000000001",
                "app_id": "0192f3a4-0000-7000-8000-000000000002",
                "deployment_id": "0192f3a4-0000-7000-8000-000000000003",
                "version": 1,
                "manifest": {}
            }"#,
    )
    .unwrap();
    assert_eq!(old.port, None);
    assert_eq!(old.gateway_pid, None);
  }

  /// An old dev.json (`gateway_port`) still parses; its
  /// port isn't carried over.
  #[test]
  fn state_file_ignores_the_old_gateway_port() {
    let old: DevState = serde_json::from_str(
      r#"{
                "org_id": "0192f3a4-0000-7000-8000-000000000001",
                "app_id": "0192f3a4-0000-7000-8000-000000000002",
                "deployment_id": "0192f3a4-0000-7000-8000-000000000003",
                "version": 1,
                "manifest": {},
                "gateway_port": 52817
            }"#,
    )
    .unwrap();
    assert_eq!(old.port, None);
    assert_eq!(old.gateway_url, None);
  }

  // ---- gateway port -------------------------------------------

  fn state(port: Option<u16>, pid: Option<u32>, url: Option<String>) -> DevState {
    DevState {
      org_id: uuid::Uuid::nil(),
      app_id: uuid::Uuid::nil(),
      deployment_id: uuid::Uuid::nil(),
      version: 1,
      manifest: serde_json::json!({}),
      port,
      gateway_pid: pid,
      gateway_url: url,
    }
  }

  fn config(toml_port: Option<u16>) -> ParsedConfig {
    ParsedConfig {
      agent: AgentSection {
        entry: "main:app".into(),
        python: None,
      },
      app: None,
      dev: Some(DevSection {
        command: None,
        env_file: None,
        port: toml_port,
      }),
      approval_tools: vec![],
    }
  }

  fn args(port: Option<u16>) -> DevArgs {
    DevArgs {
      path: ".".into(),
      config: "sectr.toml".into(),
      no_open: true,
      dashboard: Some("https://app.sectr.dev".into()),
      port,
    }
  }

  /// A port nothing listens on right now (the OS picked it for us), so
  /// parallel tests don't collide.
  fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
      .unwrap()
      .local_addr()
      .unwrap()
      .port()
  }

  /// Hold a listener on `addr`, returning it and its port.
  fn hold(addr: &str) -> (std::net::TcpListener, u16) {
    let listener = std::net::TcpListener::bind(addr).unwrap();
    let port = listener.local_addr().unwrap().port();
    (listener, port)
  }

  #[test]
  fn port_request_resolution_order() {
    let remembered = state(Some(8425), None, None);
    assert_eq!(
      port_request(&args(Some(9001)), &config(Some(9002)), Some(&remembered)),
      PortRequest::Strict {
        port: 9001,
        source: "--port"
      }
    );
    assert_eq!(
      port_request(&args(None), &config(Some(9002)), Some(&remembered)),
      PortRequest::Strict {
        port: 9002,
        source: "[dev] port in sectr.toml"
      }
    );
    assert_eq!(
      port_request(&args(None), &config(None), Some(&remembered)),
      PortRequest::Preferred {
        port: 8425,
        remembered: true
      }
    );
    assert_eq!(
      port_request(&args(None), &config(None), None),
      PortRequest::Preferred {
        port: DEFAULT_GATEWAY_PORT,
        remembered: false
      }
    );
  }

  #[test]
  fn preferred_port_moves_up_when_busy() {
    let (_held, port) = hold("127.0.0.1:0");
    let req = PortRequest::Preferred {
      port,
      remembered: true,
    };
    let bound = bind_gateway(&req).unwrap().local_addr().unwrap().port();
    assert_ne!(bound, port);
    assert!(
      port_warning(&req, bound)
        .unwrap()
        .contains("this project's last port")
    );
  }

  /// The macOS case: a 127.0.0.1 bind would succeed next to a wildcard
  /// listener (SO_REUSEADDR) — the connect probe must skip it.
  #[test]
  fn preferred_port_skips_a_wildcard_listener() {
    for addr in ["0.0.0.0:0", "[::]:0"] {
      let Ok(held) = std::net::TcpListener::bind(addr) else {
        continue; // no IPv6 on this host
      };
      let port = held.local_addr().unwrap().port();
      if std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        continue; // a v6-only [::] socket doesn't take 127.0.0.1 traffic
      }
      let req = PortRequest::Preferred {
        port,
        remembered: false,
      };
      let bound = bind_gateway(&req).unwrap().local_addr().unwrap().port();
      assert_ne!(bound, port, "{addr} listener shadowed");
    }
  }

  #[test]
  fn preferred_port_binds_exactly_when_free() {
    let port = free_port();
    let req = PortRequest::Preferred {
      port,
      remembered: false,
    };
    let bound = bind_gateway(&req).unwrap().local_addr().unwrap().port();
    assert_eq!(bound, port);
    assert_eq!(port_warning(&req, bound), None);
  }

  #[test]
  fn strict_port_errors_when_busy() {
    let (_held, port) = hold("127.0.0.1:0");
    let err = bind_gateway(&PortRequest::Strict {
      port,
      source: "--port",
    })
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
      msg.contains(&format!("port {port} (--port) is already in use")),
      "{msg}"
    );
    assert!(msg.contains("lsof"), "{msg}");
  }

  #[test]
  fn strict_port_binds_exactly_when_free() {
    let port = free_port();
    let listener = bind_gateway(&PortRequest::Strict {
      port,
      source: "[dev] port in sectr.toml",
    })
    .unwrap();
    assert_eq!(listener.local_addr().unwrap().port(), port);
  }

  #[test]
  fn default_port_warning() {
    let req = PortRequest::Preferred {
      port: DEFAULT_GATEWAY_PORT,
      remembered: false,
    };
    let warning = port_warning(&req, 8421).unwrap();
    assert!(warning.starts_with("⚠ port 8420 is in use (another sectr dev?); using 8421."));
    assert_eq!(
      port_warning(
        &PortRequest::Strict {
          port: 9001,
          source: "--port"
        },
        9001
      ),
      None
    );
  }

  #[test]
  fn stopped_keeps_the_port() {
    let mut s = state(
      Some(8420),
      Some(1),
      Some("https://local.sectr.dev:8420".into()),
    );
    stopped(&mut s);
    assert_eq!(s.port, Some(8420));
    assert_eq!(s.gateway_pid, None);
    assert_eq!(s.gateway_url, None);
  }

  #[test]
  fn guard_refuses_a_live_gateway() {
    let (_held, port) = hold("127.0.0.1:0");
    let live = state(
      Some(port),
      Some(std::process::id()),
      Some(format!("http://127.0.0.1:{port}")),
    );
    let err = check_not_running(Some(&live)).unwrap_err().to_string();
    assert!(err.contains("already running"), "{err}");
  }

  #[test]
  fn guard_allows_stale_or_absent_state() {
    let port = free_port();
    let url = Some(format!("https://local.sectr.dev:{port}"));
    // own (live) pid, nothing listening: stale
    check_not_running(Some(&state(
      Some(port),
      Some(std::process::id()),
      url.clone(),
    )))
    .unwrap();
    // dead pid (beyond any pid_max)
    check_not_running(Some(&state(Some(port), Some(i32::MAX as u32), url))).unwrap();
    check_not_running(Some(&state(Some(port), None, None))).unwrap();
    check_not_running(None).unwrap();
  }

  #[test]
  fn port_zero_is_rejected() {
    use clap::Parser;
    #[derive(Parser)]
    struct Cli {
      #[command(flatten)]
      dev: DevArgs,
    }
    assert!(Cli::try_parse_from(["sectr", "--port", "0"]).is_err());
    assert_eq!(
      Cli::try_parse_from(["sectr", "--port", "8500"])
        .unwrap()
        .dev
        .port,
      Some(8500)
    );

    let dir = std::env::temp_dir().join(format!("sectr-toml-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("sectr.toml");
    std::fs::write(&path, "[agent]\nentry = \"main:app\"\n\n[dev]\nport = 0\n").unwrap();
    let err = parse_sectr_toml(&path).err().unwrap().to_string();
    assert!(err.contains("[dev] port"), "{err}");
    std::fs::write(
      &path,
      "[agent]\nentry = \"main:app\"\n\n[dev]\nport = 8500\n",
    )
    .unwrap();
    assert_eq!(
      parse_sectr_toml(&path).unwrap().dev.unwrap().port,
      Some(8500)
    );
  }
}
