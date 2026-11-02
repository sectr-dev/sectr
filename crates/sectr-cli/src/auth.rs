//! `sectr auth` — platform-key storage + the shared resolution
//! every cloud command goes through.
//!
//! Storage: `~/.config/sectr/credentials.toml` (0600), written by
//! `sectr auth login` after the key validates against the server:
//!
//! ```toml
//! api = "https://api.sectr.dev"
//! platform_key = "skp_…"
//! key_id = "…"   # only for keys minted by the browser login
//! ```
//!
//! Resolution (`resolve`) — one precedence rule for ALL cloud commands
//! (deploy, secrets, send --cloud, tail --cloud):
//! explicit flag > `SECTR_PLATFORM_KEY` / `SECTR_API_URL` env > the
//! credentials file. Nothing here prints or logs the key.

use std::io::Write as _;
use std::path::PathBuf;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credentials {
  /// The server base the key belongs to — stored so later commands
  /// (`sectr deploy` with no `--api`) talk to the same server.
  pub api: String,
  pub platform_key: String,
  /// The id of a key minted by the browser login, so `sectr auth logout`
  /// can revoke it. Absent for pasted keys (those are only forgotten).
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub key_id: Option<String>,
}

fn credentials_path() -> anyhow::Result<PathBuf> {
  let base = match std::env::var("XDG_CONFIG_HOME") {
    Ok(dir) => PathBuf::from(dir),
    Err(_) => {
      let home = std::env::var("HOME").context("no HOME — cannot locate credentials")?;
      PathBuf::from(home).join(".config")
    }
  };
  Ok(base.join("sectr").join("credentials.toml"))
}

/// Load stored credentials, if any (missing file = None; malformed file
/// is an error — the user should see it, not silently fall through).
pub fn load() -> anyhow::Result<Option<Credentials>> {
  let path = credentials_path()?;
  let raw = match std::fs::read_to_string(&path) {
    Ok(raw) => raw,
    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
    Err(e) => return Err(anyhow::anyhow!("read {}: {e}", path.display())),
  };
  let creds: Credentials =
    toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
  Ok(Some(creds))
}

/// Persist (creating parent dirs; 0600 on unix — the file holds a live key).
pub fn store(creds: &Credentials) -> anyhow::Result<()> {
  let path = credentials_path()?;
  if let Some(dir) = path.parent() {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
  }
  let body = toml::to_string_pretty(creds).context("serialize credentials")?;
  let mut file = {
    #[cfg(unix)]
    {
      use std::os::unix::fs::OpenOptionsExt;
      std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
    }
    #[cfg(not(unix))]
    std::fs::OpenOptions::new()
      .write(true)
      .create(true)
      .truncate(true)
      .open(&path)
  }
  .with_context(|| format!("open {}", path.display()))?;
  file.write_all(body.as_bytes())?;
  Ok(())
}

pub fn clear() -> anyhow::Result<bool> {
  let path = credentials_path()?;
  match std::fs::remove_file(&path) {
    Ok(()) => Ok(true),
    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
    Err(e) => Err(anyhow::anyhow!("remove {}: {e}", path.display())),
  }
}

/// The resolved target of a cloud command: where to talk, and as whom.
pub struct Target {
  pub api: String,
  pub key: String,
}

/// One precedence rule for every cloud command. `api`/`key` are the
/// explicit flags (already extracted from args); the env and the
/// credentials file fill the gaps. Errors name the fix ("sectr auth login").
pub fn resolve(api: Option<&str>, key: Option<&str>) -> anyhow::Result<Target> {
  let api = api
    .map(str::to_owned)
    .or_else(|| std::env::var("SECTR_API_URL").ok());
  let key = key
    .map(str::to_owned)
    .or_else(|| std::env::var("SECTR_PLATFORM_KEY").ok());

  let stored = load()?;
  let api = match api {
    Some(a) => a,
    None => match &stored {
      Some(c) => c.api.clone(),
      None => {
        bail!("no server address: pass --api, set SECTR_API_URL, or run `sectr auth login`")
      }
    },
  };
  let key = match key {
    Some(k) => k,
    None => match &stored {
      Some(c) => c.platform_key.clone(),
      None => {
        bail!("no platform key: pass --key, set SECTR_PLATFORM_KEY, or run `sectr auth login`")
      }
    },
  };
  Ok(Target {
    api: api.trim_end_matches('/').to_owned(),
    key,
  })
}

/// `GET /me` — the key-validation probe (login + whoami). Returns the
/// org name + id.
pub async fn whoami(api: &str, key: &str) -> anyhow::Result<(String, String)> {
  let url = format!("{}/me", api.trim_end_matches('/'));
  let resp = reqwest::Client::new()
    .get(&url)
    .bearer_auth(key)
    .send()
    .await
    .map_err(|e| anyhow::anyhow!("reach {url}: {e}"))?;
  match resp.status() {
    reqwest::StatusCode::UNAUTHORIZED => bail!("the server rejected that key (401)"),
    s if !s.is_success() => bail!("the server answered {s}"),
    _ => {}
  }
  let body: serde_json::Value = resp.json().await?;
  let org = body.get("org").context("server /me response has no org")?;
  let name = org
    .get("name")
    .and_then(|v| v.as_str())
    .unwrap_or("unknown")
    .to_owned();
  let id = org
    .get("id")
    .and_then(|v| v.as_str())
    .unwrap_or("?")
    .to_owned();
  Ok((name, id))
}
