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
//! dashboard = "https://app.sectr.dev"   # the server's dashboard, for `sectr dev`
//! ```
//!
//! Resolution (`resolve`) — one precedence rule for ALL cloud commands
//! (deploy, secrets, send --cloud, tail --cloud):
//! explicit flag > `SECTR_PLATFORM_KEY` / `SECTR_API_URL` env > the
//! credentials file. The saved key only ever goes to the saved server.
//! Nothing here prints or logs the key.

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
  /// The server's dashboard origin, saved at login so `sectr dev` can open
  /// it without reaching the server.
  pub dashboard: String,
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
/// is an error — the user should see it, not silently fall through). Files
/// from before `dashboard` was saved are malformed too: logging in again
/// replaces them.
pub fn load() -> anyhow::Result<Option<Credentials>> {
  let path = credentials_path()?;
  let raw = match std::fs::read_to_string(&path) {
    Ok(raw) => raw,
    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
    Err(e) => return Err(anyhow::anyhow!("read {}: {e}", path.display())),
  };
  let creds: Credentials = toml::from_str(&raw).with_context(|| {
    format!(
      "parse {} (run `sectr auth login` to replace it)",
      path.display()
    )
  })?;
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

/// A server URL's identity for comparison: trailing slashes don't count.
fn normalize_api(api: &str) -> &str {
  api.trim_end_matches('/')
}

/// An env value, with an empty one counting as unset.
fn env_value(name: &str) -> Option<String> {
  std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// The one precedence rule, from explicit values. An explicit key (flag or
/// env) is used as given. The saved key is used only for the saved server:
/// sending it anywhere else would hand it to that server.
fn resolve_target(
  flag_api: Option<&str>,
  flag_key: Option<&str>,
  env_api: Option<String>,
  env_key: Option<String>,
  stored: Option<&Credentials>,
) -> anyhow::Result<Target> {
  let api = flag_api
    .map(str::to_owned)
    .or(env_api)
    .or_else(|| stored.map(|c| c.api.clone()));
  let Some(api) = api else {
    bail!("no server address: pass --api, set SECTR_API_URL, or run `sectr auth login`");
  };
  let api = normalize_api(&api).to_owned();
  let key = match flag_key.map(str::to_owned).or(env_key) {
    Some(key) => key,
    None => match stored {
      Some(c) if normalize_api(&c.api) == api => c.platform_key.clone(),
      Some(c) => bail!(
        "your saved key belongs to {}, not {api}, so it wasn't sent: run `sectr auth login --api \
         {api}`, or pass --key",
        normalize_api(&c.api)
      ),
      None => {
        bail!("no platform key: pass --key, set SECTR_PLATFORM_KEY, or run `sectr auth login`")
      }
    },
  };
  Ok(Target { api, key })
}

/// [`resolve_target`] from the environment and the credentials file.
/// Doesn't print anything.
pub fn resolve_quietly(api: Option<&str>, key: Option<&str>) -> anyhow::Result<Target> {
  let stored = load()?;
  resolve_target(
    api,
    key,
    env_value("SECTR_API_URL"),
    env_value("SECTR_PLATFORM_KEY"),
    stored.as_ref(),
  )
}

/// One precedence rule for every cloud command. `api`/`key` are the
/// explicit flags (already extracted from args); the env and the
/// credentials file fill the gaps. Errors name the fix ("sectr auth login").
/// Prints which server the command is talking to, since that now depends on
/// the login.
pub fn resolve(api: Option<&str>, key: Option<&str>) -> anyhow::Result<Target> {
  let target = resolve_quietly(api, key)?;
  eprintln!("{}", server_line(&target.api));
  Ok(target)
}

fn server_line(api: &str) -> String {
  format!("server: {api}")
}

/// What `GET /me` says about a key's identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Me {
  pub org_name: String,
  pub org_id: String,
  /// The server's dashboard origin, without a trailing slash.
  pub dashboard_url: String,
}

/// `GET /me` — the key-validation probe (login + whoami).
pub async fn whoami(api: &str, key: &str) -> anyhow::Result<Me> {
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
  let body = resp.bytes().await?;
  let me: sectr_common::api::Me = serde_json::from_slice(&body)
    .map_err(|e| anyhow::anyhow!("the server's /me response isn't one: {e}"))?;
  let dashboard_url = me.dashboard_url.trim_end_matches('/').to_owned();
  if dashboard_url.is_empty() {
    bail!("server /me response has no dashboard_url");
  }
  Ok(Me {
    org_name: me.org.name,
    org_id: me.org.id.to_string(),
    dashboard_url,
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  fn saved(api: &str) -> Credentials {
    Credentials {
      api: api.to_owned(),
      platform_key: "skp_saved".to_owned(),
      key_id: None,
      dashboard: "https://sectr.example.com".to_owned(),
    }
  }

  const SAVED_SERVER: &str = "https://sectr.example.com";

  #[test]
  fn the_saved_login_is_used_when_nothing_else_is_given() {
    let stored = saved(SAVED_SERVER);
    let target = resolve_target(None, None, None, None, Some(&stored)).unwrap();
    assert_eq!(target.api, SAVED_SERVER);
    assert_eq!(target.key, "skp_saved");
  }

  #[test]
  fn the_saved_key_goes_to_its_own_server_whatever_the_slashes() {
    let stored = saved("https://sectr.example.com/");
    let target = resolve_target(
      Some("https://sectr.example.com"),
      None,
      None,
      None,
      Some(&stored),
    )
    .unwrap();
    assert_eq!(target.key, "skp_saved");
    let stored = saved(SAVED_SERVER);
    let target = resolve_target(
      None,
      None,
      Some("https://sectr.example.com//".into()),
      None,
      Some(&stored),
    )
    .unwrap();
    assert_eq!(target.api, SAVED_SERVER);
    assert_eq!(target.key, "skp_saved");
  }

  #[test]
  fn the_saved_key_is_never_sent_to_another_server() {
    let stored = saved(SAVED_SERVER);
    let other = "https://other.example.com";
    for (flag, env) in [(Some(other), None), (None, Some(other.to_owned()))] {
      let err = resolve_target(flag, None, env, None, Some(&stored))
        .err()
        .expect("must refuse")
        .to_string();
      assert!(
        err.contains(&format!("sectr auth login --api {other}")),
        "{err}"
      );
      assert!(!err.contains("skp_saved"), "{err}");
    }
  }

  #[test]
  fn an_explicit_key_is_used_as_given_with_any_server() {
    let stored = saved(SAVED_SERVER);
    let other = "https://other.example.com";
    let target = resolve_target(Some(other), Some("skp_flag"), None, None, Some(&stored)).unwrap();
    assert_eq!(
      (target.api.as_str(), target.key.as_str()),
      (other, "skp_flag")
    );
    let target = resolve_target(
      Some(other),
      None,
      None,
      Some("skp_env".into()),
      Some(&stored),
    )
    .unwrap();
    assert_eq!(target.key, "skp_env");
  }

  #[test]
  fn flags_beat_env_and_env_beats_the_saved_login() {
    let stored = saved(SAVED_SERVER);
    let target = resolve_target(
      Some("https://flag.example.com/"),
      Some("skp_flag"),
      Some("https://env.example.com".into()),
      Some("skp_env".into()),
      Some(&stored),
    )
    .unwrap();
    assert_eq!(target.api, "https://flag.example.com");
    assert_eq!(target.key, "skp_flag");
    let target = resolve_target(
      None,
      None,
      Some("https://env.example.com".into()),
      Some("skp_env".into()),
      Some(&stored),
    )
    .unwrap();
    assert_eq!(target.api, "https://env.example.com");
    assert_eq!(target.key, "skp_env");
  }

  #[test]
  fn missing_pieces_name_the_fix() {
    let err = resolve_target(None, Some("skp_x"), None, None, None)
      .err()
      .unwrap()
      .to_string();
    assert!(err.contains("no server address"), "{err}");
    let err = resolve_target(Some(SAVED_SERVER), None, None, None, None)
      .err()
      .unwrap()
      .to_string();
    assert!(err.contains("no platform key"), "{err}");
  }

  #[test]
  fn cloud_commands_name_their_server() {
    assert_eq!(
      server_line("https://sectr.example.com"),
      "server: https://sectr.example.com"
    );
  }

  #[test]
  fn credentials_round_trip_and_need_the_dashboard() {
    let creds = saved(SAVED_SERVER);
    let round_tripped: Credentials =
      toml::from_str(&toml::to_string_pretty(&creds).unwrap()).unwrap();
    assert_eq!(round_tripped.dashboard, "https://sectr.example.com");
    let without_dashboard =
      toml::from_str::<Credentials>("api = \"https://api.sectr.dev\"\nplatform_key = \"skp_a\"\n");
    assert!(without_dashboard.is_err());
  }

  #[tokio::test]
  async fn me_must_name_the_dashboard() {
    let app = axum::Router::new().route(
      "/me",
      axum::routing::get(|| async {
        axum::Json(serde_json::json!({"org": {"id": "0190d8a4-0000-7000-8000-000000000002", "name": "acme"}}))
      }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let err = whoami(&api, "skp_x").await.unwrap_err().to_string();
    assert!(err.contains("dashboard_url"), "{err}");
  }
}
