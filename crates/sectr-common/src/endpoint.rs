//! Endpoint deployments: what the server, the CLI and the dashboard share.
//!
//! The server's endpoint errors are built from the phrases below, and the
//! CLI (and the dashboard, whose test reads this file) match on them to add
//! a hint about what to fix. Change a phrase here and every reader follows;
//! reword one in a message without it and the readers' tests fail.

/// Starts every error a failed wake journals as the turn's `SESSION_ERROR`.
pub const WAKE_FAILED: &str = "endpoint wake failed:";

/// The server has endpoint deployments off.
pub const DISABLED: &str = "doesn't accept endpoint deployments";
/// The endpoint's host is (or resolves to) a private address.
pub const PRIVATE_ADDRESS: &str = "resolves to a private address";
/// No connection to the endpoint.
pub const COULD_NOT_CONNECT: &str = "could not connect to";
/// The endpoint's probe ran out of time.
pub const NO_ANSWER_IN_TIME: &str = "didn't answer in time";
/// Followed by the status the endpoint answered with.
pub const ANSWERED_HTTP: &str = "answered HTTP";
/// The sidecar dialed an ingest that doesn't know its token: the image's
/// `SECTR_INGEST_URL` names another server.
pub const WRONG_INGEST: &str = "dialed a server that doesn't know this";
/// The sidecar couldn't reach ingest at all.
pub const DIAL_FAILED: &str = "couldn't reach";
/// A wake whose sidecar never took the turn in time.
pub const WAKE_TIMEOUT: &str = "did not start the turn within";

/// Where an endpoint runs, as far as its URL tells: hints differ per
/// platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
  /// A Lambda function URL: `<id>.lambda-url.<region>.on.aws`.
  Lambda,
  /// A Cloud Run service: `*.run.app`.
  CloudRun,
  /// Anything else.
  Other,
}

/// Classify an endpoint by its host (lowercase, no port).
pub fn platform_of(host: &str) -> Platform {
  let host = host.trim_end_matches('.').to_ascii_lowercase();
  let labels: Vec<&str> = host.split('.').collect();
  match labels.as_slice() {
    [.., "lambda-url", _region, "on", "aws"] if labels.len() >= 5 => Platform::Lambda,
    [.., "run", "app"] if labels.len() >= 3 => Platform::CloudRun,
    _ => Platform::Other,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn hosts_classify_by_their_suffix() {
    assert_eq!(
      platform_of("abc123.lambda-url.us-east-1.on.aws"),
      Platform::Lambda
    );
    assert_eq!(
      platform_of("ABC.Lambda-Url.eu-west-1.on.aws."),
      Platform::Lambda
    );
    assert_eq!(
      platform_of("support-agent-xyz-uc.a.run.app"),
      Platform::CloudRun
    );
    assert_eq!(platform_of("agent-123.run.app"), Platform::CloudRun);
    // Look-alikes are generic.
    for host in [
      "run.app",
      "run.app.evil.com",
      "lambda-url.us-east-1.on.aws",
      "x.lambda-url.us-east-1.on.aws.evil.com",
      "agent.example.com",
      "",
    ] {
      assert_eq!(platform_of(host), Platform::Other, "{host}");
    }
  }
}
