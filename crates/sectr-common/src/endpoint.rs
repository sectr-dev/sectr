//! Endpoint deployments: what the server, the CLI and the dashboard share.
//!
//! The server's endpoint errors are built from the phrases below, and the
//! CLI (and the dashboard, whose test reads this file) match on them to add
//! a hint about what to fix. Change a phrase here and every reader follows;
//! reword one in a message without it and the readers' tests fail.

use serde::{Deserialize, Serialize};

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
/// The app's manifest import failed in the endpoint's image. The message
/// goes on with the app's own stderr, which may hold any of the phrases
/// above, so readers give no hint for it.
pub const IMPORT_FAILED: &str = "the manifest import failed in your image";
/// A wake whose sidecar never took the turn in time.
pub const WAKE_TIMEOUT: &str = "did not start the turn within";

/// The longest wake deadline a server may set
/// (`SECTR_ENDPOINT_WAKE_DEADLINE_SECS`). A serve-mode sidecar that has
/// dialed nothing for longer than this gives up on its wake: the server has
/// already failed the turn. Changing it changes that window
/// (`sectr_worker::serve::WAKE_DIAL_WINDOW`), whose docs state it in
/// seconds.
pub const MAX_WAKE_DEADLINE_SECS: u64 = 90;

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

/// `GET /endpoint-deployments/config`: whether the server takes endpoint
/// deployments, the ingest URL an endpoint's image must dial, and the base
/// image it builds from. Absent fields read as unset, so the CLI can tell
/// an older server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointDeploymentsConfig {
  #[serde(default)]
  pub enabled: bool,
  #[serde(default)]
  pub ingest_url: Option<String>,
  #[serde(default)]
  pub base_image: Option<String>,
}

/// `POST /endpoint-deployments`'s body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterEndpoint {
  /// `sectr.toml`, as text.
  pub manifest: String,
  pub endpoint_url: String,
}

// ---------------------------------------------------------------------------
// Serve mode's wire shapes (`schema/serve.schema.json`): the sidecar writes
// them and the server reads them, so both sides are checked against one
// definition.
// ---------------------------------------------------------------------------

/// `GET /`: identifies a serve-mode sidecar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
  pub sectr_sidecar: SidecarIdentity,
}

/// The sidecar's version and mode (always `serve` from this endpoint).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidecarIdentity {
  pub version: String,
  pub mode: String,
}

/// The mode a serve-mode sidecar names in its [`Identity`].
pub const SERVE_MODE: &str = "serve";

/// `POST /wake`'s body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WakeRequest {
  pub ingest_token: String,
}

/// `POST /wake`'s answer, once the invocation is over. Receivers ignore
/// outcomes they don't know ([`WakeAnswer::Unknown`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum WakeAnswer {
  /// A turn ran and is over; the journal holds how it ended.
  TurnEnded,
  /// Ingest refused the token before any turn started.
  TokenRejected,
  /// No dial was accepted in time; nothing ran.
  DialFailed { message: String },
  /// An outcome newer than this reader.
  #[serde(other, skip_serializing)]
  Unknown,
}

impl WakeAnswer {
  /// The `outcome`, for logs.
  pub fn outcome(&self) -> &'static str {
    match self {
      WakeAnswer::TurnEnded => "turn_ended",
      WakeAnswer::TokenRejected => "token_rejected",
      WakeAnswer::DialFailed { .. } => "dial_failed",
      WakeAnswer::Unknown => "unknown",
    }
  }
}

/// `POST /manifest`'s body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestRequest {
  pub registration_token: String,
  pub entry: String,
}

/// `POST /manifest`'s answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ManifestAnswer {
  /// The import's manifest (what `python3 -m sectr.inspect` writes), and
  /// whether the import ran under `uv run --no-sync`.
  Manifest {
    manifest: serde_json::Value,
    uses_uv: bool,
  },
  /// Ingest answered 401 at the dial; the app was not imported.
  TokenRejected,
  /// The dial failed for another reason; the app was not imported.
  DialFailed { message: String },
  /// The import failed. `exit_code` is `None` when it never exited on its
  /// own; `message` then says why.
  ImportFailed {
    exit_code: Option<i32>,
    stderr_tail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    message: Option<String>,
  },
}

impl ManifestAnswer {
  /// The `outcome`, for logs.
  pub fn outcome(&self) -> &'static str {
    match self {
      ManifestAnswer::Manifest { .. } => "manifest",
      ManifestAnswer::TokenRejected => "token_rejected",
      ManifestAnswer::DialFailed { .. } => "dial_failed",
      ManifestAnswer::ImportFailed { .. } => "import_failed",
    }
  }
}

/// A refused serve-mode request: the `error` code (`bad_request`, `busy`,
/// `too_large`, `internal`) and a message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
  pub error: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub message: Option<String>,
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Every serve-mode golden fixture reads as its shape and writes back
  /// the same JSON (unknown fields aside, which readers drop).
  #[test]
  fn serve_fixtures_round_trip_through_the_shapes() {
    fn round_trip<T: serde::de::DeserializeOwned + Serialize>(
      doc: &serde_json::Value,
    ) -> serde_json::Value {
      let typed: T = serde_json::from_value(doc.clone()).unwrap();
      serde_json::to_value(typed).unwrap()
    }
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
      .join("../../schema/fixtures/valid/serve");
    let mut seen = 0;
    for entry in std::fs::read_dir(dir).unwrap() {
      let path = entry.unwrap().path();
      let name = path.file_stem().unwrap().to_string_lossy().into_owned();
      let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
      let written = match name.as_str() {
        "identity" => round_trip::<Identity>(&doc),
        n if n.starts_with("wake_request") => round_trip::<WakeRequest>(&doc),
        n if n.starts_with("wake_") => round_trip::<WakeAnswer>(&doc),
        "manifest_request" => round_trip::<ManifestRequest>(&doc),
        n if n.starts_with("manifest") => round_trip::<ManifestAnswer>(&doc),
        n if n.starts_with("error_") => round_trip::<Refusal>(&doc),
        other => panic!("no shape for serve fixture {other}"),
      };
      let mut expected = doc;
      if name == "wake_request_with_unknown_field" {
        expected.as_object_mut().unwrap().remove("ingest_url");
      }
      assert_eq!(written, expected, "{name}");
      seen += 1;
    }
    assert!(seen > 0, "no serve fixtures found");
  }

  #[test]
  fn an_unknown_wake_outcome_is_read_not_refused() {
    let answer: WakeAnswer = serde_json::from_str(r#"{"outcome": "something_new"}"#).unwrap();
    assert_eq!(answer, WakeAnswer::Unknown);
  }

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
