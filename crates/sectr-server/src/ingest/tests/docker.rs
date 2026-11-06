//! The Docker backend's parity with E2B, against a real daemon and the real
//! runner base image: the deploy pipeline, uid 1000, per-turn containers, the
//! reaper, the lifetime limit and the redeploy error.
//!
//! These run wherever a daemon and the base image exist, including Docker
//! Desktop. The image isn't pulled or built: without it (or with
//! `SECTR_SKIP_DOCKER_E2E` set) they skip. Set `SECTR_TEST_RUNNER_BASE_IMAGE`
//! to use a tag other than this version's `sectr-runner-base`.

use super::*;

use crate::platform::{DockerPlatform, RunnerPlatform};
use crate::reaper::{OrphanReason, ReapOutcome, ReaperMode, reap_once};

/// The base image to test on, if these tests can run here.
fn base_image() -> Option<String> {
  if std::env::var_os("SECTR_SKIP_DOCKER_E2E").is_some() {
    return None;
  }
  let image = std::env::var("SECTR_TEST_RUNNER_BASE_IMAGE")
    .unwrap_or_else(|_| format!("sectr-runner-base:v{}", env!("CARGO_PKG_VERSION")));
  let found = std::process::Command::new("docker")
    .args(["image", "inspect", &image])
    .output()
    .is_ok_and(|output| output.status.success());
  if !found {
    eprintln!("skipping: no docker daemon, or no local image {image}");
    return None;
  }
  Some(image)
}

/// The host address containers reach the test's server at.
fn host_from_a_container() -> &'static str {
  if cfg!(target_os = "macos") {
    "host.docker.internal"
  } else {
    "172.17.0.1"
  }
}

/// A short-lived Docker network, removed on drop.
struct TestNetwork(String);

impl TestNetwork {
  fn create() -> Self {
    let name = format!("sectr-test-{}", uuid::Uuid::now_v7().simple());
    let status = std::process::Command::new("docker")
      .args(["network", "create", &name])
      .output()
      .expect("docker network create");
    assert!(status.status.success(), "{status:?}");
    TestNetwork(name)
  }
}

impl Drop for TestNetwork {
  fn drop(&mut self) {
    let _ = std::process::Command::new("docker")
      .args(["network", "rm", &self.0])
      .output();
  }
}

/// A live container's value for a `docker inspect` format string.
fn inspect(name: &str, format: &str) -> String {
  let output = std::process::Command::new("docker")
    .args(["inspect", "-f", format, name])
    .output()
    .expect("docker inspect");
  assert!(output.status.success(), "{output:?}");
  String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

const APP_TOML: &str = "[app]\nname = \"parity-app\"\n\n[agent]\nentry = \"main:app\"\n";

/// An agent that reports how it was run, then ends its turn.
const REPORTING_APP: &str = r#"
import os
from collections.abc import AsyncIterator
from uuid import uuid4

from sectr import AgentApp, SessionContext
from sectr.events import MessageDelta, MessageEnd, MessageStart, RunnerEvent

app = AgentApp(name="parity-app")


@app.agent_route("/assist")
async def assist(ctx: SessionContext) -> AsyncIterator[RunnerEvent]:
    msg_id = uuid4().hex[:16]
    report = f"uid={os.getuid()} gid={os.getgid()} home={os.environ['HOME']}"
    yield MessageStart(role="assistant", message_id=msg_id)
    yield MessageDelta(message_id=msg_id, delta={"type": "text", "text": report})
    yield MessageEnd(message_id=msg_id)
"#;

/// An agent that never finishes its turn.
const HANGING_APP: &str = r#"
import asyncio
from collections.abc import AsyncIterator
from uuid import uuid4

from sectr import AgentApp, SessionContext
from sectr.events import MessageDelta, MessageEnd, MessageStart, RunnerEvent

app = AgentApp(name="parity-app")


@app.agent_route("/assist")
async def assist(ctx: SessionContext) -> AsyncIterator[RunnerEvent]:
    msg_id = uuid4().hex[:16]
    yield MessageStart(role="assistant", message_id=msg_id)
    yield MessageDelta(message_id=msg_id, delta={"type": "text", "text": "started"})
    await asyncio.sleep(3600)
    yield MessageEnd(message_id=msg_id)
"#;

/// A deployment's built image, removed on drop so runs don't fill the daemon.
struct Deployed {
  row: sectr_common::registry::Deployment,
}

impl Drop for Deployed {
  fn drop(&mut self) {
    let image = crate::platform::docker::image_tag(self.row.id, self.row.version);
    let _ = std::process::Command::new("docker")
      .args(["image", "rm", &image])
      .output();
  }
}

/// Upload an app and wait for its build to resolve.
async fn deploy(w: &World, files: &[(&str, &str)]) -> Deployed {
  let tarball = package_app(files);
  let (boundary, body) = multipart_body(&tarball, APP_TOML);
  let request = axum::http::Request::builder()
    .method("POST")
    .uri("/deployments")
    .header("Authorization", format!("Bearer {}", w.platform_key))
    .header(
      "Content-Type",
      format!("multipart/form-data; boundary={boundary}"),
    )
    .body(axum::body::Body::from(body))
    .unwrap();
  let response = w.app.clone().oneshot(request).await.unwrap();
  let status = response.status();
  let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
    .await
    .unwrap();
  assert_eq!(
    status,
    axum::http::StatusCode::ACCEPTED,
    "{}",
    String::from_utf8_lossy(&bytes)
  );
  let deployed: Value = serde_json::from_slice(&bytes).unwrap();
  let id: uuid::Uuid = deployed["id"].as_str().unwrap().parse().unwrap();
  let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(300);
  loop {
    let row = w.registry.get_deployment(id).await.unwrap();
    if row.status != DeploymentStatus::Building {
      return Deployed { row };
    }
    assert!(
      tokio::time::Instant::now() < deadline,
      "the build never resolved"
    );
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
  }
}

/// Create a session on the deployed app; the dispatch spawns its runner.
async fn start_session(w: &World) -> uuid::Uuid {
  let (status, body) = post(
    &w.app,
    "/sessions",
    &w.platform_key,
    json!({"app": "parity-app", "route": "/assist", "input": "hi"}),
  )
  .await;
  assert_eq!(status, axum::http::StatusCode::CREATED, "{body}");
  body["session_id"].as_str().unwrap().parse().unwrap()
}

/// All the text the session's messages carried.
async fn journal_text(w: &World, session: uuid::Uuid) -> String {
  w.journal
    .fetch(session, None)
    .await
    .unwrap()
    .iter()
    .map(|event| serde_json::to_string(&event.payload).unwrap())
    .collect()
}

async fn wait_for_tail(w: &World, session: uuid::Uuid, want: &[EventType]) {
  let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
  loop {
    let events = w.journal.fetch(session, None).await.unwrap();
    let types: Vec<EventType> = events.iter().map(|e| e.event_type).collect();
    if types.ends_with(want) {
      return;
    }
    assert!(
      tokio::time::Instant::now() < deadline,
      "journal tail never reached {want:?}; have {types:?}"
    );
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
  }
}

#[tokio::test]
async fn turns_run_as_uid_1000_each_in_a_fresh_container() {
  let Some(image) = base_image() else { return };
  let Some((w, platform)) = world_deploy_on(30, &image, host_from_a_container(), |_| {}).await
  else {
    return;
  };
  let deployed = deploy(&w, &[("main.py", REPORTING_APP)]).await;
  assert_eq!(
    deployed.row.status,
    DeploymentStatus::Active,
    "{:?}",
    deployed.row.error
  );

  let session = start_session(&w).await;
  let seen = watch_runners(platform.driver().clone(), session);
  wait_for_tail(
    &w,
    session,
    &[
      EventType::MessageDelta,
      EventType::MessageEnd,
      EventType::NoMoreActions,
    ],
  )
  .await;
  let text = journal_text(&w, session).await;
  assert!(text.contains("uid=1000 gid=1000 home=/tmp"), "{text}");
  wait_runners_gone(platform.driver(), session).await;
  let first = seen.names();
  assert_eq!(first.len(), 1, "one container ran the turn: {first:?}");
  assert!(first[0].starts_with(&format!("sectr-runner-{}-", session.simple())));

  let (status, _) = post(
    &w.app,
    &format!("/sessions/{session}/input"),
    &w.platform_key,
    json!({"input": "one more turn"}),
  )
  .await;
  assert_eq!(status, axum::http::StatusCode::ACCEPTED);
  wait_for_tail(
    &w,
    session,
    &[
      EventType::InvocationStarted,
      EventType::MessageStart,
      EventType::MessageDelta,
      EventType::MessageEnd,
      EventType::NoMoreActions,
    ],
  )
  .await;
  wait_runners_gone(platform.driver(), session).await;
  assert_eq!(seen.names().len(), 2, "turn 2 ran in a fresh container");
}

/// A dependency-bearing app: the runner and the import both go through
/// `uv run --no-sync`, which needs a writable `HOME` as uid 1000. `uv sync`
/// downloads the example's locked packages during the build.
#[tokio::test]
async fn uv_run_works_as_uid_1000() {
  let Some(image) = base_image() else { return };
  let Some((w, platform)) = world_deploy_on(30, &image, host_from_a_container(), |_| {}).await
  else {
    return;
  };
  let deployed = deploy(
    &w,
    &[
      ("main.py", REPORTING_APP),
      (
        "pyproject.toml",
        include_str!("../../../../../examples/raw/pyproject.toml"),
      ),
      (
        "uv.lock",
        include_str!("../../../../../examples/raw/uv.lock"),
      ),
    ],
  )
  .await;
  assert_eq!(
    deployed.row.status,
    DeploymentStatus::Active,
    "{:?}",
    deployed.row.error
  );

  let session = start_session(&w).await;
  wait_for_tail(&w, session, &[EventType::NoMoreActions]).await;
  let text = journal_text(&w, session).await;
  assert!(text.contains("uid=1000 gid=1000 home=/tmp"), "{text}");
  wait_runners_gone(platform.driver(), session).await;
}

#[tokio::test]
async fn a_failed_import_keeps_its_exit_code_and_logs() {
  let Some(image) = base_image() else { return };
  let Some((w, _)) = world_deploy_on(30, &image, host_from_a_container(), |_| {}).await else {
    return;
  };
  let deployed = deploy(
    &w,
    &[(
      "main.py",
      "import sys\nprint('import-time-output', file=sys.stderr)\nsys.exit(7)\n",
    )],
  )
  .await;
  assert_eq!(deployed.row.status, DeploymentStatus::Failed);
  let error = deployed.row.error.clone().unwrap_or_default();
  assert!(error.contains("import failed (exit 7)"), "{error}");
  assert!(error.contains("import-time-output"), "{error}");
}

#[tokio::test]
async fn an_import_that_writes_no_manifest_says_so() {
  let Some(image) = base_image() else { return };
  let Some((w, _)) = world_deploy_on(30, &image, host_from_a_container(), |_| {}).await else {
    return;
  };
  let deployed = deploy(&w, &[("main.py", "import os\nos._exit(0)\n")]).await;
  assert_eq!(deployed.row.status, DeploymentStatus::Failed);
  let error = deployed.row.error.clone().unwrap_or_default();
  assert!(error.contains("manifest output missing"), "{error}");
}

#[tokio::test]
async fn an_import_that_writes_garbage_says_so() {
  let Some(image) = base_image() else { return };
  let Some((w, _)) = world_deploy_on(30, &image, host_from_a_container(), |_| {}).await else {
    return;
  };
  let deployed = deploy(
    &w,
    &[(
      "main.py",
      "import atexit, os\n\
       atexit.register(lambda: open(os.environ['SECTR_MANIFEST_OUT'], 'w').write('not json'))\n\
       from sectr import AgentApp\n\
       app = AgentApp(name='parity-app')\n\
       @app.agent_route('/assist')\n\
       async def assist(ctx):\n    yield None\n",
    )],
  )
  .await;
  assert_eq!(deployed.row.status, DeploymentStatus::Failed);
  let error = deployed.row.error.clone().unwrap_or_default();
  assert!(error.contains("manifest output is not JSON"), "{error}");
}

#[tokio::test]
async fn the_build_workspace_is_removed_even_when_the_build_fails() {
  let Some(image) = base_image() else { return };
  let build_dir = std::env::temp_dir().join(format!("sectr-build-{}", uuid::Uuid::now_v7()));
  let configured = build_dir.to_string_lossy().into_owned();
  let Some((w, _)) = world_deploy_on(30, &image, host_from_a_container(), |config| {
    config.build_dir = configured;
  })
  .await
  else {
    return;
  };
  let deployed = deploy(&w, &[("main.py", "import sys\nsys.exit(1)\n")]).await;
  assert_eq!(deployed.row.status, DeploymentStatus::Failed);
  let left: Vec<_> = std::fs::read_dir(&build_dir)
    .unwrap()
    .map(|entry| entry.unwrap().file_name())
    .collect();
  std::fs::remove_dir_all(&build_dir).ok();
  assert!(left.is_empty(), "left in the build dir: {left:?}");
}

/// A platform whose runners are just `sleep`, on a deployment row pointing
/// at the base image: for what the reaper and the driver see, with no turn
/// involved.
async fn sleeping_runners(
  image: &str,
  configure: impl FnOnce(&mut crate::config::ServerConfig),
) -> Option<(World, Arc<DockerPlatform>, uuid::Uuid)> {
  let (w, platform) = world_deploy_on(300, image, host_from_a_container(), |config| {
    config.runner_sidecar_cmd = vec!["sleep".into(), "300".into()];
    configure(config);
  })
  .await?;
  let org = w
    .registry
    .create_org("runner-org", "runner-org")
    .await
    .unwrap();
  let app = w
    .registry
    .create_app(NewApp {
      org_id: org.id,
      name: "runner-app".into(),
      slug: "runner-app".into(),
    })
    .await
    .unwrap();
  let deployment = w
    .registry
    .create_deployment(NewDeployment {
      app_id: app.id,
      version: 1,
      manifest: json!({"routes": [{"path": "/assist"}], "runner": {"image": image}}),
    })
    .await
    .unwrap();
  Some((w, platform, deployment.id))
}

/// Teardown can return while the daemon's own auto-remove is still finishing.
async fn wait_container_gone(platform: &DockerPlatform, name: &str) {
  for _ in 0..100 {
    if !platform.driver().exists(name).await.unwrap() {
      return;
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
  }
  panic!("container {name} is still there");
}

/// A session row on `deployment`, for the reaper's lease lookups.
async fn session_on(w: &World, deployment: uuid::Uuid) -> uuid::Uuid {
  let row = w.registry.get_deployment(deployment).await.unwrap();
  w.registry
    .create_session(sectr_common::registry::NewSession {
      id: None,
      app_id: row.app_id,
      deployment_id: deployment,
      route: "/assist".into(),
      origin: sectr_common::api::Origin::Api,
      external_user_id: None,
    })
    .await
    .unwrap()
    .id
}

#[tokio::test]
async fn the_reaper_removes_orphans_after_the_grace_period_and_keeps_owned_runners() {
  let Some(image) = base_image() else { return };
  let Some((w, platform, deployment)) = sleeping_runners(&image, |_| {}).await else {
    return;
  };
  let owned_invocation = uuid::Uuid::now_v7();
  let owned = session_on(&w, deployment).await;
  w.registry
    .claim_lease(
      owned,
      "sectr-server",
      Some(owned_invocation),
      chrono::Duration::seconds(300),
    )
    .await
    .unwrap();
  let owned_name = platform
    .spawn(owned, deployment, owned_invocation, "sit_x")
    .await
    .unwrap()
    .unwrap();

  let orphan = session_on(&w, deployment).await;
  let orphan_name = platform
    .spawn(orphan, deployment, uuid::Uuid::now_v7(), "sit_x")
    .await
    .unwrap()
    .unwrap();

  // Another install on the same daemon: never listed, never touched.
  let (_, other_platform, other_deployment) = sleeping_runners(&image, |config| {
    config.runner_server_url = "ws://other-install:8080/ingest".into();
  })
  .await
  .unwrap();
  let other_name = other_platform
    .spawn(
      uuid::Uuid::now_v7(),
      other_deployment,
      uuid::Uuid::now_v7(),
      "sit_x",
    )
    .await
    .unwrap()
    .unwrap();

  let listed: Vec<String> = platform
    .list_runners()
    .await
    .unwrap()
    .unwrap()
    .into_iter()
    .map(|runner| runner.handle)
    .collect();
  assert!(listed.contains(&owned_name), "{listed:?}");
  assert!(listed.contains(&orphan_name), "{listed:?}");
  assert!(!listed.contains(&other_name), "{listed:?}");

  // Within the grace period, nothing is judged.
  let grace = crate::reaper::REAP_GRACE;
  let outcome = reap_once(
    platform.as_ref(),
    w.registry.as_ref(),
    ReaperMode::On,
    grace,
    chrono::Utc::now(),
  )
  .await;
  let ReapOutcome::Scanned { orphans, .. } = outcome else {
    panic!("expected a completed scan");
  };
  assert!(orphans.is_empty(), "{orphans:?}");
  assert!(platform.driver().exists(&orphan_name).await.unwrap());

  // Past it, the orphan goes and the owned runner stays.
  let outcome = reap_once(
    platform.as_ref(),
    w.registry.as_ref(),
    ReaperMode::On,
    grace,
    chrono::Utc::now() + chrono::Duration::seconds(600),
  )
  .await;
  let ReapOutcome::Scanned { orphans, .. } = outcome else {
    panic!("expected a completed scan");
  };
  assert_eq!(orphans.len(), 1, "{orphans:?}");
  assert_eq!(orphans[0].handle, orphan_name);
  assert_eq!(orphans[0].reason, OrphanReason::NoLiveLease);
  wait_container_gone(&platform, &orphan_name).await;
  assert!(platform.driver().exists(&owned_name).await.unwrap());
  assert!(platform.driver().exists(&other_name).await.unwrap());

  platform.teardown(owned, Some(&owned_name)).await;
  other_platform
    .teardown(uuid::Uuid::now_v7(), Some(&other_name))
    .await;
  wait_container_gone(&platform, &owned_name).await;
}

#[tokio::test]
async fn a_runner_gets_its_network_limits_and_labels() {
  let Some(image) = base_image() else { return };
  let network = TestNetwork::create();
  let network_name = network.0.clone();
  let Some((w, platform, deployment)) = sleeping_runners(&image, |config| {
    config.runner_network = Some(network_name);
    config.runner_resources = crate::config::RunnerResources {
      cpu_count: 0.5,
      memory_mb: 256,
      timeout_secs: 300,
    };
  })
  .await
  else {
    return;
  };
  let session = session_on(&w, deployment).await;
  let invocation = uuid::Uuid::now_v7();
  let name = platform
    .spawn(session, deployment, invocation, "sit_x")
    .await
    .unwrap()
    .unwrap();

  assert!(
    inspect(&name, "{{json .NetworkSettings.Networks}}").contains(&network.0),
    "the runner must join the configured network"
  );
  assert_eq!(inspect(&name, "{{.HostConfig.Memory}}"), "268435456");
  assert_eq!(inspect(&name, "{{.HostConfig.NanoCpus}}"), "500000000");
  assert_eq!(inspect(&name, "{{.Config.User}}"), "1000:1000");
  assert_eq!(
    inspect(&name, "{{index .Config.Labels \"sectr.invocation\"}}"),
    invocation.to_string()
  );
  assert!(
    inspect(&name, "{{index .Config.Labels \"sectr.server\"}}")
      .ends_with(&format!("@{}", network.0))
  );

  platform.teardown(session, Some(&name)).await;
  wait_container_gone(&platform, &name).await;
}

#[tokio::test]
async fn a_pruned_image_gives_the_redeploy_error() {
  let Some(image) = base_image() else { return };
  let Some((w, platform, deployment)) = sleeping_runners(&image, |_| {}).await else {
    return;
  };
  let row = w.registry.get_deployment(deployment).await.unwrap();
  let pruned = w
    .registry
    .create_deployment(NewDeployment {
      app_id: row.app_id,
      version: 2,
      manifest: json!({"runner": {"image": "sectr-app-pruned:v1"}}),
    })
    .await
    .unwrap();
  let error = platform
    .spawn(
      uuid::Uuid::now_v7(),
      pruned.id,
      uuid::Uuid::now_v7(),
      "sit_x",
    )
    .await
    .unwrap_err()
    .to_string();
  assert!(
    error.contains(&format!(
      "image sectr-app-pruned:v1 for deployment {} does not exist: redeploy the app",
      pruned.id
    )),
    "{error}"
  );
}

/// The lifetime limit stops a runaway turn's runner. The turn then ends the
/// same way as after an E2B sandbox timeout: its lease expires and the
/// reconciler journals the error.
#[tokio::test]
async fn lifetime_expiry_ends_in_the_reconcilers_turn_end() {
  let Some(image) = base_image() else { return };
  let Some((w, platform)) = world_deploy_on(4, &image, host_from_a_container(), |config| {
    config.runner_resources.timeout_secs = 8;
  })
  .await
  else {
    return;
  };
  let deployed = deploy(&w, &[("main.py", HANGING_APP)]).await;
  assert_eq!(
    deployed.row.status,
    DeploymentStatus::Active,
    "{:?}",
    deployed.row.error
  );

  let session = start_session(&w).await;
  wait_for_tail(&w, session, &[EventType::MessageDelta]).await;

  // The runner exists until its lifetime runs out, then stops by itself.
  assert_eq!(
    platform
      .driver()
      .list_by_label("sectr.session", &session.to_string())
      .await
      .unwrap()
      .len(),
    1,
    "the hanging turn's runner is alive"
  );
  let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(45);
  loop {
    let left = platform
      .driver()
      .list_by_label("sectr.session", &session.to_string())
      .await
      .unwrap();
    if left.is_empty() {
      break;
    }
    assert!(
      tokio::time::Instant::now() < deadline,
      "the runner outlived its lifetime: {left:?}"
    );
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
  }

  let reconciler = Reconciler::new(
    w.registry.clone(),
    w.journal.clone(),
    std::time::Duration::from_secs(15),
  )
  .with_aborter(Arc::new(crate::ingest::HubAbort(w.hub.clone())));
  let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
  loop {
    reconciler.scan_once().await.unwrap();
    let events = w.journal.fetch(session, None).await.unwrap();
    let types: Vec<EventType> = events.iter().map(|e| e.event_type).collect();
    if types.ends_with(&[EventType::SessionError, EventType::NoMoreActions]) {
      break;
    }
    assert!(
      tokio::time::Instant::now() < deadline,
      "the reconciler never ended the turn: {types:?}"
    );
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
  }
}

/// A reclaimed lease always terminates the runner: the reconciler's abort
/// tears down the container named by the turn's handle.
#[tokio::test]
async fn a_reclaimed_turns_runner_is_torn_down() {
  let Some(image) = base_image() else { return };
  let Some((w, platform)) = world_deploy_on(3, &image, host_from_a_container(), |_| {}).await
  else {
    return;
  };
  let deployed = deploy(&w, &[("main.py", HANGING_APP)]).await;
  assert_eq!(
    deployed.row.status,
    DeploymentStatus::Active,
    "{:?}",
    deployed.row.error
  );

  let session = start_session(&w).await;
  wait_for_tail(&w, session, &[EventType::MessageDelta]).await;
  let reconciler = Reconciler::new(
    w.registry.clone(),
    w.journal.clone(),
    std::time::Duration::from_secs(15),
  )
  .with_aborter(Arc::new(crate::ingest::HubAbort(w.hub.clone())));
  let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
  loop {
    reconciler.scan_once().await.unwrap();
    let events = w.journal.fetch(session, None).await.unwrap();
    let types: Vec<EventType> = events.iter().map(|e| e.event_type).collect();
    if types.ends_with(&[EventType::SessionError, EventType::NoMoreActions]) {
      break;
    }
    assert!(
      tokio::time::Instant::now() < deadline,
      "the reconciler never ended the turn: {types:?}"
    );
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
  }
  wait_runners_gone(platform.driver(), session).await;
}
