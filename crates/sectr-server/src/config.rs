//! Server configuration from the environment. All knobs
//! are env vars — the process is a single container on the VPS compose
//! stack; no config files.

use std::net::SocketAddr;

pub use crate::limits::LimitsConfig;

#[derive(Debug, Clone)]
pub struct ServerConfig {
  /// Postgres connection URL. MUST be a direct (unpooled) endpoint — the
  /// journal tail is LISTEN/NOTIFY, which poolers break. `SECTR_PG_URL`,
  /// required.
  pub pg_url: String,

  /// Bind address. `SECTR_BIND_ADDR`, default `0.0.0.0:8080`.
  pub bind_addr: SocketAddr,

  /// Master key for encrypting secret values at rest (libsodium
  /// sealed box / AEAD — decided with G). `SECTR_SECRETS_MASTER_KEY`.
  /// Optional for now; the secrets endpoints refuse to start without it
  /// once they exist.
  #[allow(dead_code)] // consumed by the secrets endpoints
  pub secrets_master_key: Option<String>,

  /// Docker socket for runner spawn/supervise (`SandboxDriver`).
  /// `SECTR_DOCKER_SOCKET`, default the usual `/var/run/docker.sock`.
  #[allow(dead_code)] // consumed by the SandboxDriver
  pub docker_socket: String,

  /// A pre-provisioned platform API key (full value) minted out-of-band
  /// and registered at startup — the bootstrap path out of the
  /// chicken-and-egg problem (no login exists yet that could mint the
  /// first key). Idempotent: already-registered keys are left alone.
  /// `SECTR_BOOTSTRAP_PLATFORM_KEY`, optional.
  pub bootstrap_platform_key: Option<String>,

  /// Lease TTL for runner claims (`SECTR_LEASE_TTL_SECS`, default 300).
  /// Ingest traffic IS the heartbeat — a turn whose events stop for this
  /// long is considered lost, and the reconciler ends it (a genuinely
  /// silent-but-alive generation stall up to 5 minutes is
  /// preferred over false-positive kills).
  pub lease_ttl_secs: u64,

  /// User-token TTL (`SECTR_USER_TOKEN_TTL_SECS`, default 86400 = 24h):
  /// browser tokens are minted per page load with no
  /// refresh logic — clients re-mint (or reload) on a 401.
  pub user_token_ttl_secs: u64,

  /// User-token minting rate limit per (invocation key, external_user_id)
  /// per rolling hour (`SECTR_MINT_RATE_LIMIT_PER_HOUR`, default 240) —
  /// the cheap guard against mint-farms; the real gate is the app's own
  /// auth in front of the mint call.
  pub mint_rate_limit_per_hour: u32,

  /// The ingest URL as seen from runner containers (`SECTR_RUNNER_SERVER_URL`).
  /// The server binds 0.0.0.0, so the docker bridge gateway address is the
  /// default; set it explicitly when the topology differs (macOS Docker
  /// Desktop: `ws://host.docker.internal:PORT/ingest`).
  /// Default: `ws://172.17.0.1:{bind port}/ingest`.
  pub runner_server_url: String,

  /// The sidecar command inside the container (`SECTR_RUNNER_SIDECAR_CMD`,
  /// whitespace-split). The runner base image's ENTRYPOINT is expected to
  /// be the sidecar; this overrides the image CMD. Default:
  /// `/usr/local/bin/sectr-worker`.
  pub runner_sidecar_cmd: Vec<String>,

  /// Default runner image when the deployment manifest carries no
  /// `runner.image` (`SECTR_RUNNER_IMAGE`). the build path pins the
  /// image per deployment; until then a global default keeps dev/testing
  /// possible.
  pub runner_image: Option<String>,

  /// The runner BASE image deploys build on (`SECTR_RUNNER_BASE_IMAGE`):
  /// python + uv + the sectr SDK + the sidecar binary — see
  /// `docker/runner-base.Dockerfile`. Required for `POST /deployments`.
  pub runner_base_image: Option<String>,

  /// Build workspace root (`SECTR_BUILD_DIR`): tarball unpack + manifest
  /// extraction bind-mount source. MUST be shared with the docker daemon
  /// when the server runs containerized (compose: one host volume mounted
  /// into the server container; the daemon sees the host path).
  pub build_dir: String,

  /// Dashboard origin (`SECTR_DASHBOARD_URL`, default `https://app.sectr.dev`,
  /// no trailing slash): where `POST /auth/cli` points the browser.
  pub dashboard_url: String,

  /// The single MVP org's name (created by bootstrap if absent).
  /// `SECTR_ORG_NAME`, default `sectr`.
  pub org_name: String,

  /// Journal tail-pool size (`SECTR_TAIL_POOL_MAX`, default 32): the cap
  /// on concurrent journal tails. Each open SSE tail holds ONE dedicated
  /// Postgres connection for LISTEN for the stream's lifetime — from a
  /// pool separate from requests, so tails can never starve the request
  /// path (M1 run finding: shared pool + held tails = every authed
  /// request hung). Bounds total PG connections ≈ request pool (10) +
  /// this; keep it under the server's `max_connections` budget (Neon
  /// free tier is tight).
  pub tail_pool_max: u32,

  /// Artifact store backend (the model (a)/(b) switch).
  /// `SECTR_ARTIFACT_BACKEND`, `fs` (default) | `gcs`.
  pub artifact_backend: ArtifactBackend,

  /// Filesystem store root when `artifact_backend = Fs`
  /// (`SECTR_ARTIFACTS_DIR`, default `/tmp/sectr-artifacts`).
  pub artifact_dir: String,

  /// GCS bucket when `artifact_backend = Gcs` (`SECTR_GCS_BUCKET`);
  /// required in that mode — startup fails without it.
  pub gcs_bucket: Option<String>,

  /// Optional GCS endpoint override (`SECTR_GCS_ENDPOINT`) for testing
  /// against a fake-gcs-server emulator; anonymous credentials are used
  /// in that mode. Unset = real Cloud Storage via ADC.
  pub gcs_endpoint: Option<String>,

  /// GCP project id when `platform = Gcp` (`SECTR_GCP_PROJECT_ID`); required
  /// in that mode — startup fails without it.
  pub gcp_project_id: Option<String>,

  /// GCP region when `platform = Gcp` (`SECTR_GCP_REGION`); required in
  /// that mode. Must match where the bucket / AR repo / Cloud Run live
  /// (`us-east4`).
  pub gcp_region: Option<String>,

  /// Artifact Registry repo for deployment images when `platform = Gcp`
  /// (`SECTR_GCP_AR_REPO`, default `sectr-runners`) — the R4
  /// image-contract repo (`<region>-docker.pkg.dev/<project>/<repo>/…`).
  pub gcp_ar_repo: String,

  /// The Cloud Build service account builds run AS
  /// (`SECTR_GCP_BUILD_SA_EMAIL`, e.g.
  /// `sectr-builder@<project>.iam.gserviceaccount.com`); required when
  /// `platform = Gcp` — the server submits builds impersonating it via
  /// the Build.service_account field.
  pub gcp_build_sa_email: Option<String>,
  /// SA the ensured Cloud Run Jobs run AS (SECTR_GCP_RUNNER_SA_EMAIL) —
  /// the least-privilege runtime identity (AR reader only; the runner
  /// dials out and needs nothing else). Required for platform gcp.
  pub gcp_runner_sa_email: Option<String>,

  /// E2B account API key when `platform = E2b` (`SECTR_E2B_API_KEY`);
  /// required in that mode — a server secret like the SA key.
  pub e2b_api_key: Option<String>,

  /// E2B domain suffix (`SECTR_E2B_DOMAIN`, default `e2b.app`): platform
  /// API at `https://api.<domain>`, envd at `https://sandbox.<domain>`
  /// (EU deployments use `e2b.eu`).
  pub e2b_domain: String,

  /// Template cpuCount (`SECTR_E2B_CPU_COUNT`, default 1): vCPUs per
  /// sandbox. Free tier totals 8 vCPUs across concurrent sandboxes.
  pub e2b_cpu_count: i32,

  /// Template memoryMB (`SECTR_E2B_MEMORY_MB`, default 1024): MiB per
  /// sandbox. Free tier totals 8 GiB across concurrent sandboxes.
  pub e2b_memory_mb: i32,

  /// Per-turn sandbox TTL in seconds (`SECTR_E2B_SANDBOX_TIMEOUT_SECS`,
  /// default 3600 — the Hobby max continuous runtime). The runaway-turn
  /// safety net ONLY: our teardown kills first; a timeout kill mid-turn
  /// is covered by lease expiry + reconciler (same durability as the
  /// Cloud Run task timeout).
  pub e2b_sandbox_timeout_secs: u32,

  /// Path to the `sectr-e2b-puller` SA JSON key file
  /// (`SECTR_E2B_PULLER_SA_JSON`); required for platform e2b. Its
  /// CONTENT is sent to E2B on every template build
  /// (`fromImageRegistry.serviceAccountJson`) so E2B can pull the AR
  /// image — a third-party-held credential, scoped to
  /// artifactregistry.reader on the runners repo and rotatable.
  pub e2b_puller_sa_json: Option<String>,

  /// Which RunnerPlatform backend the server composes.
  /// `SECTR_PLATFORM`, default `docker` (model a). `gcp`
  /// (model b) and `e2b` REQUIRE the gcs artifact backend —
  /// one flag selects builder AND runner, so they cannot disagree.
  pub platform: PlatformKind,

  /// PostHog project API key for product analytics (`SECTR_POSTHOG_KEY`).
  /// Optional by design: unset → the analytics
  /// client is a no-op and the server emits nothing.
  pub posthog_key: Option<String>,

  /// PostHog ingest host (`SECTR_POSTHOG_HOST`, default the US cloud) —
  /// point at `https://eu.i.posthog.com` or a self-hosted instance.
  pub posthog_host: String,

  /// Discord webhook that gets a message per new account
  /// (`SECTR_SIGNUP_WEBHOOK_URL`; see `signup::SignupNotifier`). Optional:
  /// unset → no notices. A `Secret`: the URL is the webhook's credential.
  pub signup_webhook_url: Option<sectr_common::secret::Secret>,

  /// Clerk signup — `None` ⇒ `POST /auth/clerk` answers 503.
  /// Set by `SECTR_CLERK_ISSUER` + `SECTR_CLERK_JWT_KEY` +
  /// `SECTR_CLERK_AUTHORIZED_PARTIES`, all-or-none (partial = startup
  /// error); `SECTR_DASHBOARD_SESSION_TTL_SECS` is optional.
  pub clerk: Option<ClerkConfig>,

  /// guardrails: concurrent-turn caps, session-creation rate
  /// limits, the signup cap and the orphan-runner reaper mode. See
  /// [`LimitsConfig`] for the env keys.
  pub limits: LimitsConfig,
}

/// Default lifetime of the dashboard-session key `POST /auth/clerk` mints.
const DEFAULT_DASHBOARD_SESSION_TTL_SECS: u64 = 604_800;

/// Clerk session-JWT verification settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClerkConfig {
  /// Expected `iss` — the Clerk Frontend API URL (`SECTR_CLERK_ISSUER`).
  pub issuer: String,
  /// The instance's RS256 public key as a PEM SPKI block, re-wrapped from
  /// the single-line base64 `SECTR_CLERK_JWT_KEY` (systemd
  /// `EnvironmentFile` cannot hold multi-line values).
  pub jwt_key_pem: String,
  /// Allowed `azp` values (`SECTR_CLERK_AUTHORIZED_PARTIES`, comma list).
  pub authorized_parties: Vec<String>,
  /// Lifetime of the exchanged dashboard key
  /// (`SECTR_DASHBOARD_SESSION_TTL_SECS`, default 7 days).
  pub session_ttl_secs: u64,
}

impl ClerkConfig {
  /// All-or-none over the three required vars; the TTL only tunes.
  /// Takes the raw values (not the environment) so it is unit-testable.
  fn from_vars(
    issuer: Option<&str>,
    jwt_key: Option<&str>,
    authorized_parties: Option<&str>,
    session_ttl: Option<&str>,
  ) -> anyhow::Result<Option<Self>> {
    let present = |v: Option<&str>| v.is_some_and(|v| !v.trim().is_empty());
    let set = [issuer, jwt_key, authorized_parties]
      .into_iter()
      .filter(|v| present(*v))
      .count();
    if set == 0 {
      return Ok(None);
    }
    if set != 3 {
      anyhow::bail!(
        "Clerk is partially configured: SECTR_CLERK_ISSUER, SECTR_CLERK_JWT_KEY and \
         SECTR_CLERK_AUTHORIZED_PARTIES must be set together (or none of them)"
      );
    }
    let parties: Vec<String> = authorized_parties
      .unwrap_or_default()
      .split(',')
      .map(str::trim)
      .filter(|s| !s.is_empty())
      .map(str::to_owned)
      .collect();
    if parties.is_empty() {
      anyhow::bail!("SECTR_CLERK_AUTHORIZED_PARTIES has no entries");
    }
    let session_ttl_secs = match session_ttl.map(str::trim).filter(|v| !v.is_empty()) {
      Some(v) => v
        .parse::<u64>()
        .ok()
        .filter(|ttl| *ttl > 0)
        .ok_or_else(|| {
          anyhow::anyhow!("SECTR_DASHBOARD_SESSION_TTL_SECS={v:?} is not a positive integer")
        })?,
      None => DEFAULT_DASHBOARD_SESSION_TTL_SECS,
    };
    Ok(Some(ClerkConfig {
      issuer: issuer.unwrap_or_default().trim().to_owned(),
      jwt_key_pem: spki_base64_to_pem(jwt_key.unwrap_or_default())?,
      authorized_parties: parties,
      session_ttl_secs,
    }))
  }
}

/// Re-wrap single-line base64 SPKI (a PEM body with the BEGIN/END lines and
/// newlines stripped) into a PEM block, and prove it parses as an RSA public
/// key — a bad `SECTR_CLERK_JWT_KEY` must fail at startup, not on the first
/// sign-in.
pub(crate) fn spki_base64_to_pem(body: &str) -> anyhow::Result<String> {
  use base64::Engine as _;
  let body: String = body.split_whitespace().collect();
  base64::engine::general_purpose::STANDARD
    .decode(&body)
    .map_err(|e| anyhow::anyhow!("SECTR_CLERK_JWT_KEY is not valid base64: {e}"))?;
  let mut pem = String::from("-----BEGIN PUBLIC KEY-----\n");
  for line in body.as_bytes().chunks(64) {
    pem.push_str(std::str::from_utf8(line).expect("base64 is ascii"));
    pem.push('\n');
  }
  pem.push_str("-----END PUBLIC KEY-----\n");
  jsonwebtoken::DecodingKey::from_rsa_pem(pem.as_bytes())
    .map_err(|e| anyhow::anyhow!("SECTR_CLERK_JWT_KEY is not an RSA public key (SPKI): {e}"))?;
  Ok(pem)
}

/// The runner-platform backend (model a vs model b vs E2B).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformKind {
  /// Local docker daemon: build + run on the VPS (model a).
  Docker,
  /// Cloud Build + Cloud Run Jobs (model b).
  Gcp,
  /// Cloud Build (build half shared with Gcp) + E2B sandboxes for
  /// execution.
  E2b,
}

impl PlatformKind {
  fn from_env() -> anyhow::Result<Self> {
    match std::env::var("SECTR_PLATFORM")
      .unwrap_or_else(|_| "docker".into())
      .trim()
      .to_ascii_lowercase()
      .as_str()
    {
      "docker" => Ok(PlatformKind::Docker),
      "gcp" | "gcs" | "cloudrun" => Ok(PlatformKind::Gcp),
      "e2b" => Ok(PlatformKind::E2b),
      other => Err(anyhow::anyhow!(
        "SECTR_PLATFORM={other:?} is not one of docker | gcp | e2b"
      )),
    }
  }

  /// Platforms whose BUILD half is Cloud Build (the `gs:` artifact-uri
  /// pairing is structural). Gcp and E2b share it; only spawn/teardown
  /// differ.
  pub fn builds_on_gcp(self) -> bool {
    matches!(self, PlatformKind::Gcp | PlatformKind::E2b)
  }
}

/// Which [`ArtifactStore`](sectr_common::drivers::ArtifactStore) backs the
/// deploy flow. Parsed from `SECTR_ARTIFACT_BACKEND` (default `fs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactBackend {
  /// Filesystem store (model a) — the VPS artifact volume.
  Fs,
  /// GCS store (model b) — needs the sectr-server `gcs` feature.
  Gcs,
}

impl ArtifactBackend {
  fn from_env() -> anyhow::Result<Self> {
    match std::env::var("SECTR_ARTIFACT_BACKEND")
      .unwrap_or_else(|_| "fs".into())
      .trim()
      .to_ascii_lowercase()
      .as_str()
    {
      "fs" | "filesystem" => Ok(ArtifactBackend::Fs),
      "gcs" => Ok(ArtifactBackend::Gcs),
      other => Err(anyhow::anyhow!(
        "SECTR_ARTIFACT_BACKEND={other:?} is not one of fs | gcs"
      )),
    }
  }
}

impl ServerConfig {
  pub fn from_env() -> anyhow::Result<Self> {
    let pg_url = std::env::var("SECTR_PG_URL")
      .map_err(|_| anyhow::anyhow!("SECTR_PG_URL is required (Neon DIRECT/unpooled url)"))?;
    let bind_addr = std::env::var("SECTR_BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let bind_addr: SocketAddr = bind_addr.parse()?;
    let config = ServerConfig {
      pg_url,
      bind_addr,
      secrets_master_key: std::env::var("SECTR_SECRETS_MASTER_KEY").ok(),
      docker_socket: std::env::var("SECTR_DOCKER_SOCKET")
        .unwrap_or_else(|_| "/var/run/docker.sock".into()),
      bootstrap_platform_key: std::env::var("SECTR_BOOTSTRAP_PLATFORM_KEY").ok(),
      org_name: std::env::var("SECTR_ORG_NAME").unwrap_or_else(|_| "sectr".into()),
      lease_ttl_secs: std::env::var("SECTR_LEASE_TTL_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300),
      user_token_ttl_secs: std::env::var("SECTR_USER_TOKEN_TTL_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(86_400),
      mint_rate_limit_per_hour: std::env::var("SECTR_MINT_RATE_LIMIT_PER_HOUR")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(240),
      runner_server_url: std::env::var("SECTR_RUNNER_SERVER_URL")
        .unwrap_or_else(|_| format!("ws://172.17.0.1:{}/ingest", bind_addr.port())),
      runner_sidecar_cmd: std::env::var("SECTR_RUNNER_SIDECAR_CMD")
        .unwrap_or_else(|_| "/usr/local/bin/sectr-worker".into())
        .split_whitespace()
        .map(str::to_owned)
        .collect(),
      runner_image: std::env::var("SECTR_RUNNER_IMAGE").ok(),
      runner_base_image: std::env::var("SECTR_RUNNER_BASE_IMAGE").ok(),
      build_dir: std::env::var("SECTR_BUILD_DIR").unwrap_or_else(|_| "/tmp/sectr-build".into()),
      dashboard_url: std::env::var("SECTR_DASHBOARD_URL")
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "https://app.sectr.dev".into()),
      tail_pool_max: std::env::var("SECTR_TAIL_POOL_MAX")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(32),
      artifact_backend: ArtifactBackend::from_env()?,
      artifact_dir: std::env::var("SECTR_ARTIFACTS_DIR")
        .unwrap_or_else(|_| "/tmp/sectr-artifacts".into()),
      gcs_bucket: std::env::var("SECTR_GCS_BUCKET").ok(),
      gcs_endpoint: std::env::var("SECTR_GCS_ENDPOINT")
        .ok()
        .filter(|s| !s.is_empty()),
      gcp_project_id: std::env::var("SECTR_GCP_PROJECT_ID").ok(),
      gcp_region: std::env::var("SECTR_GCP_REGION").ok(),
      gcp_ar_repo: std::env::var("SECTR_GCP_AR_REPO").unwrap_or_else(|_| "sectr-runners".into()),
      gcp_build_sa_email: std::env::var("SECTR_GCP_BUILD_SA_EMAIL").ok(),
      gcp_runner_sa_email: std::env::var("SECTR_GCP_RUNNER_SA_EMAIL").ok(),
      e2b_api_key: std::env::var("SECTR_E2B_API_KEY").ok(),
      e2b_domain: std::env::var("SECTR_E2B_DOMAIN")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "e2b.app".into()),
      e2b_cpu_count: parse_e2b_cpu_count(std::env::var("SECTR_E2B_CPU_COUNT").ok().as_deref()),
      e2b_memory_mb: std::env::var("SECTR_E2B_MEMORY_MB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1024),
      e2b_sandbox_timeout_secs: std::env::var("SECTR_E2B_SANDBOX_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3600),
      e2b_puller_sa_json: std::env::var("SECTR_E2B_PULLER_SA_JSON").ok(),
      platform: PlatformKind::from_env()?,
      posthog_key: std::env::var("SECTR_POSTHOG_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty()),
      posthog_host: std::env::var("SECTR_POSTHOG_HOST")
        .ok()
        .filter(|h| !h.trim().is_empty())
        .unwrap_or_else(|| "https://us.i.posthog.com".into()),
      signup_webhook_url: std::env::var("SECTR_SIGNUP_WEBHOOK_URL")
        .ok()
        .map(|u| u.trim().to_owned())
        .filter(|u| !u.is_empty())
        .map(sectr_common::secret::Secret::new),
      clerk: ClerkConfig::from_vars(
        std::env::var("SECTR_CLERK_ISSUER").ok().as_deref(),
        std::env::var("SECTR_CLERK_JWT_KEY").ok().as_deref(),
        std::env::var("SECTR_CLERK_AUTHORIZED_PARTIES")
          .ok()
          .as_deref(),
        std::env::var("SECTR_DASHBOARD_SESSION_TTL_SECS")
          .ok()
          .as_deref(),
      )?,
      limits: LimitsConfig::from_env()?,
    };

    // Fail fast on an incoherent artifact-backend configuration — a
    // misconfigured GCS backend must not be discovered on first deploy.
    check_artifact_backend(
      config.platform,
      config.artifact_backend,
      config.gcs_bucket.as_deref(),
    )?;
    // Same discipline for the model-(b) provider keys (E2B's build half
    // IS the gcp build half — project/region/build-SA/base-image are
    // shared requirements; only the runner SA is gcp-specific).
    check_gcp_config(
      config.platform,
      config.gcp_project_id.as_deref(),
      config.gcp_region.as_deref(),
      config.gcp_build_sa_email.as_deref(),
      config.gcp_runner_sa_email.as_deref(),
      config.runner_base_image.as_deref(),
    )?;
    check_e2b_config(
      config.platform,
      config.e2b_api_key.as_deref(),
      config.e2b_puller_sa_json.as_deref(),
      config.e2b_sandbox_timeout_secs,
      config.e2b_cpu_count,
      config.e2b_memory_mb,
    )?;

    Ok(config)
  }
}

/// Refuse a model-(b) platform config that is missing its provider keys at
/// startup rather than on first deploy.
fn check_gcp_config(
  platform: PlatformKind,
  gcp_project_id: Option<&str>,
  gcp_region: Option<&str>,
  gcp_build_sa_email: Option<&str>,
  gcp_runner_sa_email: Option<&str>,
  runner_base_image: Option<&str>,
) -> anyhow::Result<()> {
  if !platform.builds_on_gcp() {
    return Ok(());
  }
  if gcp_project_id.is_none() {
    anyhow::bail!("SECTR_GCP_PROJECT_ID is required when SECTR_PLATFORM=gcp");
  }
  if gcp_region.is_none() {
    anyhow::bail!("SECTR_GCP_REGION is required when SECTR_PLATFORM=gcp");
  }
  if gcp_build_sa_email.is_none() {
    anyhow::bail!(
      "SECTR_GCP_BUILD_SA_EMAIL is required when SECTR_PLATFORM={} \
             (the Cloud Build service account builds run AS)",
      platform.as_str()
    );
  }
  // The runner SA is the CLOUD RUN runtime identity — the E2B backend
  // never ensures jobs, so it never needs one
  // (GcpPlatform::build_only). Required for gcp only.
  if platform == PlatformKind::Gcp && gcp_runner_sa_email.is_none() {
    anyhow::bail!(
      "SECTR_GCP_RUNNER_SA_EMAIL is required when SECTR_PLATFORM=gcp \
             (the least-privilege runtime identity ensured jobs run AS — \
             the runner service account)"
    );
  }
  // R4: the base image must live in the CONFIGURED region's registry —
  // a mismatched host silently builds in the wrong region or fails at
  // pull time (R6 finding).
  let Some(base) = runner_base_image else {
    anyhow::bail!(
      "SECTR_RUNNER_BASE_IMAGE is required when SECTR_PLATFORM=gcp \
             (the runner base image in AR — every deploy builds on it)"
    );
  };
  let expected_host = format!("{}-docker.pkg.dev", gcp_region.unwrap_or(""));
  let host = base.split('/').next().unwrap_or("");
  if host != expected_host {
    anyhow::bail!(
      "SECTR_RUNNER_BASE_IMAGE {base:?} is not in the configured region's \
             registry (expected host {expected_host:?} for SECTR_GCP_REGION)"
    );
  }
  Ok(())
}

impl PlatformKind {
  /// The env value the config was parsed from (for error messages).
  fn as_str(self) -> &'static str {
    match self {
      PlatformKind::Docker => "docker",
      PlatformKind::Gcp => "gcp",
      PlatformKind::E2b => "e2b",
    }
  }
}

/// `SECTR_E2B_CPU_COUNT` value: unset or unparseable falls back to 1 vCPU.
fn parse_e2b_cpu_count(raw: Option<&str>) -> i32 {
  raw.and_then(|v| v.parse().ok()).unwrap_or(1)
}

/// E2B platform keys: the account key + the puller-SA key
/// path are required; the sandbox timeout is validated against the Hobby
/// continuous-runtime cap (warn, not refuse — Pro raises it legitimately).
fn check_e2b_config(
  platform: PlatformKind,
  e2b_api_key: Option<&str>,
  e2b_puller_sa_json: Option<&str>,
  sandbox_timeout_secs: u32,
  cpu_count: i32,
  memory_mb: i32,
) -> anyhow::Result<()> {
  if platform != PlatformKind::E2b {
    return Ok(());
  }
  if e2b_api_key.is_none() {
    anyhow::bail!("SECTR_E2B_API_KEY is required when SECTR_PLATFORM=e2b");
  }
  if e2b_puller_sa_json.is_none() {
    anyhow::bail!(
      "SECTR_E2B_PULLER_SA_JSON is required when SECTR_PLATFORM=e2b \
             (path to the sectr-e2b-puller SA JSON key whose CONTENT E2B uses \
             to pull the AR image)"
    );
  }
  if sandbox_timeout_secs > 3600 {
    tracing::warn!(
      timeout = sandbox_timeout_secs,
      "SECTR_E2B_SANDBOX_TIMEOUT_SECS exceeds the Hobby plan's 1h max \
             continuous runtime — sandboxes will be killed at the plan limit \
             (upgrade to Pro for longer turns)"
    );
  }
  // Non-positive sizing/TTL would only fail at first create — reject
  // here to keep the fail-fast surface complete (E2B-GATE finding 7).
  if sandbox_timeout_secs == 0 {
    anyhow::bail!(
      "SECTR_E2B_SANDBOX_TIMEOUT_SECS must be > 0 (the API default of \
             15s would kill the sandbox before the sidecar dials in)"
    );
  }
  if cpu_count < 1 {
    anyhow::bail!("SECTR_E2B_CPU_COUNT must be >= 1 (got {cpu_count})");
  }
  if memory_mb < 128 {
    anyhow::bail!("SECTR_E2B_MEMORY_MB must be >= 128 (E2B's floor; got {memory_mb})");
  }
  Ok(())
}

/// Refuse an incoherent artifact-backend config at startup rather than on
/// first deploy. The feature-off refusal is compile-time dependent: with
/// the `gcs` feature built, only the bucket is checked.
fn check_artifact_backend(
  platform: PlatformKind,
  backend: ArtifactBackend,
  gcs_bucket: Option<&str>,
) -> anyhow::Result<()> {
  // The platform and the artifact backend are one backend by construction
  // — a `fs:` URI cannot feed a Cloud Build builder, and a
  // `gs:` URI cannot feed a local docker build. Fail fast here.
  match (platform, backend) {
    (PlatformKind::Docker, ArtifactBackend::Fs) => {}
    (PlatformKind::Gcp, ArtifactBackend::Gcs) => {}
    (PlatformKind::E2b, ArtifactBackend::Gcs) => {}
    (PlatformKind::Docker, ArtifactBackend::Gcs) => anyhow::bail!(
      "SECTR_PLATFORM=docker requires SECTR_ARTIFACT_BACKEND=fs — the \
             local docker build reads `fs:` artifact URIs"
    ),
    (PlatformKind::Gcp, ArtifactBackend::Fs) | (PlatformKind::E2b, ArtifactBackend::Fs) => {
      anyhow::bail!(
        "SECTR_PLATFORM={} requires SECTR_ARTIFACT_BACKEND=gcs — the \
                 Cloud Build builder reads `gs:` artifact URIs",
        platform.as_str()
      )
    }
  }
  if backend != ArtifactBackend::Gcs {
    return Ok(());
  }
  if cfg!(not(feature = "gcs")) {
    anyhow::bail!(
      "SECTR_ARTIFACT_BACKEND=gcs requires the sectr-server `gcs` \
             cargo feature (rebuild with: cargo build --features gcs)"
    );
  }
  if gcs_bucket.is_none() {
    anyhow::bail!("SECTR_GCS_BUCKET is required when SECTR_ARTIFACT_BACKEND=gcs");
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A freshly generated public key as the single base64 line operators
  /// put in `SECTR_CLERK_JWT_KEY` (BEGIN/END lines and newlines stripped).
  fn single_line_spki() -> String {
    crate::testkeys::primary().spki_b64.clone()
  }

  const ISSUER: &str = "https://clerk.sectr.dev";
  const PARTIES: &str = "https://app.sectr.dev, http://localhost:3000";

  #[test]
  fn clerk_unset_is_none_and_ttl_alone_does_not_enable_it() {
    assert_eq!(
      ClerkConfig::from_vars(None, None, None, None).unwrap(),
      None
    );
    assert_eq!(
      ClerkConfig::from_vars(Some(""), Some("  "), None, Some("60")).unwrap(),
      None
    );
  }

  #[test]
  fn clerk_single_line_spki_is_rewrapped_into_pem() {
    let spki = single_line_spki();
    let cfg = ClerkConfig::from_vars(Some(ISSUER), Some(&spki), Some(PARTIES), None)
      .unwrap()
      .unwrap();
    assert!(cfg.jwt_key_pem.starts_with("-----BEGIN PUBLIC KEY-----\n"));
    assert!(cfg.jwt_key_pem.ends_with("-----END PUBLIC KEY-----\n"));
    assert!(
      cfg
        .jwt_key_pem
        .lines()
        .all(|l| l.len() <= 64 || l.starts_with("-----"))
    );
    // Re-wrapping is lossless: the PEM body is the original base64.
    let body: String = cfg
      .jwt_key_pem
      .lines()
      .filter(|l| !l.starts_with("-----"))
      .collect();
    assert_eq!(body, spki);
    assert_eq!(cfg.jwt_key_pem, crate::testkeys::primary().public_pem);
    assert_eq!(cfg.issuer, ISSUER);
    assert_eq!(
      cfg.authorized_parties,
      vec!["https://app.sectr.dev", "http://localhost:3000"]
    );
    assert_eq!(cfg.session_ttl_secs, 604_800, "7-day default");
  }

  #[test]
  fn clerk_garbage_key_is_a_startup_error() {
    for bad in ["not base64 !!!", "AAAA", "aGVsbG8gd29ybGQ="] {
      let err = ClerkConfig::from_vars(Some(ISSUER), Some(bad), Some(PARTIES), None).unwrap_err();
      assert!(
        err.to_string().contains("SECTR_CLERK_JWT_KEY"),
        "{bad}: {err}"
      );
    }
  }

  #[test]
  fn clerk_is_all_or_none() {
    let spki = single_line_spki();
    for (i, k, p) in [
      (Some(ISSUER), None, None),
      (None, Some(spki.as_str()), None),
      (Some(ISSUER), Some(spki.as_str()), None),
      (None, Some(spki.as_str()), Some(PARTIES)),
    ] {
      let err = ClerkConfig::from_vars(i, k, p, None).unwrap_err();
      assert!(err.to_string().contains("partially configured"), "{err}");
    }
    assert!(ClerkConfig::from_vars(Some(ISSUER), Some(&spki), Some(" , "), None).is_err());
  }

  #[test]
  fn clerk_session_ttl_override_and_validation() {
    let spki = single_line_spki();
    let ttl = |v: &str| ClerkConfig::from_vars(Some(ISSUER), Some(&spki), Some(PARTIES), Some(v));
    assert_eq!(ttl("3600").unwrap().unwrap().session_ttl_secs, 3600);
    assert!(ttl("0").is_err());
    assert!(ttl("a week").is_err());
  }

  #[test]
  fn fs_backend_needs_nothing() {
    check_artifact_backend(PlatformKind::Docker, ArtifactBackend::Fs, None).unwrap();
    check_artifact_backend(PlatformKind::Docker, ArtifactBackend::Fs, Some("bucket")).unwrap();
  }

  #[test]
  fn gcs_backend_refuses_with_a_clear_message() {
    let e = check_artifact_backend(PlatformKind::Gcp, ArtifactBackend::Gcs, None).unwrap_err();
    if cfg!(feature = "gcs") {
      // Feature on: the complaint must be the missing bucket.
      assert!(e.to_string().contains("SECTR_GCS_BUCKET"), "{e}");
    } else {
      // Feature off: refuse with the rebuild instruction.
      assert!(e.to_string().contains("gcs` cargo feature"), "{e}");
    }
  }

  #[test]
  fn gcs_backend_with_bucket_is_accepted_when_feature_is_on() {
    let result = check_artifact_backend(
      PlatformKind::Gcp,
      ArtifactBackend::Gcs,
      Some("sectr-artifacts"),
    );
    if cfg!(feature = "gcs") {
      result.unwrap();
    } else {
      assert!(result.is_err());
    }
  }

  #[test]
  fn gcp_platform_requires_project_region_and_sas() {
    // Docker platform: the GCP keys are irrelevant.
    check_gcp_config(PlatformKind::Docker, None, None, None, None, None).unwrap();

    // Each missing key gets its own clear complaint.
    let e = check_gcp_config(
      PlatformKind::Gcp,
      None,
      Some("us-east4"),
      Some("sa@x"),
      Some("r@x"),
      Some("us-east4-docker.pkg.dev/p/r/base:v1"),
    )
    .unwrap_err();
    assert!(e.to_string().contains("SECTR_GCP_PROJECT_ID"), "{e}");
    let e = check_gcp_config(
      PlatformKind::Gcp,
      Some("p"),
      None,
      Some("sa@x"),
      Some("r@x"),
      Some("us-east4-docker.pkg.dev/p/r/base:v1"),
    )
    .unwrap_err();
    assert!(e.to_string().contains("SECTR_GCP_REGION"), "{e}");
    let e = check_gcp_config(
      PlatformKind::Gcp,
      Some("p"),
      Some("us-east4"),
      None,
      Some("r@x"),
      Some("us-east4-docker.pkg.dev/p/r/base:v1"),
    )
    .unwrap_err();
    assert!(e.to_string().contains("SECTR_GCP_BUILD_SA_EMAIL"), "{e}");
    let e = check_gcp_config(
      PlatformKind::Gcp,
      Some("p"),
      Some("us-east4"),
      Some("sa@x"),
      None,
      Some("us-east4-docker.pkg.dev/p/r/base:v1"),
    )
    .unwrap_err();
    assert!(e.to_string().contains("SECTR_GCP_RUNNER_SA_EMAIL"), "{e}");
    let e = check_gcp_config(
      PlatformKind::Gcp,
      Some("p"),
      Some("us-east4"),
      Some("sa@x"),
      Some("r@x"),
      None,
    )
    .unwrap_err();
    assert!(e.to_string().contains("SECTR_RUNNER_BASE_IMAGE"), "{e}");
    let e = check_gcp_config(
      PlatformKind::Gcp,
      Some("p"),
      Some("us-east4"),
      Some("sa@x"),
      Some("r@x"),
      Some("us-central1-docker.pkg.dev/p/r/base:v1"),
    )
    .unwrap_err();
    assert!(
      e.to_string()
        .contains("not in the configured region's registry"),
      "{e}"
    );

    // All present: accepted.
    check_gcp_config(
      PlatformKind::Gcp,
      Some("p"),
      Some("us-east4"),
      Some("sa@x"),
      Some("r@x"),
      Some("us-east4-docker.pkg.dev/p/r/base:v1"),
    )
    .unwrap();
  }

  #[test]
  fn e2b_platform_requires_api_key_and_puller_sa_but_not_the_runner_sa() {
    // Docker platform: the E2B keys are irrelevant.
    check_e2b_config(PlatformKind::Docker, None, None, 3600, 2, 1024).unwrap();

    // Each missing key gets its own clear complaint.
    let e = check_e2b_config(PlatformKind::E2b, None, Some("/sa.json"), 3600, 2, 1024).unwrap_err();
    assert!(e.to_string().contains("SECTR_E2B_API_KEY"), "{e}");
    let e = check_e2b_config(PlatformKind::E2b, Some("key"), None, 3600, 2, 1024).unwrap_err();
    assert!(e.to_string().contains("SECTR_E2B_PULLER_SA_JSON"), "{e}");

    // Both present: accepted — and the runner SA is NOT required for
    // e2b (the Cloud Run runtime identity is gcp-specific; the E2B
    // backend never ensures jobs).
    check_e2b_config(
      PlatformKind::E2b,
      Some("key"),
      Some("/sa.json"),
      3600,
      2,
      1024,
    )
    .unwrap();
    check_gcp_config(
      PlatformKind::E2b,
      Some("p"),
      Some("us-east4"),
      Some("sa@x"),
      None,
      Some("us-east4-docker.pkg.dev/p/r/base:v1"),
    )
    .unwrap();
  }

  #[test]
  fn e2b_rejects_non_positive_sizing() {
    let e = check_e2b_config(PlatformKind::E2b, Some("k"), Some("/s"), 0, 2, 1024).unwrap_err();
    assert!(
      e.to_string().contains("SECTR_E2B_SANDBOX_TIMEOUT_SECS"),
      "{e}"
    );
    let e = check_e2b_config(PlatformKind::E2b, Some("k"), Some("/s"), 3600, 0, 1024).unwrap_err();
    assert!(e.to_string().contains("SECTR_E2B_CPU_COUNT"), "{e}");
    let e = check_e2b_config(PlatformKind::E2b, Some("k"), Some("/s"), 3600, 2, 64).unwrap_err();
    assert!(e.to_string().contains("SECTR_E2B_MEMORY_MB"), "{e}");
  }

  #[test]
  fn e2b_cpu_count_defaults_to_one() {
    assert_eq!(parse_e2b_cpu_count(None), 1);
    assert_eq!(parse_e2b_cpu_count(Some("2")), 2);
    assert_eq!(parse_e2b_cpu_count(Some("not-a-number")), 1);
  }

  #[test]
  fn e2b_builds_on_gcp_and_pairs_with_gcs_only() {
    assert!(PlatformKind::E2b.builds_on_gcp());
    assert!(PlatformKind::Gcp.builds_on_gcp());
    assert!(!PlatformKind::Docker.builds_on_gcp());
    let e = check_artifact_backend(PlatformKind::E2b, ArtifactBackend::Fs, Some("b")).unwrap_err();
    assert!(e.to_string().contains("SECTR_PLATFORM=e2b"), "{e}");
    if cfg!(feature = "gcs") {
      check_artifact_backend(PlatformKind::E2b, ArtifactBackend::Gcs, Some("b")).unwrap();
    }
  }

  #[test]
  fn platform_env_parses_e2b() {
    // from_env is private; the enum mapping is pinned via as_str.
    assert_eq!(PlatformKind::E2b.as_str(), "e2b");
  }
}
