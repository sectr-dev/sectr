//! Endpoint deployments in the CLI: the agent runs on the developer's own
//! AWS Lambda, Google Cloud Run or container platform, and Sectr wakes it per turn.
//! `sectr init --endpoint` writes the image's Dockerfile, the developer
//! deploys it with their own tools, and `sectr deploy` registers it when
//! `sectr.toml` has `[deploy] endpoint`.
//!
//! This module holds what `init` and `deploy` share: the platforms, the
//! setup steps printed for each, and the hints added to the server's errors.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, bail};
use sectr_common::endpoint::{
  ANSWERED_HTTP, COULD_NOT_CONNECT, DIAL_FAILED, DISABLED, IMPORT_FAILED, NO_ANSWER_IN_TIME,
  PRIVATE_ADDRESS, Platform as Host, WRONG_INGEST, platform_of,
};

/// The setup guide for each platform.
pub const GUIDE: &str = "https://docs.sectr.dev/guides/your-own-compute/";

/// Where the developer runs the image (`sectr init --endpoint <platform>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
  Lambda,
  CloudRun,
  Container,
}

impl Platform {
  pub fn from_arg(arg: &str) -> anyhow::Result<Self> {
    match arg {
      "lambda" => Ok(Platform::Lambda),
      "cloud-run" => Ok(Platform::CloudRun),
      "container" => Ok(Platform::Container),
      other => bail!(
        "unknown endpoint platform {other:?} — the platforms are `lambda`, `cloud-run` and \
         `container`"
      ),
    }
  }

  pub fn arg(&self) -> &'static str {
    match self {
      Platform::Lambda => "lambda",
      Platform::CloudRun => "cloud-run",
      Platform::Container => "container",
    }
  }

  /// Where it runs, in a sentence ("deploy it on …").
  pub fn name(&self) -> &'static str {
    match self {
      Platform::Lambda => "AWS Lambda",
      Platform::CloudRun => "Google Cloud Run",
      Platform::Container => "your container platform",
    }
  }

  /// What the URL to put in sectr.toml is called on this platform.
  pub fn url_name(&self) -> &'static str {
    match self {
      Platform::Lambda => "the function URL",
      Platform::CloudRun => "the service URL",
      Platform::Container => "its HTTPS URL",
    }
  }

  /// The platform an endpoint URL points at, as far as its host tells.
  fn of_url(url: &str) -> Platform {
    let host = reqwest::Url::parse(url)
      .ok()
      .and_then(|u| u.host_str().map(str::to_owned))
      .unwrap_or_default();
    match platform_of(&host) {
      Host::Lambda => Platform::Lambda,
      Host::CloudRun => Platform::CloudRun,
      Host::Other => Platform::Container,
    }
  }

  /// How to build and deploy the image on this platform: printed by `init`.
  pub fn next_steps(&self, app_name: &str) -> Vec<String> {
    let mut steps = match self {
      Platform::Lambda => vec![
        format!("1. docker build --platform linux/amd64 --provenance=false -t {app_name} ."),
        "2. push it to ECR, then create a Lambda function from the image".to_owned(),
        "3. give it a function URL with auth type NONE (the console also adds the public \
         invoke permissions; with the AWS CLI, add both lambda:InvokeFunctionUrl and \
         lambda:InvokeFunction for principal *)"
          .to_owned(),
        "   The function needs: timeout 900 s · memory 1024 MB or more".to_owned(),
      ],
      Platform::CloudRun => vec![format!(
        "1. gcloud run deploy {app_name} --source . --allow-unauthenticated \
           --timeout 3600 --concurrency 1"
      )],
      Platform::Container => vec![
        // Without --platform, an Apple Silicon Mac builds arm64, which an
        // amd64 host can't run.
        format!("1. docker build --platform linux/amd64 -t {app_name} ."),
        "   (--platform linux/arm64 instead if your platform runs on Arm)".to_owned(),
        "2. run it with its port published and PORT set (8080 by default), behind HTTPS, \
         with requests allowed to last as long as a turn"
          .to_owned(),
      ],
    };
    steps.push(format!("Full guide: {GUIDE}"));
    steps
  }
}

/// What to do about a server error from an endpoint deployment, if the
/// CLI knows: registration refusals, failed registrations and warnings.
/// `url` is the endpoint URL from `sectr.toml`.
pub fn hint(message: &str, url: &str) -> Option<String> {
  // The rest of an import failure is the app's own stderr, which can say
  // anything: what to fix is in it, not here.
  if message.contains(IMPORT_FAILED) {
    return None;
  }
  let platform = Platform::of_url(url);
  if message.contains(DISABLED) {
    return Some("remove `[deploy] endpoint` from sectr.toml to deploy to Sectr Run".into());
  }
  if message.contains(&format!("{ANSWERED_HTTP} 403")) {
    return match platform {
      Platform::Lambda => Some(
        "a Lambda function URL needs auth type NONE, and a resource policy that lets anyone \
         call both lambda:InvokeFunctionUrl and lambda:InvokeFunction"
          .into(),
      ),
      Platform::CloudRun => {
        Some("allow unauthenticated invocations (deploy with --allow-unauthenticated)".into())
      }
      Platform::Container => None,
    };
  }
  if message.contains(PRIVATE_ADDRESS) {
    return Some("the server can only reach public compute endpoints".into());
  }
  if message.contains(COULD_NOT_CONNECT) || message.contains(NO_ANSWER_IN_TIME) {
    return Some("check the URL, and that the service is deployed and running".into());
  }
  if message.contains(WRONG_INGEST) {
    return Some(format!(
      "fix SECTR_INGEST_URL in the Dockerfile, or rerun `sectr init --endpoint {} --force` \
       ({REWRITES}), then rebuild and redeploy the image",
      platform.arg()
    ));
  }
  if message.contains(DIAL_FAILED) && platform == Platform::Lambda {
    return Some("a Lambda function in a VPC needs a NAT gateway for outbound connections".into());
  }
  None
}

/// What `init --force` does to the endpoint files: said wherever a hint
/// suggests it, since it drops the developer's own changes.
const REWRITES: &str = "it rewrites the Dockerfile and .dockerignore from scratch";

/// The hint for a registration warning (today only the sidecar's version).
pub fn warning_hint(url: &str) -> String {
  format!(
    "update the Dockerfile's FROM line, or rerun `sectr init --endpoint {} --force` \
     ({REWRITES}), then rebuild and redeploy the image",
    Platform::of_url(url).arg()
  )
}

/// What an endpoint image pins from its server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pins {
  /// The image to build `FROM`: the runner base image of the server's
  /// version.
  pub base_image: String,
  /// The only server the image's sidecar dials (`SECTR_INGEST_URL`).
  pub ingest_url: String,
}

/// Read the pins from the server (`GET /endpoint-deployments/config`),
/// resolved like every cloud command. Fails when there's no server, or it
/// doesn't take endpoint deployments.
pub async fn fetch_pins(api: Option<&str>, key: Option<&str>) -> anyhow::Result<Pins> {
  let target = crate::auth::resolve(api, key).map_err(|e| {
    anyhow::anyhow!(
      "the Dockerfile pins your server's base image and ingest URL, so this needs a server: \
       {e:#}"
    )
  })?;
  fetch_pins_from(&target).await
}

pub(crate) async fn fetch_pins_from(target: &crate::auth::Target) -> anyhow::Result<Pins> {
  let client = reqwest::Client::builder()
    .timeout(Duration::from_secs(15))
    .build()
    .context("build HTTP client")?;
  let url = format!("{}/endpoint-deployments/config", target.api);
  let response = client
    .get(&url)
    .bearer_auth(&target.key)
    .send()
    .await
    .with_context(|| format!("GET {url}"))?;
  let status = response.status();
  if status == reqwest::StatusCode::NOT_FOUND {
    bail!(
      "{} doesn't support endpoint deployments (it's older than this CLI)",
      target.api
    );
  }
  let text = response.text().await.unwrap_or_default();
  if !status.is_success() {
    bail!("GET {url} returned {status}: {}", text.trim());
  }
  let config: serde_json::Value =
    serde_json::from_str(&text).with_context(|| format!("GET {url} didn't answer JSON"))?;
  let field = |name: &str| {
    config
      .get(name)
      .and_then(|v| v.as_str())
      .filter(|v| !v.is_empty())
      .map(str::to_owned)
  };
  if config.get("enabled").and_then(|v| v.as_bool()) != Some(true) {
    bail!(
      "{} {DISABLED} (its admin turns them on with SECTR_ENDPOINT_INGEST_URL)",
      target.api
    );
  }
  match (field("base_image"), field("ingest_url")) {
    (Some(base_image), Some(ingest_url)) => Ok(Pins {
      base_image,
      ingest_url,
    }),
    _ => bail!(
      "{} didn't name its base image and ingest URL (it's older than this CLI)",
      target.api
    ),
  }
}

/// Never in an image: local tooling, and `.env`, so local secrets don't
/// get baked in.
pub const DOCKERIGNORE: &str = ".git\n.venv\nvenv\n__pycache__\n*.pyc\n.agent\nnode_modules\n\
                                .mypy_cache\n.pytest_cache\n.ruff_cache\n.env\n";

/// The comment above the `endpoint` key `init` adds.
const ENDPOINT_COMMENT: &str =
  "# Your compute endpoint's URL, once the image is deployed: `sectr deploy` registers it.\n";

/// Write the endpoint files into the agent directory `dir`: `Dockerfile`
/// and `.dockerignore` (refused if either exists, unless `force`), and
/// `endpoint = ""` under `[deploy]` in its `sectr.toml` unless it has an
/// `endpoint` already. Returns whether it added the key.
pub fn write_files(
  dir: &Path,
  platform: Platform,
  pins: &Pins,
  force: bool,
) -> anyhow::Result<bool> {
  let sectr_toml = dir.join("sectr.toml");
  let text = std::fs::read_to_string(&sectr_toml)
    .with_context(|| format!("reading {}", sectr_toml.display()))?;
  // Everything is checked before anything is written.
  let edited = add_endpoint_key(&text)?;
  if !force {
    for name in ["Dockerfile", ".dockerignore"] {
      if dir.join(name).exists() {
        bail!(
          "{} exists — refusing to overwrite it (pass --force to replace it)",
          dir.join(name).display()
        );
      }
    }
  }
  let dockerfile = sectr_common::dockerfile::endpoint_dockerfile(
    &pins.base_image,
    &pins.ingest_url,
    platform == Platform::Lambda,
  );
  for (name, contents) in [
    ("Dockerfile", dockerfile.as_str()),
    (".dockerignore", DOCKERIGNORE),
  ] {
    let path = dir.join(name);
    std::fs::write(&path, contents).with_context(|| format!("writing {}", path.display()))?;
  }
  if let Some(edited) = &edited {
    std::fs::write(&sectr_toml, edited)
      .with_context(|| format!("writing {}", sectr_toml.display()))?;
  }
  Ok(edited.is_some())
}

/// `sectr.toml` with `endpoint = ""` added under `[deploy]`, keeping its
/// comments and layout; `None` when it has an `endpoint` already.
fn add_endpoint_key(text: &str) -> anyhow::Result<Option<String>> {
  let mut doc: toml_edit::DocumentMut = text
    .parse()
    .context("sectr.toml isn't valid TOML — fix it first")?;
  let deploy = doc.entry("deploy").or_insert_with(toml_edit::table);
  if let Some(table) = deploy.as_table_mut() {
    if table.contains_key("endpoint") {
      return Ok(None);
    }
    table.insert("endpoint", toml_edit::value(""));
    if let Some(mut key) = table.key_mut("endpoint") {
      key.leaf_decor_mut().set_prefix(ENDPOINT_COMMENT);
    }
  } else if let Some(inline) = deploy.as_inline_table_mut() {
    // `deploy = { … }` is one line: a comment can't go inside it.
    if inline.contains_key("endpoint") {
      return Ok(None);
    }
    inline.insert("endpoint", "".into());
  } else {
    bail!("`deploy` in sectr.toml isn't a table");
  }
  let edited = doc.to_string();
  // Refuse rather than write a sectr.toml that doesn't parse.
  edited
    .parse::<toml::Table>()
    .context("adding `[deploy] endpoint` would break sectr.toml — add `endpoint = \"\"` under [deploy] yourself")?;
  Ok(Some(edited))
}

/// The app's name in `sectr.toml` (`[app] name`), for the printed steps.
pub fn app_name(dir: &Path) -> Option<String> {
  let text = std::fs::read_to_string(dir.join("sectr.toml")).ok()?;
  let table: toml::Table = text.parse().ok()?;
  table.get("app")?.get("name")?.as_str().map(str::to_owned)
}

/// The lines `init` prints after writing the endpoint files: the platform's
/// steps, then registering.
pub fn print_next_steps(platform: Platform, app_name: &str) {
  println!(
    "Next: build the image and deploy it on {}:",
    platform.name()
  );
  for step in platform.next_steps(app_name) {
    println!("  {step}");
  }
  println!(
    "Then put {} in sectr.toml ([deploy] endpoint) and run `sectr deploy`.",
    platform.url_name()
  );
}

#[cfg(test)]
pub(crate) mod tests {
  use super::*;

  const LAMBDA: &str = "https://abc.lambda-url.us-east-1.on.aws/";
  const RUN: &str = "https://agent-xyz-uc.a.run.app";
  const OTHER: &str = "https://agent.example.com/";

  fn pins() -> Pins {
    Pins {
      base_image: "ghcr.io/sectr-dev/sectr-runner-base:v1.2.3".into(),
      ingest_url: "wss://ingest.sectr.dev/ingest".into(),
    }
  }

  #[test]
  fn the_key_is_added_keeping_the_files_comments() {
    let text = "# My agent.\n[app]\nname = \"a\" # keep me\n\n[agent]\nentry = \"app:app\"\n";
    let edited = add_endpoint_key(text).unwrap().unwrap();
    assert!(edited.starts_with(text), "{edited}");
    assert!(
      edited.ends_with(&format!("\n[deploy]\n{ENDPOINT_COMMENT}endpoint = \"\"\n")),
      "{edited}"
    );
    assert_eq!(
      crate::deploy::deploy_mode(&edited).unwrap(),
      crate::deploy::Mode::EndpointUnset
    );

    // An existing [deploy] table gains the key; its other keys stay.
    let text = "[deploy]\napproval_tools = [\"refund\"]\n";
    let edited = add_endpoint_key(text).unwrap().unwrap();
    assert!(
      edited.contains("approval_tools = [\"refund\"]\n"),
      "{edited}"
    );
    assert!(edited.contains("endpoint = \"\""), "{edited}");

    // A set value is kept.
    assert_eq!(
      add_endpoint_key("[deploy]\nendpoint = \"https://a.run.app\"\n").unwrap(),
      None
    );
    // An inline or dotted [deploy] gains the key too, and stays valid TOML.
    for text in [
      "deploy = { approval_tools = [\"x\"] }\n",
      "deploy = {}\n",
      "deploy.approval_tools = [\"x\"]\n",
    ] {
      let edited = add_endpoint_key(text).unwrap().unwrap();
      assert_eq!(
        crate::deploy::deploy_mode(&edited).unwrap(),
        crate::deploy::Mode::EndpointUnset,
        "{edited}"
      );
      let table: toml::Table = edited.parse().unwrap();
      if text.contains("approval_tools") {
        assert!(table["deploy"].get("approval_tools").is_some(), "{edited}");
      }
    }
    assert_eq!(
      add_endpoint_key("deploy = { endpoint = \"https://a.run.app\" }\n").unwrap(),
      None
    );
    assert!(add_endpoint_key("deploy = 1\n").is_err());
    assert!(add_endpoint_key("not = [toml").is_err());
  }

  #[test]
  fn files_are_written_once_unless_forced() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("sectr.toml"), "[app]\nname = \"a\"\n").unwrap();
    assert!(write_files(dir.path(), Platform::Lambda, &pins(), false).unwrap());
    let dockerfile = std::fs::read_to_string(dir.path().join("Dockerfile")).unwrap();
    assert!(dockerfile.contains("FROM ghcr.io/sectr-dev/sectr-runner-base:v1.2.3\n"));
    assert!(dockerfile.contains("ENV SECTR_INGEST_URL=wss://ingest.sectr.dev/ingest\n"));
    assert!(dockerfile.contains("aws-lambda-adapter"));
    let ignore = std::fs::read_to_string(dir.path().join(".dockerignore")).unwrap();
    assert!(ignore.lines().any(|l| l == ".env") && ignore.lines().any(|l| l == ".venv"));

    let err = write_files(dir.path(), Platform::CloudRun, &pins(), false)
      .unwrap_err()
      .to_string();
    assert!(
      err.contains("Dockerfile exists") && err.contains("--force"),
      "{err}"
    );
    // Forced: replaced, and the key (already there) isn't added twice.
    assert!(!write_files(dir.path(), Platform::CloudRun, &pins(), true).unwrap());
    let dockerfile = std::fs::read_to_string(dir.path().join("Dockerfile")).unwrap();
    assert!(!dockerfile.contains("aws-lambda-adapter"));
    let toml = std::fs::read_to_string(dir.path().join("sectr.toml")).unwrap();
    assert_eq!(toml.matches("endpoint =").count(), 1, "{toml}");
    assert_eq!(app_name(dir.path()).as_deref(), Some("a"));
  }

  #[test]
  fn nothing_is_written_when_sectr_toml_is_broken() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("sectr.toml"), "not = [toml").unwrap();
    assert!(write_files(dir.path(), Platform::Lambda, &pins(), false).is_err());
    assert!(!dir.path().join("Dockerfile").exists());
  }

  async fn config_stub(status: u16, body: serde_json::Value) -> crate::auth::Target {
    use axum::http::StatusCode;
    let app = axum::Router::new().route(
      "/endpoint-deployments/config",
      axum::routing::get(move || {
        let body = body.clone();
        async move { (StatusCode::from_u16(status).unwrap(), axum::Json(body)) }
      }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    crate::auth::Target {
      api: format!("http://{addr}"),
      key: "skp_x".into(),
    }
  }

  /// A server that takes endpoint deployments, pinning [`pins`].
  pub(crate) async fn enabled_stub() -> crate::auth::Target {
    config_stub(
      200,
      serde_json::json!({
        "enabled": true,
        "ingest_url": "wss://ingest.sectr.dev/ingest",
        "base_image": "ghcr.io/sectr-dev/sectr-runner-base:v1.2.3",
      }),
    )
    .await
  }

  /// A server with endpoint deployments off.
  pub(crate) async fn disabled_stub() -> crate::auth::Target {
    config_stub(
      200,
      serde_json::json!({"enabled": false, "ingest_url": null, "base_image": "x"}),
    )
    .await
  }

  #[tokio::test]
  async fn pins_come_from_the_servers_config() {
    let target = enabled_stub().await;
    assert_eq!(fetch_pins_from(&target).await.unwrap(), pins());

    let off = disabled_stub().await;
    let err = fetch_pins_from(&off).await.unwrap_err().to_string();
    assert!(err.contains(DISABLED), "{err}");

    let old = config_stub(404, serde_json::json!({})).await;
    let err = fetch_pins_from(&old).await.unwrap_err().to_string();
    assert!(err.contains("older than this CLI"), "{err}");

    let unreachable = crate::auth::Target {
      api: "http://127.0.0.1:1".into(),
      key: "skp_x".into(),
    };
    assert!(fetch_pins_from(&unreachable).await.is_err());
  }

  #[test]
  fn urls_classify_by_host() {
    assert_eq!(Platform::of_url(LAMBDA), Platform::Lambda);
    assert_eq!(Platform::of_url(RUN), Platform::CloudRun);
    assert_eq!(Platform::of_url(OTHER), Platform::Container);
    assert_eq!(
      Platform::of_url("https://run.app.evil.com/"),
      Platform::Container
    );
    assert_eq!(Platform::of_url("not a url"), Platform::Container);
  }

  /// The server's messages, as `sectr-server` builds them from the shared
  /// phrases, each with the hint expected per platform.
  #[test]
  fn each_server_error_gets_its_hint() {
    let forbidden =
      format!("your compute endpoint at {LAMBDA} {ANSWERED_HTTP} 403: it refused the request.");
    let lambda = hint(&forbidden, LAMBDA).unwrap();
    assert!(lambda.contains("auth type NONE"), "{lambda}");
    let run = hint(&forbidden, RUN).unwrap();
    assert!(run.contains("--allow-unauthenticated"), "{run}");
    assert_eq!(hint(&forbidden, OTHER), None);
    // Another status isn't an auth problem.
    assert_eq!(
      hint(
        &format!("your compute endpoint {ANSWERED_HTTP} 404, not a…"),
        LAMBDA
      ),
      None
    );

    let disabled = format!("this server {DISABLED} (its SECTR_ENDPOINT_INGEST_URL is not set)");
    assert!(
      hint(&disabled, OTHER)
        .unwrap()
        .contains("remove `[deploy] endpoint`")
    );

    for message in [
      format!("{COULD_NOT_CONNECT} agent.example.com (connection refused)"),
      format!("your compute endpoint at {OTHER} {NO_ANSWER_IN_TIME}"),
    ] {
      assert!(
        hint(&message, OTHER).unwrap().contains("check the URL"),
        "{message}"
      );
    }
    let private = format!("agent.example.com {PRIVATE_ADDRESS}, which this server doesn't allow");
    assert!(
      hint(&private, OTHER)
        .unwrap()
        .contains("public compute endpoints")
    );

    let wrong = format!("your compute endpoint's sidecar {WRONG_INGEST} registration. Check that…");
    assert!(
      hint(&wrong, RUN)
        .unwrap()
        .contains("sectr init --endpoint cloud-run --force"),
      "{wrong}"
    );
    assert!(
      hint(&wrong, OTHER)
        .unwrap()
        .contains("--endpoint container")
    );

    // Every hint that suggests --force says what it replaces.
    for hint in [hint(&wrong, RUN).unwrap(), warning_hint(LAMBDA)] {
      assert!(hint.contains(REWRITES), "{hint}");
    }
    assert!(warning_hint(RUN).contains("FROM line"));
    assert!(warning_hint(RUN).contains("--endpoint cloud-run --force"));

    let dial = format!("your compute endpoint's sidecar {DIAL_FAILED} wss://x/ingest: timed out.");
    assert!(hint(&dial, LAMBDA).unwrap().contains("NAT gateway"));
    assert_eq!(hint(&dial, RUN), None);

    // An import failure carries the app's own stderr: no hint, even when
    // that stderr holds a phrase another hint matches on.
    for stderr in [
      "Traceback",
      "psycopg2.OperationalError: could not connect to server: Connection refused",
      "httpx.ConnectError: couldn't reach api.example.com",
    ] {
      let message = format!("{IMPORT_FAILED} (exit 1): {stderr}");
      assert_eq!(hint(&message, LAMBDA), None, "{message}");
    }
  }

  #[test]
  fn platforms_round_trip_and_unknown_ones_are_refused() {
    for platform in [Platform::Lambda, Platform::CloudRun, Platform::Container] {
      assert_eq!(Platform::from_arg(platform.arg()).unwrap(), platform);
    }
    let err = Platform::from_arg("fly").unwrap_err().to_string();
    assert!(
      err.contains("`lambda`") && err.contains("`container`"),
      "{err}"
    );
  }

  #[test]
  fn every_platforms_steps_end_with_the_guide() {
    for platform in [Platform::Lambda, Platform::CloudRun, Platform::Container] {
      let steps = platform.next_steps("support-agent");
      assert_eq!(steps.last().unwrap(), &format!("Full guide: {GUIDE}"));
      assert!(steps[0].contains("support-agent"), "{platform:?}");
    }
    let lambda = Platform::Lambda.next_steps("a").join("\n");
    assert!(lambda.contains("--platform linux/amd64 --provenance=false"));
    assert!(lambda.contains("lambda:InvokeFunction "), "{lambda}");
    assert!(lambda.contains("timeout 900 s"));
    let container = Platform::Container.next_steps("a").join("\n");
    assert!(container.contains("--platform linux/amd64"), "{container}");
    let run = Platform::CloudRun.next_steps("a").join("\n");
    assert!(run.contains("--timeout 3600 --concurrency 1"));
  }
}
