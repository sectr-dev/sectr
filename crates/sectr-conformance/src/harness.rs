//! Runs cases: for each (adapter, endpoint, scenario, case), a fresh dev
//! store, a `DevDispatch` pointed at the generated project, and a mock model
//! run serving the endpoint.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use sectr_common::drivers::{InMemorySecretSource, SubprocessDriver};
use sectr_common::journal::Journal;
use sectr_common::registry::{DeploymentStatus, NewApp, NewDeployment, Registry};
use sectr_common::store::SqliteStore;
use sectr_worker::dispatch::{DevDispatch, DispatchConfig};
use sectr_worker::turn::TurnEngine;

use crate::adapter::Adapter;
use crate::case::Case;
use crate::compile;
use crate::endpoint::Endpoint;
use crate::mock::MockServer;
use crate::report::{Finished, Outcome, Output, Report};
use crate::scenario::{CaseDef, Plan, Scenario};
use crate::scenarios;
use crate::warning;
use crate::warning::Warning;

/// Runner output lines shown with a failure.
const LOG_TAIL: usize = 40;

/// Upper bound on one whole case.
const CASE_TIMEOUT: Duration = Duration::from_secs(120);

/// Runner ports, one per case and never reused in a run. They are below
/// the OS's ephemeral ranges (Linux from 32768, macOS from 49152), so no
/// outgoing connection or other dispatch can take one before its runner
/// binds it.
const PORTS: std::ops::Range<u16> = 20000..32768;

pub struct Harness {
  gen_dir: PathBuf,
  python: PathBuf,
  mock: MockServer,
  scratch: PathBuf,
  next_port: AtomicU16,
}

impl Harness {
  /// Sync the generated projects' Python env and start the mock model.
  pub async fn new(gen_dir: &Path) -> Result<Self> {
    let gen_dir = gen_dir
      .canonicalize()
      .with_context(|| format!("no generated projects at {}", gen_dir.display()))?;
    println!("run: syncing the Python env in {}", gen_dir.display());
    let status = tokio::process::Command::new("uv")
      .args(["sync", "--all-packages", "--locked", "--quiet"])
      .current_dir(&gen_dir)
      .status()
      .await
      .context("running uv")?;
    if !status.success() {
      bail!(
        "`uv sync` failed (is uv.lock current? run `uv lock` in {})",
        gen_dir.display()
      );
    }
    let scratch = std::env::temp_dir().join(format!("sectr-conformance-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&scratch)?;
    Ok(Harness {
      python: gen_dir.join(".venv/bin/python"),
      gen_dir,
      mock: MockServer::start().await?,
      scratch,
      next_port: AtomicU16::new(PORTS.start),
    })
  }

  /// Run every case whose `adapter/endpoint/scenario/case` id contains
  /// `filter`, `jobs` at a time, showing progress as `output` says, then
  /// print the summary. Returns whether everything passed (known failures
  /// count as passing).
  /// Run the cases `filter` selects, `jobs` at a time, and report them;
  /// with `warnings_out`, also write every warning there as JSON lines.
  pub async fn run_all(
    &self,
    filter: Option<&str>,
    jobs: usize,
    output: Output,
    warnings_out: Option<&Path>,
  ) -> Result<bool> {
    let started = Instant::now();
    let scenarios = scenarios::all();
    let mut queue = Vec::new();
    for adapter in Adapter::ALL {
      for endpoint in Endpoint::ALL {
        for (index, scenario) in scenarios.iter().enumerate() {
          if !scenario.adapters().contains(&adapter) {
            continue;
          }
          for (case, def) in scenario.cases.iter().enumerate() {
            let id = format!(
              "{}/{}/{}/{}",
              adapter.dir(),
              endpoint.id(),
              scenario.id,
              def.name
            );
            if filter.is_none_or(|f| id.contains(f)) {
              queue.push((adapter, endpoint, index, case));
            }
          }
        }
      }
    }
    println!("run: {} cases, {jobs} at a time", queue.len());
    let mut warnings_out = match warnings_out {
      Some(path) => Some(std::io::BufWriter::new(
        std::fs::File::create(path).with_context(|| format!("create {}", path.display()))?,
      )),
      None => None,
    };
    let mut report = Report::new(output, queue.len());

    // `buffer_unordered` pulls (and so starts) a case only when one of the
    // `jobs` running finishes. The cases are polled on this task: they
    // mostly wait, and each turn already runs on its own spawned task.
    let mut finished = futures::stream::iter(queue)
      .map(|(adapter, endpoint, index, case)| {
        let scenario = &scenarios[index];
        let def = &scenario.cases[case];
        async move {
          let t = Instant::now();
          let (outcome, warnings) = self.run_one(adapter, endpoint, index, scenario, def).await;
          Finished {
            adapter,
            endpoint,
            scenario_index: index,
            scenario: scenario.id,
            case_index: case,
            case: def.name,
            outcome,
            warnings,
            secs: t.elapsed().as_secs_f32(),
          }
        }
      })
      .buffer_unordered(jobs);
    while let Some(run) = finished.next().await {
      if let Some(out) = &mut warnings_out {
        write_warnings(out, &run)?;
      }
      report.add(run);
    }
    if let Some(out) = &mut warnings_out {
      out.flush()?;
    }
    Ok(report.finish(started.elapsed()))
  }

  async fn run_one(
    &self,
    adapter: Adapter,
    endpoint: Endpoint,
    index: usize,
    scenario: &Scenario,
    case: &CaseDef,
  ) -> (Outcome, Vec<Warning>) {
    let (run, known) = match case.plan(adapter, endpoint) {
      Plan::Skip(reason) => return (Outcome::Skipped(reason.into()), Vec::new()),
      Plan::Run(run) => (run, None),
      Plan::ExpectFailure(run, reason) => (run, Some(reason)),
    };
    let project = self
      .gen_dir
      .join(compile::project_dir(adapter, index, scenario));
    let run_id = Self::run_id(adapter, endpoint, scenario, case);
    let mut warnings = Vec::new();
    let result = async {
      let mut h = self
        .case(adapter, endpoint, scenario, &run_id, &project)
        .await?;
      let result = match tokio::time::timeout(CASE_TIMEOUT, async {
        run(&mut h).await?;
        h.finish().await
      })
      .await
      {
        Ok(result) => result,
        Err(_) => Err(anyhow::anyhow!("the case timed out after {CASE_TIMEOUT:?}")),
      };
      warnings = h.warnings().to_vec();
      result
    }
    .await;
    let outcome = match (result, known) {
      (Ok(()), None) => Outcome::Passed,
      (Ok(()), Some(reason)) => Outcome::Fixed(reason),
      (Err(err), None) => Outcome::Failed(format!(
        "{err:#}\n  runner output (last {LOG_TAIL} lines):\n{}",
        self.log_tail(&run_id)
      )),
      (Err(err), Some(reason)) => Outcome::KnownFailure(
        reason,
        format!("{err:#}").lines().next().unwrap_or_default().into(),
      ),
    };
    (outcome, warnings)
  }

  /// A fresh dev store with one app and deployment, and a dispatch whose
  /// runner is the generated project, served by this case's mock run.
  async fn case(
    &self,
    adapter: Adapter,
    endpoint: Endpoint,
    scenario: &Scenario,
    run_id: &str,
    project: &Path,
  ) -> Result<Case> {
    let run_id = run_id.to_owned();
    let db = self.scratch.join(format!("{run_id}.db"));
    let store = SqliteStore::open(&db.to_string_lossy()).await?;
    let registry: Arc<dyn Registry> = Arc::new(store.registry());
    let journal: Arc<dyn Journal> = Arc::new(store.journal());

    let org = registry.create_org("conformance", "conformance").await?;
    let app = registry
      .create_app(NewApp {
        org_id: org.id,
        name: run_id.clone(),
        slug: run_id.clone(),
      })
      .await?;
    let deployment = registry
      .create_deployment(NewDeployment {
        endpoint_url: None,
        app_id: app.id,
        version: 1,
        manifest: serde_json::json!({}),
      })
      .await?;
    registry
      .set_deployment_status(deployment.id, DeploymentStatus::Active)
      .await?;

    let model = self
      .mock
      .register(&run_id, endpoint, scenario.agent.mcp.tools);
    let dispatch = DevDispatch::new(
      journal,
      registry,
      Arc::new(TurnEngine::new()),
      // The runner's stderr goes to the case's log through the driver,
      // which keeps its end for a startup failure's SESSION_ERROR.
      Arc::new(SubprocessDriver::with_stderr_log(self.log_path(&run_id))),
      Arc::new(InMemorySecretSource::new()),
      DispatchConfig {
        lease_ttl: chrono::Duration::seconds(60),
        ready_deadline: Duration::from_secs(30),
        ready_poll_interval: Duration::from_millis(50),
        // `sectr dev`'s runner, with its output appended to the case's
        // log instead of the harness's terminal.
        runner_command: vec![
          "/bin/sh".into(),
          "-c".into(),
          r#"exec "$0" "$@" >>"$SECTR_CONFORMANCE_LOG""#.into(),
          self.python.to_string_lossy().into_owned(),
          "-m".into(),
          "sectr.server".into(),
          "main:app".into(),
        ],
        runner_workdir: Some(project.to_path_buf()),
        runner_port: Some(self.runner_port()?),
        runner_env: vec![
          ("SECTR_CONFORMANCE_URL".into(), model.base_url().into()),
          ("SECTR_CONFORMANCE_ENDPOINT".into(), endpoint.id().into()),
          (
            "SECTR_CONFORMANCE_LOG".into(),
            self.log_path(&run_id).to_string_lossy().into_owned(),
          ),
        ],
        // As `sectr dev` passes `[deploy] approval_tools`.
        approval_tools: scenario
          .policy
          .iter()
          .map(|&name| name.to_owned())
          .collect(),
        env_file: None,
      },
      app.id,
      deployment.id,
    );
    Ok(Case::new(
      adapter,
      endpoint,
      scenario.agent.clone(),
      scenario.mode,
      Arc::new(dispatch),
      model,
    ))
  }
}

impl Harness {
  /// The next free port in [`PORTS`], skipping any another process
  /// listens on.
  fn runner_port(&self) -> Result<u16> {
    loop {
      let port = self.next_port.fetch_add(1, Ordering::Relaxed);
      if !PORTS.contains(&port) {
        bail!("out of runner ports ({PORTS:?})");
      }
      if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
        return Ok(port);
      }
    }
  }

  fn run_id(adapter: Adapter, endpoint: Endpoint, scenario: &Scenario, case: &CaseDef) -> String {
    format!(
      "{}-{}-{}-{}",
      adapter.dir(),
      endpoint.id(),
      scenario.id,
      case.name
    )
  }

  fn log_path(&self, run_id: &str) -> PathBuf {
    self.scratch.join(format!("{run_id}.log"))
  }

  fn log_tail(&self, run_id: &str) -> String {
    let path = self.log_path(run_id);
    let log = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = log.lines().collect();
    lines[lines.len().saturating_sub(LOG_TAIL)..]
      .iter()
      .map(|l| format!("    {l}"))
      .collect::<Vec<_>>()
      .join("\n")
  }
}

impl Drop for Harness {
  fn drop(&mut self) {
    let _ = std::fs::remove_dir_all(&self.scratch);
  }
}

/// `run`'s warnings, one JSON object per line.
fn write_warnings(out: &mut impl Write, run: &Finished) -> Result<()> {
  for w in &run.warnings {
    let line = serde_json::json!({
      "run": format!(
        "{}/{}/{}/{}",
        run.adapter.dir(),
        run.endpoint.id(),
        run.scenario,
        run.case
      ),
      "adapter": run.adapter.dir(),
      "endpoint": run.endpoint.id(),
      "kind": w.kind,
      "reason": warning::known(w.kind, run.adapter, run.endpoint),
      "detail": w.detail,
    });
    writeln!(out, "{line}")?;
  }
  Ok(())
}
