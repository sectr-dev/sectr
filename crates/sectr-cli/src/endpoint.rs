//! Endpoint deployments in the CLI: the agent runs on the developer's own
//! Lambda, Cloud Run or container platform, and Sectr wakes it per turn.
//! `sectr init --endpoint` writes the image's Dockerfile, the developer
//! deploys it with their own tools, and `sectr deploy` registers it when
//! `sectr.toml` has `[deploy] endpoint`.
//!
//! This module holds what `init` and `deploy` share: the platforms, the
//! setup steps printed for each, and the hints added to the server's errors.

use anyhow::bail;
use sectr_common::endpoint::{
  ANSWERED_HTTP, COULD_NOT_CONNECT, DIAL_FAILED, DISABLED, NO_ANSWER_IN_TIME, PRIVATE_ADDRESS,
  Platform as Host, WRONG_INGEST, platform_of,
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
        format!("1. docker build -t {app_name} ."),
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
  let platform = Platform::of_url(url);
  if message.contains(DISABLED) {
    return Some(
      "remove `[deploy] endpoint` from sectr.toml to deploy with Sectr's runners".into(),
    );
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
    return Some("the server can only reach public endpoints".into());
  }
  if message.contains(COULD_NOT_CONNECT) || message.contains(NO_ANSWER_IN_TIME) {
    return Some("check the URL, and that the service is deployed and running".into());
  }
  if message.contains(WRONG_INGEST) {
    return Some(format!(
      "fix it in the Dockerfile (`sectr init --endpoint {} --force` rewrites it), then \
       rebuild and redeploy the image",
      platform.arg()
    ));
  }
  if message.contains(DIAL_FAILED) && platform == Platform::Lambda {
    return Some("a Lambda function in a VPC needs a NAT gateway for outbound connections".into());
  }
  None
}

/// The hint for a registration warning (today only the sidecar's version).
pub fn warning_hint(url: &str) -> String {
  format!(
    "`sectr init --endpoint {} --force` rewrites the Dockerfile with this server's base image",
    Platform::of_url(url).arg()
  )
}

#[cfg(test)]
mod tests {
  use super::*;

  const LAMBDA: &str = "https://abc.lambda-url.us-east-1.on.aws/";
  const RUN: &str = "https://agent-xyz-uc.a.run.app";
  const OTHER: &str = "https://agent.example.com/";

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
      format!("the endpoint at {LAMBDA} {ANSWERED_HTTP} 403, not a Sectr sidecar in serve mode.");
    let lambda = hint(&forbidden, LAMBDA).unwrap();
    assert!(lambda.contains("auth type NONE"), "{lambda}");
    let run = hint(&forbidden, RUN).unwrap();
    assert!(run.contains("--allow-unauthenticated"), "{run}");
    assert_eq!(hint(&forbidden, OTHER), None);
    // Another status isn't an auth problem.
    assert_eq!(
      hint(&format!("the endpoint {ANSWERED_HTTP} 404, not a…"), LAMBDA),
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
      format!("the endpoint at {OTHER} {NO_ANSWER_IN_TIME}"),
    ] {
      assert!(
        hint(&message, OTHER).unwrap().contains("check the URL"),
        "{message}"
      );
    }
    let private = format!("agent.example.com {PRIVATE_ADDRESS}, which this server doesn't allow");
    assert!(hint(&private, OTHER).unwrap().contains("public endpoints"));

    let wrong = format!("the endpoint's sidecar {WRONG_INGEST} registration. Check that…");
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

    let dial = format!("the endpoint's sidecar {DIAL_FAILED} wss://x/ingest: timed out.");
    assert!(hint(&dial, LAMBDA).unwrap().contains("NAT gateway"));
    assert_eq!(hint(&dial, RUN), None);

    // An import failure carries its own stderr: no hint.
    assert_eq!(
      hint(
        "the manifest import failed in your image (exit 1): Traceback",
        LAMBDA
      ),
      None
    );
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
    let run = Platform::CloudRun.next_steps("a").join("\n");
    assert!(run.contains("--timeout 3600 --concurrency 1"));
  }
}
