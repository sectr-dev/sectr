//! `sectr deploy`: package the project → upload → (server builds)
//! → report. The thin half of the deploy flow: the CLI's only authority is
//! the platform key; everything else (build, manifest extraction,
//! activation) happens server-side.
//!
//! Packaging: a gzipped tar of the project directory, skipping VCS/build
//! noise (`.git`, `.agent`, `.venv`, `__pycache__`, `node_modules`,
//! `target`, `*.pyc`). Deterministic enough for repeat deploys; NOT
//! reproducible-build territory.
//!
//! Auth: `--key` or `SECTR_PLATFORM_KEY` (the `sectr auth` will own
//! key storage; this reads what's given).
//!
//! The build runs async server-side: the upload returns 202 with
//! a `building` row and resolves in the background, so this polls
//! `GET /apps/{id}/deployments` every ~2s until the row reports
//! `active` or `failed` (`--timeout` bounds the wait, default 10min).
//! Cloudflare's ~100s proxy limit no longer applies — nothing but the
//! upload rides a single request.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;

use clap::Parser;

#[derive(Parser, Debug)]
pub struct DeployArgs {
  /// Project directory (must contain sectr.toml).
  #[arg(long, default_value = ".")]
  pub path: PathBuf,
  /// sectr.toml location relative to the project.
  #[arg(long, default_value = "sectr.toml")]
  pub config: String,
  /// The server base URL (`http(s)://host[:port]`). Defaults through
  /// `sectr auth login` (or SECTR_API_URL).
  #[arg(long)]
  pub api: Option<String>,
  /// Platform API key (`skp_…`). Defaults through `sectr auth login`
  /// (or SECTR_PLATFORM_KEY).
  #[arg(long)]
  pub key: Option<String>,
  /// How long to wait for the server-side build to resolve, in seconds.
  #[arg(long, default_value_t = 600)]
  pub timeout_secs: u64,
}

/// Paths that never belong in a deploy tarball.
const SKIP_DIRS: &[&str] = &[
  ".git",
  ".agent",
  ".venv",
  "venv",
  "__pycache__",
  "node_modules",
  "target",
  ".mypy_cache",
  ".pytest_cache",
  ".ruff_cache",
];

/// Per-request cap on the upload POST: generous headroom for a large
/// tarball on a slow link, but a hung connection must eventually error
/// instead of blocking forever.
const UPLOAD_REQUEST_TIMEOUT: Duration = Duration::from_secs(600);
/// Per-request cap on each poll GET: a hung connection must not outlive
/// the overall `--timeout`; the poll loop retries transport errors anyway.
const POLL_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

fn skip_path(relative: &Path) -> bool {
  for component in relative.components() {
    let Some(name) = component.as_os_str().to_str() else {
      return true; // non-UTF8 path component — skip rather than guess
    };
    if SKIP_DIRS.contains(&name) {
      return true;
    }
    if name.ends_with(".pyc") {
      return true;
    }
  }
  false
}

/// Gzip-tar the project directory (files only — directories are implicit).
fn package_project(project: &Path) -> anyhow::Result<Vec<u8>> {
  let mut tar = tar::Builder::new(Vec::new());
  let mut stack = vec![project.to_path_buf()];
  while let Some(dir) = stack.pop() {
    let entries =
      std::fs::read_dir(&dir).map_err(|e| anyhow::anyhow!("read {}: {e}", dir.display()))?;
    for entry in entries {
      let entry = entry.map_err(|e| anyhow::anyhow!("readdir: {e}"))?;
      let path = entry.path();
      let relative = path
        .strip_prefix(project)
        .map_err(|e| anyhow::anyhow!("path error: {e}"))?;
      if skip_path(relative) {
        continue;
      }
      let file_type = entry
        .file_type()
        .map_err(|e| anyhow::anyhow!("filetype: {e}"))?;
      if file_type.is_dir() {
        stack.push(path);
      } else if file_type.is_symlink() {
        // Symlinks are skipped: a source tree with a symlink is
        // either tooling noise or a packaging smell.
        continue;
      } else {
        // Append raw bytes with explicit metadata rather than
        // append_path_with_name: the latter lseeks (SEEK_HOLE) to
        // size the file, which some filesystems reject (virtiofs
        // returns ENXIO — found on the sandbox's repo share).
        let bytes =
          std::fs::read(&path).map_err(|e| anyhow::anyhow!("read {}: {e}", relative.display()))?;
        let meta = entry.metadata().map_err(|e| anyhow::anyhow!("stat: {e}"))?;
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(
          meta
            .modified()
            .ok()
            .and_then(|m| {
              m.duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_secs())
            })
            .unwrap_or(0),
        );
        header.set_cksum();
        tar
          .append_data(&mut header, relative, bytes.as_slice())
          .map_err(|e| anyhow::anyhow!("tar {}: {e}", relative.display()))?;
      }
    }
  }
  let tarball = tar.into_inner().map_err(|e| anyhow::anyhow!("tar: {e}"))?;
  let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
  std::io::Write::write_all(&mut gz, &tarball).map_err(|e| anyhow::anyhow!("gzip: {e}"))?;
  gz.finish().map_err(|e| anyhow::anyhow!("gzip: {e}"))
}

pub async fn run_deploy(args: DeployArgs) -> anyhow::Result<()> {
  let project = args.path.clone();
  let config_path = project.join(&args.config);
  let manifest_toml = std::fs::read_to_string(&config_path).map_err(|e| {
    anyhow::anyhow!(
      "read {}: {e} (sectr deploy needs a sectr.toml — see `sectr dev` for the shape)",
      config_path.display()
    )
  })?;

  let target = crate::auth::resolve(args.api.as_deref(), args.key.as_deref())?;
  let key = target.key;

  eprintln!("packaging {} …", project.display());
  let tarball = package_project(&project)?;

  let url = format!("{}/deployments", target.api);
  eprintln!("uploading to {url} …");

  let part_tarball = reqwest::multipart::Part::bytes(tarball)
    .file_name("app.tar.gz")
    .mime_str("application/gzip")
    .map_err(|e| anyhow::anyhow!("mime: {e}"))?;
  let part_manifest = reqwest::multipart::Part::text(manifest_toml);
  let form = reqwest::multipart::Form::new()
    .part("tarball", part_tarball)
    .part("manifest", part_manifest);

  let client = reqwest::Client::builder()
    .timeout(UPLOAD_REQUEST_TIMEOUT)
    .build()
    .context("build upload HTTP client")?;
  let response = client
    .post(url)
    .bearer_auth(&key)
    .multipart(form)
    .send()
    .await?;

  let status = response.status();
  let body: serde_json::Value = response.json().await.unwrap_or(serde_json::Value::Null);
  if !status.is_success() {
    anyhow::bail!(
      "deploy failed ({status}): {}",
      serde_json::to_string_pretty(&body)?
    );
  }

  // 202 + {id, app_id, version, status: "building"}: the build resolves
  // in the background — poll the list endpoint until it lands.
  let deployment_id = body
    .get("id")
    .and_then(|v| v.as_str())
    .context("server returned no deployment id — cannot track the build")?
    .to_owned();
  let app_id = body
    .get("app_id")
    .and_then(|v| v.as_str())
    .context("server returned no app_id — cannot track the build")?
    .to_owned();
  let version = body.get("version").and_then(|v| v.as_i64()).unwrap_or(0);

  eprintln!("deployment {deployment_id}: version {version} — building…");

  let list_url = format!("{}/apps/{app_id}/deployments", target.api);
  let client = reqwest::Client::builder()
    .timeout(POLL_REQUEST_TIMEOUT)
    .build()
    .context("build poll HTTP client")?;
  let fetch = || async {
    let resp = client
      .get(&list_url)
      .bearer_auth(&key)
      .send()
      .await
      .map_err(|e| anyhow::anyhow!("poll request failed: {e}"))?;
    let status = resp.status();
    let list: serde_json::Value = resp
      .json()
      .await
      .map_err(|e| anyhow::anyhow!("poll response parse failed ({status}): {e}"))?;
    if !status.is_success() {
      anyhow::bail!(
        "poll returned {status}: {}",
        serde_json::to_string_pretty(&list)?
      );
    }
    Ok(
      list
        .as_array()
        .and_then(|entries| {
          entries
            .iter()
            .find(|d| d.get("id").and_then(|v| v.as_str()) == Some(deployment_id.as_str()))
        })
        .map(|d| PollState {
          status: d
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned(),
          // Absent unless the build failed (the server skips the
          // field when there is no error).
          error: d.get("error").and_then(|v| v.as_str()).map(str::to_owned),
        }),
    )
  };

  match poll_until_resolved(
    fetch,
    Duration::from_secs(2),
    Duration::from_secs(args.timeout_secs),
  )
  .await?
  {
    Resolved::Active => {
      eprintln!("✓ deployed: version {version} ({deployment_id}) is active");
      // Best effort too: the deploy already succeeded.
      match view_live_link(&target.api, &key, &app_id).await {
        Ok(link) => eprintln!("🚀 view live at: {link}"),
        Err(e) => eprintln!("(could not look up the dashboard link: {e:#})"),
      }
      // Every deployed route is also an AG-UI endpoint (CopilotKit / any
      // AG-UI client): print the full URL so the frontend needs no config.
      // Best effort — the deploy already succeeded.
      match fetch_agui_endpoints(&client, &target.api, &key, &app_id).await {
        Ok(endpoints) => {
          for endpoint in endpoints {
            eprintln!("🔌 connect via AG-UI at: {endpoint}");
          }
        }
        Err(e) => eprintln!("(could not look up the AG-UI endpoints: {e})"),
      }
      Ok(())
    }
    Resolved::Failed(error) => anyhow::bail!(
      "deployment {deployment_id}: build failed — {}",
      failure_detail(&error)
    ),
  }
}

/// The app's page on the server's own dashboard, as `GET /me` reports it.
async fn view_live_link(api: &str, key: &str, app_id: &str) -> anyhow::Result<String> {
  let dashboard = crate::auth::whoami(api, key).await?.dashboard_url;
  Ok(format!("{dashboard}/apps/{app_id}"))
}

/// `GET /apps/{id}` → the AG-UI endpoint URL of each route of the active
/// deployment (`<api>/ag-ui/<app slug>/<route without its leading slash>`).
async fn fetch_agui_endpoints(
  client: &reqwest::Client,
  api: &str,
  key: &str,
  app_id: &str,
) -> anyhow::Result<Vec<String>> {
  let resp = client
    .get(format!("{api}/apps/{app_id}"))
    .bearer_auth(key)
    .send()
    .await?;
  let status = resp.status();
  let app: serde_json::Value = resp.json().await?;
  anyhow::ensure!(status.is_success(), "GET /apps/{app_id} returned {status}");
  Ok(agui_endpoints(api, &app))
}

/// Pure projection of an `AppDetail` body to AG-UI endpoint URLs.
pub(crate) fn agui_endpoints(api: &str, app: &serde_json::Value) -> Vec<String> {
  let Some(slug) = app.get("slug").and_then(|s| s.as_str()) else {
    return Vec::new();
  };
  let api = api.trim_end_matches('/');
  app
    .get("routes")
    .and_then(|r| r.as_array())
    .into_iter()
    .flatten()
    .filter_map(|r| r.get("path").and_then(|p| p.as_str()))
    .filter(|path| !path.trim_start_matches('/').is_empty())
    .map(|path| format!("{api}/ag-ui/{slug}/{}", path.trim_start_matches('/')))
    .collect()
}

/// The `error:` line shown when a build fails. The row's error carries the
/// tail of the build output (set by the server); when it is absent
/// (pre-error-surfacing servers, or a status without a stored reason) the
/// fallback says where to look instead.
fn failure_detail(error: &Option<String>) -> String {
  match error {
    Some(e) => format!("error: {e}"),
    None => "no error detail available — check the server logs for details \
             (build logs are not exposed by the API yet)"
      .to_owned(),
  }
}

/// One observation of the tracked deployment from the polling endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PollState {
  pub status: String,
  /// The row's stored failure reason — present only once the build
  /// failed.
  pub error: Option<String>,
}

/// Terminal deployment states as seen through the polling endpoint.
/// `Failed` carries the row's error field when the server stored one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Resolved {
  Active,
  Failed(Option<String>),
}

/// Poll `fetch` every `poll_every` until the deployment reports a terminal
/// status (`active` / `failed`), or `timeout` elapses.
///
/// `fetch` returns the deployment's current observation — `None` when the
/// row isn't listed (yet). Errors from `fetch` (transport blips, 5xx) are
/// retryable: the loop keeps polling until the deadline, and the last
/// error is surfaced in the timeout message. Unrecognized status strings
/// also keep polling (forward compatibility; the timeout is the backstop).
/// Printing belongs to the caller — the only transitions are
/// building → terminal, so "building…" then the final state is all there
/// is to show.
pub(crate) async fn poll_until_resolved<F>(
  mut fetch: F,
  poll_every: Duration,
  timeout: Duration,
) -> anyhow::Result<Resolved>
where
  F: AsyncFnMut() -> anyhow::Result<Option<PollState>>,
{
  let deadline = tokio::time::Instant::now() + timeout;
  /// What the last poll observed — the timeout error reports it so a
  /// stuck deploy is diagnosable from the CLI output alone.
  enum Last {
    Status(Option<String>),
    Error(anyhow::Error),
  }
  loop {
    // The timeout bail reads the CURRENT iteration's observation (the
    // bail fires right after this fetch), so nothing carries across
    // iterations.
    let last = match fetch().await {
      Ok(Some(state)) if state.status == "active" => return Ok(Resolved::Active),
      Ok(Some(state)) if state.status == "failed" => {
        return Ok(Resolved::Failed(state.error));
      }
      Ok(Some(state)) => Last::Status(Some(state.status)),
      Ok(None) => Last::Status(None),
      Err(e) => Last::Error(e),
    };
    if tokio::time::Instant::now() >= deadline {
      let detail = match &last {
        Last::Status(Some(status)) => format!("last status: {status}"),
        Last::Status(None) => "row not yet listed".to_owned(),
        Last::Error(e) => format!("last error: {e:#}"),
      };
      anyhow::bail!(
        "deployment did not resolve within {timeout:?} ({detail}) — \
                 it may still finish; check GET /apps/<app>/deployments or the dashboard"
      );
    }
    tokio::time::sleep(poll_every).await;
  }
}

#[cfg(test)]
mod tests {
  /// A stub `GET /me` answering with this status and body.
  async fn me_stub(status: u16, body: serde_json::Value) -> String {
    use axum::http::StatusCode;
    let app = axum::Router::new().route(
      "/me",
      axum::routing::get(move || {
        let body = body.clone();
        async move { (StatusCode::from_u16(status).unwrap(), axum::Json(body)) }
      }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
  }

  #[tokio::test]
  async fn view_live_links_to_the_servers_own_dashboard() {
    let api = me_stub(
      200,
      serde_json::json!({
        "org": {"id": "o", "name": "acme"},
        "dashboard_url": "https://sectr.example.com/",
      }),
    )
    .await;
    assert_eq!(
      view_live_link(&api, "skp_x", "app-1").await.unwrap(),
      "https://sectr.example.com/apps/app-1"
    );
  }

  #[tokio::test]
  async fn view_live_reports_why_it_has_no_link() {
    let failing = me_stub(500, serde_json::json!({})).await;
    let err = view_live_link(&failing, "skp_x", "app-1")
      .await
      .unwrap_err();
    assert!(err.to_string().contains("500"), "{err}");
    assert!(
      view_live_link("http://127.0.0.1:1", "skp_x", "app-1")
        .await
        .is_err()
    );
  }

  use super::*;

  use std::collections::VecDeque;

  /// Shorthand: a poll observation with just a status.
  fn state(status: &str) -> Option<PollState> {
    Some(PollState {
      status: status.to_owned(),
      error: None,
    })
  }

  #[test]
  fn agui_endpoints_come_from_the_active_routes() {
    let app = serde_json::json!({
      "slug": "pm-agent",
      "routes": [{"path": "/chat", "description": ""}, {"path": "/plan/weekly"}, {"path": ""}],
    });
    assert_eq!(
      agui_endpoints("https://api.sectr.dev/", &app),
      [
        "https://api.sectr.dev/ag-ui/pm-agent/chat",
        "https://api.sectr.dev/ag-ui/pm-agent/plan/weekly"
      ]
    );
    // No slug / no routes → nothing to print (never a malformed URL).
    assert!(
      agui_endpoints(
        "https://x",
        &serde_json::json!({"routes": [{"path": "/c"}]})
      )
      .is_empty()
    );
    assert!(agui_endpoints("https://x", &serde_json::json!({"slug": "a"})).is_empty());
  }

  #[tokio::test]
  async fn polls_until_active() {
    let mut statuses: VecDeque<Option<PollState>> =
      [state("building"), state("building"), state("active")].into();
    let resolved = poll_until_resolved(
      async || Ok(statuses.pop_front().expect("scripted status exhausted")),
      Duration::from_millis(1),
      Duration::from_secs(5),
    )
    .await
    .unwrap();
    assert_eq!(resolved, Resolved::Active);
  }

  #[tokio::test]
  async fn reports_failed_as_a_state_not_an_error() {
    // "failed" is a resolved outcome — the caller turns it into a
    // non-zero exit; the poll loop must return it, not error.
    let mut statuses: VecDeque<Option<PollState>> = [state("building"), state("failed")].into();
    let resolved = poll_until_resolved(
      async || Ok(statuses.pop_front().expect("scripted status exhausted")),
      Duration::from_millis(1),
      Duration::from_secs(5),
    )
    .await
    .unwrap();
    assert_eq!(resolved, Resolved::Failed(None));
  }

  #[tokio::test]
  async fn failed_poll_carries_the_row_error() {
    // The server stores the build-failure tail on the row; the poll
    // loop must surface it to the caller untouched.
    let mut statuses: VecDeque<Option<PollState>> = [
      state("building"),
      Some(PollState {
        status: "failed".into(),
        error: Some("import failed (exit 1): Traceback".into()),
      }),
    ]
    .into();
    let resolved = poll_until_resolved(
      async || Ok(statuses.pop_front().expect("scripted status exhausted")),
      Duration::from_millis(1),
      Duration::from_secs(5),
    )
    .await
    .unwrap();
    assert_eq!(
      resolved,
      Resolved::Failed(Some("import failed (exit 1): Traceback".into()))
    );
  }

  #[test]
  fn failure_detail_prints_the_row_error_when_present() {
    let detail = failure_detail(&Some("docker build died".into()));
    assert!(
      detail.starts_with("error: "),
      "row error is prefixed: {detail}"
    );
    assert!(detail.contains("docker build died"));
  }

  #[test]
  fn failure_detail_falls_back_without_a_row_error() {
    let detail = failure_detail(&None);
    assert!(
      detail.contains("build logs are not exposed"),
      "fallback: {detail}"
    );
  }

  #[tokio::test]
  async fn retries_transport_errors_until_terminal() {
    let mut calls = 0u32;
    let resolved = poll_until_resolved(
      async || {
        calls += 1;
        if calls < 3 {
          Err(anyhow::anyhow!("connection reset"))
        } else {
          Ok(state("active"))
        }
      },
      Duration::from_millis(1),
      Duration::from_secs(5),
    )
    .await
    .unwrap();
    assert_eq!(resolved, Resolved::Active);
  }

  #[tokio::test]
  async fn times_out_mentioning_the_last_observation() {
    let err = poll_until_resolved(
      async || Ok(state("building")),
      Duration::from_millis(1),
      Duration::from_millis(30),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("did not resolve"), "unexpected message: {err}");
    assert!(
      err.contains("building"),
      "timeout must carry the last status: {err}"
    );
  }

  #[tokio::test]
  async fn unlisted_row_and_unknown_status_keep_polling() {
    let mut statuses: VecDeque<Option<PollState>> =
      [None, state("uploading"), state("active")].into();
    let resolved = poll_until_resolved(
      async || Ok(statuses.pop_front().expect("scripted status exhausted")),
      Duration::from_millis(1),
      Duration::from_secs(5),
    )
    .await
    .unwrap();
    assert_eq!(resolved, Resolved::Active);
  }
}
