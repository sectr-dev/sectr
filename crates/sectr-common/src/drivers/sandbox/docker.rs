//! `DockerSandboxDriver` — the production [`SandboxDriver`] surface:
//! per-session runner containers on the same host as the server (bollard;
//! the server holds the Docker socket).
//!
//! This is a **container-specific** surface ([`ContainerRun`] →
//! [`RunnerHandle`]), not the generic [`SandboxDriver::spawn`]-a-`RunnerSpec`
//! shape: a container needs an image, a name (idempotent
//! re-spawn), labels (bookkeeping/cleanup), and env — none of which fit the
//! subprocess-oriented `RunnerSpec` (command/workdir/env) without polluting
//! the dev path. The trait boundary that matters for swapping sandbox
//! technologies is the server's `SessionSpawner` (spawn per session) +
//! Contract 4's dial-out design, both already driver-agnostic.
//!
//! Lifecycle: `spawn_container` starts the container detached and returns
//! immediately (readiness is NOT the driver's job — the module doc of
//! `mod.rs`; in prod the sidecar's `ready` frame is the signal). Handles
//! wait/kill via the Docker API. Containers are created with
//! `AutoRemove`, so an exited container leaves nothing behind.
//!
//! The runner base image has no entrypoint: `cmd` is the whole command.

// Bollard 0.19 deprecates the hand-written container types in favor of the
// OpenAPI-generated builders/stubs; the deprecated surface still works and
// is what the 0.19 docs' examples use for these calls. Revisit on the next
// bollard major.
#![allow(deprecated)]

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use bollard::Docker;
use bollard::container::{
  Config, CreateContainerOptions, DownloadFromContainerOptions, KillContainerOptions,
  ListContainersOptions, LogOutput, RemoveContainerOptions, WaitContainerOptions,
};
use bollard::secret::HostConfig;
use futures::StreamExt;

use super::RunnerHandle;
use crate::drivers::{DriverError, Result, RunnerExit};

/// How long Docker API calls may take (connect + per-call).
const API_TIMEOUT: Duration = Duration::from_secs(30);

/// The most a one-shot's output file may hold. The file is written by user
/// code, and it's read into memory.
const MAX_OUTPUT_FILE_BYTES: usize = 16 * 1024 * 1024;

/// Everything needed to materialize one runner container.
#[derive(Debug, Clone, Default)]
pub struct ContainerRun {
  /// Image reference (registry path + tag). Must exist locally or on the
  /// configured registry — pulling is NOT automatic here (the build
  /// path produces and pins the image; the spawner resolves it).
  pub image: String,
  /// Container name. Given a name, spawn is idempotent: a leftover
  /// container with the same name is removed first, so a name must identify
  /// one runner and never be shared between turns.
  pub name: Option<String>,
  /// Labels (bookkeeping: org/app/session tagging for cleanup tooling).
  pub labels: HashMap<String, String>,
  /// Environment (`KEY` → `VALUE`), rendered as `KEY=VALUE`.
  pub env: Vec<(String, String)>,
  /// The command to run (overrides the image CMD).
  pub cmd: Vec<String>,
  /// Daemon-side auto-remove on exit. RUNNER containers set this TRUE
  /// (the sidecar exits itself at the turn boundary, and
  /// nothing on the prod path waits for its exit code, so the exited
  /// husk must not linger). Driver tests that assert exit codes via
  /// `wait()` set it FALSE (AutoRemove races the wait: the container is
  /// gone before the code can be read).
  pub auto_remove: bool,
  /// Docker network to join. `None` = the daemon's default bridge.
  pub network: Option<String>,
  /// Memory limit in bytes. `None` = unlimited.
  pub memory: Option<i64>,
  /// CPU limit in billionths of a CPU. `None` = unlimited.
  pub nano_cpus: Option<i64>,
  /// `uid:gid` to run as. `None` = the image's default user.
  pub user: Option<String>,
}

impl ContainerRun {
  /// Env rendered for the Docker API (`KEY=VALUE` pairs).
  fn env_kv(&self) -> Vec<String> {
    self.env.iter().map(|(k, v)| format!("{k}={v}")).collect()
  }
}

/// One container from a listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedContainer {
  /// The container's name, without the API's leading `/`.
  pub name: String,
  pub labels: HashMap<String, String>,
  /// When the container was created, in Unix seconds.
  pub created: i64,
}

/// What a one-shot container left behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OneShotOutput {
  pub exit_code: i32,
  pub logs: String,
  /// The requested output file. `None` when it doesn't exist or isn't a
  /// regular file.
  pub output_file: Option<Vec<u8>>,
}

/// The settings for a one-shot container.
#[derive(Debug, Clone, Default)]
pub struct OneShotRun {
  pub image: String,
  /// `KEY=VALUE` pairs.
  pub env: Vec<String>,
  pub cmd: Vec<String>,
  pub working_dir: Option<String>,
  /// `uid:gid` to run as. `None` = the image's default user.
  pub user: Option<String>,
  /// A file to copy out of the container after it exits.
  pub output_path: String,
}

/// Read the single file out of the tar archive Docker returns for a path.
/// Parsed in memory, so an entry's name can't write anywhere, and a symlink
/// or directory left at the path counts as no file.
fn file_from_archive(archive: &[u8]) -> std::result::Result<Option<Vec<u8>>, String> {
  use std::io::Read;
  let mut archive = tar::Archive::new(archive);
  let mut entries = archive
    .entries()
    .map_err(|e| format!("read output archive: {e}"))?;
  let Some(entry) = entries.next() else {
    return Ok(None);
  };
  let mut entry = entry.map_err(|e| format!("read output archive: {e}"))?;
  if !entry.header().entry_type().is_file() {
    return Ok(None);
  }
  let mut contents = Vec::new();
  entry
    .read_to_end(&mut contents)
    .map_err(|e| format!("read output archive: {e}"))?;
  Ok(Some(contents))
}

/// Bollard-backed container driver. Cheap to clone (the `Docker` client is
/// an `Arc` internally); one per server process.
#[derive(Clone)]
pub struct DockerSandboxDriver {
  docker: Docker,
}

impl DockerSandboxDriver {
  /// Connect over the given Unix socket (e.g. `/var/run/docker.sock`).
  pub fn connect_with_socket(socket_path: &str) -> Result<Self> {
    let docker = Docker::connect_with_socket(
      socket_path,
      API_TIMEOUT.as_secs(),
      bollard::API_DEFAULT_VERSION,
    )
    .map_err(|e| DriverError::Other(format!("docker connect ({socket_path}): {e}")))?;
    Ok(DockerSandboxDriver { docker })
  }

  /// Connect via the environment (`DOCKER_HOST` et al.) — used by tests
  /// and Compose-style deployments.
  pub fn connect_env() -> Result<Self> {
    let docker = Docker::connect_with_local_defaults()
      .map_err(|e| DriverError::Other(format!("docker connect (env): {e}")))?;
    Ok(DockerSandboxDriver { docker })
  }

  /// Liveness/self-check for tests and `/healthz` enrichment.
  pub async fn ping(&self) -> Result<()> {
    self
      .docker
      .ping()
      .await
      .map(|_| ())
      .map_err(|e| DriverError::Other(format!("docker ping: {e}")))
  }

  /// Remove a leftover container with the given name, if any, even a
  /// running one (spawn is idempotent thanks to this pre-step; 404 = nothing
  /// to do).
  pub async fn remove_if_exists(&self, name: &str) -> Result<()> {
    match self
      .docker
      .remove_container(
        name,
        Some(RemoveContainerOptions {
          force: true,
          ..Default::default()
        }),
      )
      .await
    {
      Ok(()) => Ok(()),
      Err(bollard::errors::Error::DockerResponseServerError {
        status_code: 404, ..
      }) => Ok(()),
      Err(e) => Err(DriverError::Other(format!("remove {name}: {e}"))),
    }
  }

  /// Spawn a container detached. Returns as soon as the container exists
  /// and is started — NOT when the runner is ready to serve (see module
  /// doc). With `run.name` set, any leftover same-name container is
  /// removed first (idempotent re-spawn). A container that was created but
  /// wouldn't start is removed before the error returns.
  pub async fn spawn_container(&self, run: ContainerRun) -> Result<Box<dyn RunnerHandle>> {
    if let Some(name) = &run.name {
      self.remove_if_exists(name).await?;
    }
    let options = CreateContainerOptions {
      name: run.name.clone().unwrap_or_default(),
      platform: None,
    };
    let config = Config {
      image: Some(run.image.clone()),
      cmd: (!run.cmd.is_empty()).then(|| run.cmd.clone()),
      env: Some(run.env_kv()),
      labels: (!run.labels.is_empty()).then(|| run.labels.clone()),
      user: run.user.clone(),
      host_config: Some(HostConfig {
        // See ContainerRun::auto_remove: TRUE for runner
        // containers (self-exiting; nothing waits for the exit
        // code on the prod path), FALSE for tests that assert
        // exit codes via `wait()` (the daemon's removal races the
        // wait and loses the code). Exited containers WITHOUT
        // auto-remove are cleaned by teardown (force-remove) or
        // the next same-name spawn.
        auto_remove: Some(run.auto_remove),
        network_mode: run.network.clone(),
        memory: run.memory,
        nano_cpus: run.nano_cpus,
        ..Default::default()
      }),
      ..Default::default()
    };
    let id = self
      .docker
      .create_container(Some(options), config)
      .await
      .map_err(|e| DriverError::Spawn(format!("create container: {e}")))?
      .id;
    if let Err(e) = self
      .docker
      .start_container(
        &id,
        None::<bollard::query_parameters::StartContainerOptions>,
      )
      .await
    {
      if let Err(remove_err) = self.force_remove(&id).await {
        tracing::warn!(container = %id, error = %remove_err, "cleanup after a failed container start also failed");
      }
      return Err(DriverError::Spawn(format!("start container {id}: {e}")));
    }
    Ok(Box::new(DockerRunnerHandle {
      docker: self.docker.clone(),
      id,
    }))
  }

  /// Build an image from a tarred build context (the
  /// caller tars `Dockerfile` + app dir). Returns the build log tail on
  /// failure — the deploy surfaces it to the CLI.
  pub async fn build_image(
    &self,
    tag: &str,
    dockerfile: &str,
    context_tar: Vec<u8>,
  ) -> std::result::Result<(), String> {
    use bollard::image::BuildImageOptions;
    let options = BuildImageOptions {
      t: tag.to_owned(),
      dockerfile: dockerfile.to_owned(),
      rm: true,
      ..Default::default()
    };
    let mut stream =
      self
        .docker
        .build_image(options, None, Some(bollard::body_full(context_tar.into())));
    let mut log_tail = String::new();
    while let Some(item) = stream.next().await {
      match item {
        Ok(info) => {
          if let Some(line) = info.stream {
            // Keep only the tail — build logs are long.
            log_tail.push_str(&line);
            let len = log_tail.len();
            if len > 4000 {
              log_tail = log_tail[len - 4000..].to_owned();
            }
          }
          if let Some(err) = info.error {
            return Err(format!(
              "image build failed: {err}\n--- build log tail ---\n{log_tail}"
            ));
          }
          if let Some(detail) = info.error_detail
            && detail.message.is_some()
          {
            return Err(format!(
              "image build failed: {}\n--- build log tail ---\n{log_tail}",
              detail.message.unwrap_or_default()
            ));
          }
        }
        Err(e) => {
          // The daemon's final stream error (e.g. "The command
          // '/bin/sh -c …' returned a non-zero code: 2") says
          // WHAT failed but not WHY — the failing RUN's own
          // output is in the log tail.
          return Err(format!(
            "image build stream: {e}\n--- build log tail ---\n{log_tail}"
          ));
        }
      }
    }
    Ok(())
  }

  /// Run a one-shot container (deploy flow: the authoritative-manifest
  /// import), wait for it to exit, and return its exit code, logs and output
  /// file. The file is copied from the stopped container, so this needs no
  /// path shared with the daemon. The container is always removed.
  pub async fn run_once(&self, run: OneShotRun) -> std::result::Result<OneShotOutput, String> {
    use bollard::container::{Config, LogsOptions};
    let name = format!("sectr-run-{}", uuid::Uuid::now_v7().simple());
    let options = bollard::container::CreateContainerOptions {
      name: name.clone(),
      platform: None,
    };
    let config = Config {
      image: Some(run.image),
      cmd: (!run.cmd.is_empty()).then_some(run.cmd),
      env: (!run.env.is_empty()).then_some(run.env),
      user: run.user,
      working_dir: run.working_dir,
      ..Default::default()
    };
    let id = self
      .docker
      .create_container(Some(options), config)
      .await
      .map_err(|e| format!("create run-once container: {e}"))?
      .id;
    let result = async {
      self
        .docker
        .start_container(
          &id,
          None::<bollard::query_parameters::StartContainerOptions>,
        )
        .await
        .map_err(|e| format!("start run-once container: {e}"))?;
      // Bollard delivers a NON-ZERO exit as an Err item on the wait
      // stream (DockerContainerWaitError { code, error }) whose Display
      // is just the (usually empty) inner message — the exit code lives
      // only in the variant. Treat it as data: the exit code is what
      // run_once returns, and the logs below are how the caller sees
      // WHY the process failed. A bare stream error (transport) stays
      // an error.
      let exit = match self
        .docker
        .wait_container(
          &id,
          Some(WaitContainerOptions {
            condition: "not-running",
          }),
        )
        .next()
        .await
        .ok_or("wait stream ended")?
      {
        Ok(wait) => wait.status_code,
        Err(bollard::errors::Error::DockerContainerWaitError { code, .. }) => code,
        Err(e) => return Err(format!("wait: {e}")),
      };
      let logs_options = LogsOptions::<String> {
        stdout: true,
        stderr: true,
        follow: false,
        timestamps: false,
        tail: "100".into(),
        ..Default::default()
      };
      let mut logs = String::new();
      let mut log_stream = self.docker.logs(&id, Some(logs_options));
      while let Some(item) = log_stream.next().await {
        if let Ok(
          output
          @ (LogOutput::StdOut { .. } | LogOutput::StdErr { .. } | LogOutput::Console { .. }),
        ) = item
        {
          logs.push_str(&String::from_utf8_lossy(&output.into_bytes()));
        }
      }
      let output_file = self.read_output_file(&id, &run.output_path).await?;
      Ok(OneShotOutput {
        exit_code: i32::try_from(exit).unwrap_or(-1),
        logs,
        output_file,
      })
    }
    .await;
    // Always remove (force: it may still be running if something failed).
    let _ = self
      .docker
      .remove_container(
        &id,
        Some(RemoveContainerOptions {
          force: true,
          ..Default::default()
        }),
      )
      .await;
    result
  }

  /// Copy one file out of a container, which may have stopped. A missing
  /// path is `None`.
  async fn read_output_file(
    &self,
    id: &str,
    path: &str,
  ) -> std::result::Result<Option<Vec<u8>>, String> {
    let mut stream = self.docker.download_from_container(
      id,
      Some(DownloadFromContainerOptions {
        path: path.to_owned(),
      }),
    );
    let mut archive = Vec::new();
    while let Some(chunk) = stream.next().await {
      match chunk {
        Ok(chunk) => {
          archive.extend_from_slice(&chunk);
          // The archive is a little larger than the file: allow for the tar
          // headers and padding.
          if archive.len() > MAX_OUTPUT_FILE_BYTES + 64 * 1024 {
            return Err(format!(
              "{path} is larger than the {MAX_OUTPUT_FILE_BYTES}-byte limit"
            ));
          }
        }
        Err(bollard::errors::Error::DockerResponseServerError {
          status_code: 404, ..
        }) => return Ok(None),
        Err(e) => return Err(format!("copy {path} from the container: {e}")),
      }
    }
    file_from_archive(&archive)
  }

  /// Whether an image exists on this daemon. Nothing is pulled.
  pub async fn image_exists(&self, image: &str) -> Result<bool> {
    match self.docker.inspect_image(image).await {
      Ok(_) => Ok(true),
      Err(bollard::errors::Error::DockerResponseServerError {
        status_code: 404, ..
      }) => Ok(false),
      Err(e) => Err(DriverError::Other(format!("inspect image {image}: {e}"))),
    }
  }

  /// Every container with this label value, in any state, so a container
  /// that was created but never started is listed too.
  pub async fn list_by_label(&self, key: &str, value: &str) -> Result<Vec<ListedContainer>> {
    let filters = HashMap::from([("label".to_owned(), vec![format!("{key}={value}")])]);
    let listed = self
      .docker
      .list_containers(Some(ListContainersOptions::<String> {
        all: true,
        filters,
        ..Default::default()
      }))
      .await
      .map_err(|e| DriverError::Other(format!("list containers: {e}")))?;
    Ok(
      listed
        .into_iter()
        .filter_map(|container| {
          let name = container.names?.into_iter().next()?;
          Some(ListedContainer {
            name: name.trim_start_matches('/').to_owned(),
            labels: container.labels.unwrap_or_default(),
            created: container.created?,
          })
        })
        .collect(),
    )
  }

  /// Force-remove a container. Already gone, or already being removed (an
  /// auto-remove in progress), is success.
  async fn force_remove(&self, id: &str) -> Result<()> {
    match self
      .docker
      .remove_container(
        id,
        Some(RemoveContainerOptions {
          force: true,
          ..Default::default()
        }),
      )
      .await
    {
      Ok(())
      | Err(bollard::errors::Error::DockerResponseServerError {
        status_code: 404 | 409,
        ..
      }) => Ok(()),
      Err(e) => Err(DriverError::Other(format!("remove {id}: {e}"))),
    }
  }

  /// Whether a container with this name/id currently exists (any state).
  pub async fn exists(&self, name: &str) -> Result<bool> {
    match self
      .docker
      .inspect_container(
        name,
        None::<bollard::query_parameters::InspectContainerOptions>,
      )
      .await
    {
      Ok(_) => Ok(true),
      Err(bollard::errors::Error::DockerResponseServerError {
        status_code: 404, ..
      }) => Ok(false),
      Err(e) => Err(DriverError::Other(format!("inspect {name}: {e}"))),
    }
  }

  /// Kill (SIGKILL) a container by name/id, then force-remove the husk.
  /// Missing container = success — kill must be idempotent for the
  /// teardown paths.
  pub async fn kill(&self, id: &str) -> Result<()> {
    match self
      .docker
      .kill_container(
        id,
        Some(KillContainerOptions::<String> {
          signal: "SIGKILL".into(),
        }),
      )
      .await
    {
      Ok(()) => {}
      Err(bollard::errors::Error::DockerResponseServerError {
        status_code: 404 | 409,
        ..
      }) => {}
      Err(e) => return Err(DriverError::Other(format!("kill {id}: {e}"))),
    }
    self.force_remove(id).await
  }
}

/// `RunnerHandle` over a live container. `wait` resolves with the exit
/// code; a killed container resolves promptly.
#[async_trait]
impl RunnerHandle for DockerRunnerHandle {
  fn id(&self) -> String {
    self.id.clone()
  }

  async fn wait(&mut self) -> Result<RunnerExit> {
    let mut stream = self.docker.wait_container(
      &self.id,
      Some(WaitContainerOptions {
        condition: "not-running",
      }),
    );
    match stream.next().await {
      Some(Ok(response)) => Ok(RunnerExit {
        code: Some(i32::try_from(response.status_code).unwrap_or(-1)),
        signal: None,
      }),
      // The container was removed before the wait could observe the
      // exit (kill's force-remove): the runner was terminated by us
      // (SIGKILL) — signal it accordingly instead of erroring.
      Some(Err(bollard::errors::Error::DockerResponseServerError {
        status_code: 404, ..
      })) => Ok(RunnerExit {
        code: None,
        signal: Some(9),
      }),
      Some(Err(e)) => Err(DriverError::Other(format!("wait {}: {e}", self.id))),
      None => Err(DriverError::Other(format!(
        "wait {}: stream ended",
        self.id
      ))),
    }
  }

  async fn kill(&mut self) -> Result<()> {
    self.kill_impl().await
  }
}

struct DockerRunnerHandle {
  docker: Docker,
  id: String,
}

impl DockerRunnerHandle {
  async fn kill_impl(&self) -> Result<()> {
    match self
      .docker
      .kill_container(
        &self.id,
        Some(KillContainerOptions::<String> {
          signal: "SIGKILL".into(),
        }),
      )
      .await
    {
      Ok(()) => {}
      // 404 = already gone; 409 = already exiting (restarting).
      Err(bollard::errors::Error::DockerResponseServerError {
        status_code: 404 | 409,
        ..
      }) => {}
      Err(e) => return Err(DriverError::Other(format!("kill {}: {e}", self.id))),
    }
    // Kill alone leaves an `Exited` husk; force-remove cleans up (the
    // daemon kills it first if it is somehow still running).
    self
      .docker
      .remove_container(
        &self.id,
        Some(RemoveContainerOptions {
          force: true,
          ..Default::default()
        }),
      )
      .await
      .map(|_| ())
      .or_else(|e| match e {
        bollard::errors::Error::DockerResponseServerError {
          status_code: 404, ..
        } => Ok(()),
        e => Err(DriverError::Other(format!("remove {}: {e}", self.id))),
      })
  }
}

/// Docker-sandbox driver tests. These spawn a REAL container as their
/// subject — `postgres:16` (arbitrary but pinned; matches the runner base
/// image family). The image must exist locally: `docker pull postgres:16`
/// (CI pulls it explicitly). No docker daemon → tests skip.
#[cfg(test)]
mod tests {
  use super::*;

  async fn driver() -> Option<DockerSandboxDriver> {
    let d = DockerSandboxDriver::connect_env().ok()?;
    d.ping().await.ok()?;
    Some(d)
  }

  /// Skipped unless a Docker daemon is reachable (sandbox + CI-less dev:
  /// `docker compose -f docker-compose.test.yml up -d` implies it).
  #[tokio::test]
  async fn spawn_wait_kill_roundtrip() {
    let Some(d) = driver().await else {
      eprintln!("skipping: no docker daemon");
      return;
    };
    let mut handle = d
      .spawn_container(ContainerRun {
        image: "postgres:16".into(),
        cmd: vec!["sleep".into(), "30".into()],
        auto_remove: false, // assert exit codes via wait()
        ..Default::default()
      })
      .await
      .expect("spawn");
    let id = handle.id();

    // Wait resolves at exit; kill (SIGKILL) terminates the sleep
    // promptly with a non-zero status (128 + signal).
    handle.kill().await.expect("kill");
    let exit = tokio::time::timeout(Duration::from_secs(15), handle.wait())
      .await
      .expect("wait resolves after kill (15s)")
      .expect("wait ok");
    assert_ne!(exit.code, Some(0), "killed container exits non-zero");
    let _ = id;
  }

  #[tokio::test]
  async fn spawn_removes_leftover_same_name_container() {
    let Some(d) = driver().await else {
      eprintln!("skipping: no docker daemon");
      return;
    };
    let name = format!("sectr-test-{}", uuid::Uuid::now_v7().simple());
    let mut first = d
      .spawn_container(ContainerRun {
        image: "postgres:16".into(),
        name: Some(name.clone()),
        cmd: vec!["sleep".into(), "60".into()],
        auto_remove: false, // assert exit codes via wait()
        ..Default::default()
      })
      .await
      .expect("first spawn");
    first.kill().await.expect("kill");
    let _ = tokio::time::timeout(Duration::from_secs(10), first.wait()).await;
    // (the exited husk is force-removed by kill; give the daemon a beat)
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Same name spawns cleanly over the leftover (removed pre-create).
    let mut second = d
      .spawn_container(ContainerRun {
        image: "postgres:16".into(),
        name: Some(name.clone()),
        cmd: vec!["sleep".into(), "60".into()],
        auto_remove: false, // assert exit codes via wait()
        ..Default::default()
      })
      .await
      .expect("re-spawn over leftover name");
    second.kill().await.expect("kill");

    // kill-by-driver for an already-gone container is a no-op.
    d.kill(&name).await.expect("idempotent driver kill");
  }

  fn one_shot(script: &str) -> OneShotRun {
    OneShotRun {
      image: "postgres:16".into(),
      cmd: vec!["sh".into(), "-c".into(), script.into()],
      output_path: "/tmp/out.json".into(),
      ..Default::default()
    }
  }

  #[tokio::test]
  async fn run_once_copies_the_output_file_from_the_stopped_container() {
    let Some(d) = driver().await else {
      eprintln!("skipping: no docker daemon");
      return;
    };
    let out = d
      .run_once(one_shot("echo noise; printf '{\"a\":1}' > /tmp/out.json"))
      .await
      .unwrap();
    assert_eq!(out.exit_code, 0);
    assert!(out.logs.contains("noise"), "{}", out.logs);
    assert_eq!(out.output_file.as_deref(), Some(&b"{\"a\":1}"[..]));
  }

  #[tokio::test]
  async fn run_once_keeps_logs_and_exit_code_when_the_command_fails() {
    let Some(d) = driver().await else {
      eprintln!("skipping: no docker daemon");
      return;
    };
    let out = d.run_once(one_shot("echo boom >&2; exit 3")).await.unwrap();
    assert_eq!(out.exit_code, 3);
    assert!(out.logs.contains("boom"), "{}", out.logs);
    assert_eq!(out.output_file, None);
  }

  #[tokio::test]
  async fn run_once_treats_a_symlink_or_directory_as_no_output() {
    let Some(d) = driver().await else {
      eprintln!("skipping: no docker daemon");
      return;
    };
    for script in [
      "ln -s /etc/passwd /tmp/out.json",
      "mkdir /tmp/out.json; echo x > /tmp/out.json/inner",
    ] {
      let out = d.run_once(one_shot(script)).await.unwrap();
      assert_eq!(out.exit_code, 0, "{script}");
      assert_eq!(out.output_file, None, "{script}");
    }
  }

  #[tokio::test]
  async fn run_once_refuses_an_oversized_output_file() {
    let Some(d) = driver().await else {
      eprintln!("skipping: no docker daemon");
      return;
    };
    let script = format!(
      "head -c {} /dev/zero > /tmp/out.json",
      MAX_OUTPUT_FILE_BYTES + 1024 * 1024
    );
    let err = d.run_once(one_shot(&script)).await.unwrap_err();
    assert!(err.contains("limit"), "{err}");
  }

  #[tokio::test]
  async fn run_once_runs_as_the_requested_user() {
    let Some(d) = driver().await else {
      eprintln!("skipping: no docker daemon");
      return;
    };
    let mut run = one_shot("id -u > /tmp/out.json");
    run.user = Some("1000:1000".into());
    let out = d.run_once(run).await.unwrap();
    assert_eq!(out.output_file.as_deref(), Some(&b"1000\n"[..]));
  }

  #[tokio::test]
  async fn a_container_that_will_not_start_is_removed() {
    let Some(d) = driver().await else {
      eprintln!("skipping: no docker daemon");
      return;
    };
    let name = format!("sectr-test-{}", uuid::Uuid::now_v7().simple());
    let result = d
      .spawn_container(ContainerRun {
        image: "postgres:16".into(),
        name: Some(name.clone()),
        cmd: vec!["sleep".into(), "30".into()],
        network: Some("sectr-test-no-such-network".into()),
        ..Default::default()
      })
      .await;
    let error = result.err().expect("the start must fail").to_string();
    assert!(error.contains("start container"), "{error}");
    assert!(
      !d.exists(&name).await.unwrap(),
      "the failed container must not be left behind"
    );
  }

  #[tokio::test]
  async fn limits_user_and_labels_reach_the_container() {
    let Some(d) = driver().await else {
      eprintln!("skipping: no docker daemon");
      return;
    };
    let name = format!("sectr-test-{}", uuid::Uuid::now_v7().simple());
    let label = uuid::Uuid::now_v7().to_string();
    let mut handle = d
      .spawn_container(ContainerRun {
        image: "postgres:16".into(),
        name: Some(name.clone()),
        cmd: vec!["sleep".into(), "30".into()],
        labels: HashMap::from([("sectr.test".to_owned(), label.clone())]),
        memory: Some(256 * 1024 * 1024),
        nano_cpus: Some(500_000_000),
        user: Some("1000:1000".into()),
        ..Default::default()
      })
      .await
      .unwrap();
    let inspected = d
      .docker
      .inspect_container(
        &name,
        None::<bollard::query_parameters::InspectContainerOptions>,
      )
      .await
      .unwrap();
    let host = inspected.host_config.unwrap();
    assert_eq!(host.memory, Some(256 * 1024 * 1024));
    assert_eq!(host.nano_cpus, Some(500_000_000));
    assert_eq!(inspected.config.unwrap().user.as_deref(), Some("1000:1000"));

    let listed = d.list_by_label("sectr.test", &label).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, name);
    assert_eq!(listed[0].labels["sectr.test"], label);
    assert!(listed[0].created > 0);
    assert!(
      d.list_by_label("sectr.test", "other")
        .await
        .unwrap()
        .is_empty()
    );
    handle.kill().await.unwrap();
  }

  #[tokio::test]
  async fn image_exists_does_not_pull() {
    let Some(d) = driver().await else {
      eprintln!("skipping: no docker daemon");
      return;
    };
    assert!(d.image_exists("postgres:16").await.unwrap());
    assert!(!d.image_exists("sectr-app-does-not-exist:v1").await.unwrap());
  }

  #[test]
  fn file_from_archive_reads_only_a_regular_file() {
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(2);
    header.set_mode(0o644);
    header.set_cksum();
    builder.append_data(&mut header, "out", &b"hi"[..]).unwrap();
    let archive = builder.into_inner().unwrap();
    assert_eq!(
      file_from_archive(&archive).unwrap().as_deref(),
      Some(&b"hi"[..])
    );

    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(0);
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_cksum();
    builder
      .append_link(&mut header, "out", "/etc/passwd")
      .unwrap();
    let archive = builder.into_inner().unwrap();
    assert_eq!(file_from_archive(&archive).unwrap(), None);

    assert_eq!(file_from_archive(&[]).unwrap(), None);
  }
}
