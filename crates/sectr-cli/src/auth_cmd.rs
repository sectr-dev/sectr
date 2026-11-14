//! `sectr auth` — sign in once so later cloud commands
//! (`sectr deploy`, `sectr secrets`, `sectr send --cloud`,
//! `sectr tail --cloud`) need no flags.
//!
//! - `sectr auth login` — opens the dashboard in your browser, waits for
//!   you to approve the request there, and stores the resulting platform
//!   key (named `CLI (<hostname>)` in Settings). Works over SSH and in
//!   containers: the browser only has to reach the dashboard.
//!   `--no-browser` prints the link without opening it.
//! - `sectr auth login --key skp_…` — validate and store a key you created
//!   in Settings. `--with-key` reads one from a hidden prompt or stdin
//!   (for scripts: `echo $KEY | sectr auth login --with-key`).
//!   The server comes from `--api`, then `SECTR_API_URL`, then the saved
//!   login, then the public endpoint; pass `--api http://localhost:8080`
//!   the first time for a local server.
//! - `sectr auth whoami` — the stored identity (org + server).
//! - `sectr auth logout` — forget the key; a key obtained through the
//!   browser login is also revoked.

use std::io::{IsTerminal, Read as _, Write as _};
use std::time::Duration;

use anyhow::{Context, bail};
use clap::{Args, Subcommand};
use sectr_common::api::cli_login::{
  ApprovedLogin, DeviceGrant, PollAnswer, PollRequest, StartRequest,
};

use crate::auth::{self, Credentials};

#[derive(Args, Debug)]
pub struct AuthArgs {
  #[command(subcommand)]
  pub command: AuthCommand,
  /// The server base URL. Default: SECTR_API_URL, then the saved login's
  /// server, then the public endpoint.
  #[arg(long, global = true)]
  pub api: Option<String>,
  /// Platform API key (`skp_…`). With `login`, stores that key instead of
  /// opening the browser.
  #[arg(long, global = true)]
  pub key: Option<String>,
}

#[derive(Subcommand, Debug)]
pub enum AuthCommand {
  /// Sign in: opens the dashboard in your browser to approve the request
  /// (or pass --key / --with-key to store an API key instead).
  Login {
    /// Read the platform key from a hidden prompt or stdin instead of
    /// using the browser.
    #[arg(long)]
    with_key: bool,
    /// Print the approval link without trying to open a browser (remote
    /// machines, coding agents).
    #[arg(long)]
    no_browser: bool,
  },
  /// Print the stored identity (org + server) after re-validating.
  Whoami,
  /// Forget the stored key (and revoke it, if the browser login made it).
  Logout,
}

const DEFAULT_API: &str = "https://api.sectr.dev";

pub async fn run_auth(args: AuthArgs) -> anyhow::Result<()> {
  match args.command {
    AuthCommand::Login {
      with_key,
      no_browser,
    } => run_login(&args, with_key, no_browser).await,
    AuthCommand::Whoami => run_whoami(&args).await,
    AuthCommand::Logout => run_logout().await,
  }
}

/// Which server `sectr auth login` talks to. A broken credentials file
/// doesn't block logging in again, which is how it gets replaced.
fn login_server(flag: Option<&str>, env: Option<&str>, saved: Option<&Credentials>) -> String {
  flag
    .or(env.filter(|v| !v.trim().is_empty()))
    .or(saved.map(|c| c.api.as_str()))
    .unwrap_or(DEFAULT_API)
    .trim_end_matches('/')
    .to_owned()
}

async fn run_login(args: &AuthArgs, with_key: bool, no_browser: bool) -> anyhow::Result<()> {
  let saved = auth::load().ok().flatten();
  let api = login_server(
    args.api.as_deref(),
    std::env::var("SECTR_API_URL").ok().as_deref(),
    saved.as_ref(),
  );

  // Manual route: --key, or --with-key (hidden prompt / stdin).
  let key = match (&args.key, with_key) {
    (Some(k), _) => Some(k.clone()),
    (None, true) => Some(read_key()?),
    (None, false) => None,
  };
  let Some(key) = key else {
    let approved = device_login(&api, !no_browser).await?;
    let me = auth::whoami(&api, &approved.key)
      .await
      .map_err(|e| anyhow::anyhow!("approved, but reading the server's details failed: {e:#}"))?;
    auth::store(&Credentials {
      api: api.clone(),
      platform_key: approved.key,
      key_id: Some(approved.key_id.to_string()),
      dashboard: me.dashboard_url,
    })?;
    eprintln!(
      "logged in to {api} — org \"{}\" (stored in ~/.config/sectr/credentials.toml)",
      approved.org.name
    );
    return Ok(());
  };

  if key.trim().is_empty() {
    bail!("empty key");
  }
  let me = auth::whoami(&api, key.trim())
    .await
    .map_err(|e| anyhow::anyhow!("{e:#}"))?;
  auth::store(&Credentials {
    api: api.clone(),
    platform_key: key.trim().to_owned(),
    key_id: None,
    dashboard: me.dashboard_url,
  })?;
  eprintln!(
    "logged in to {api} — org \"{}\" (stored in ~/.config/sectr/credentials.toml)",
    me.org_name
  );
  Ok(())
}

/// `--with-key`: a hidden prompt on a terminal, else the first line of stdin.
fn read_key() -> anyhow::Result<String> {
  if std::io::stdin().is_terminal() {
    eprint!("platform key: ");
    std::io::stderr().flush().context("prompt")?;
    return rpassword::read_password().context("prompt");
  }
  let mut input = String::new();
  std::io::stdin()
    .read_to_string(&mut input)
    .context("read key from stdin")?;
  Ok(input.lines().next().unwrap_or("").to_owned())
}

// ---- browser (device) login ----------------------------------------------

/// This machine's name, shown on the approval page and used to name the
/// minted key (`CLI (<hostname>)`).
fn hostname() -> String {
  #[cfg(unix)]
  {
    let mut buf = [0u8; 256];
    // SAFETY: `buf` is a valid writable buffer of the length passed.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc == 0 {
      let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
      let name = String::from_utf8_lossy(&buf[..end]).trim().to_owned();
      if !name.is_empty() {
        return name;
      }
    }
  }
  #[cfg(windows)]
  if let Ok(name) = std::env::var("COMPUTERNAME")
    && !name.trim().is_empty()
  {
    return name;
  }
  "unknown machine".into()
}

async fn device_login(api: &str, open: bool) -> anyhow::Result<ApprovedLogin> {
  let client = reqwest::Client::new();
  let grant = start_device_login(&client, api, &hostname()).await?;

  eprintln!("To sign in, open this page and approve the request:");
  eprintln!("  {}", grant.verification_url_complete);
  eprintln!("Code: {} (check that it matches the page)", grant.user_code);
  eprintln!("Waiting for approval… (Ctrl-C to cancel)");
  if open {
    crate::browser::open_browser(&grant.verification_url_complete);
  }

  poll_device_login(&client, api, &grant, Duration::from_secs(1)).await
}

async fn start_device_login(
  client: &reqwest::Client,
  api: &str,
  client_name: &str,
) -> anyhow::Result<DeviceGrant> {
  let url = format!("{api}/auth/cli");
  let resp = client
    .post(&url)
    .json(&StartRequest {
      client_name: client_name.to_owned(),
    })
    .send()
    .await
    .map_err(|e| anyhow::anyhow!("reach {url}: {e}"))?;
  let status = resp.status();
  if matches!(status.as_u16(), 404 | 503) {
    bail!(
      "this server doesn't support browser login ({status}); use `sectr auth login --key skp_…` \
       or `--with-key` with a key from Settings → API keys"
    );
  }
  if !status.is_success() {
    bail!("the server answered {status} to the login request");
  }
  resp
    .json()
    .await
    .context("parse the login request response")
}

/// Poll until approved, denied or expired. Transient failures (network
/// errors, 5xx) are retried at the same interval until the request's own
/// expiry. `min_interval` floors the server's `interval` (tests pass zero).
async fn poll_device_login(
  client: &reqwest::Client,
  api: &str,
  grant: &DeviceGrant,
  min_interval: Duration,
) -> anyhow::Result<ApprovedLogin> {
  let url = format!("{api}/auth/cli/poll");
  let interval = Duration::from_secs(grant.interval).max(min_interval);
  let deadline = tokio::time::Instant::now() + Duration::from_secs(grant.expires_in);
  while tokio::time::Instant::now() < deadline {
    tokio::time::sleep(interval).await;
    let resp = client
      .post(&url)
      .json(&PollRequest {
        device_code: grant.device_code.clone(),
      })
      .send()
      .await;
    let Ok(resp) = resp else { continue };
    if !resp.status().is_success() {
      continue;
    }
    let Ok(body) = resp.bytes().await else {
      continue;
    };
    let answer = match serde_json::from_slice::<PollAnswer>(&body) {
      Ok(answer) => answer,
      // An approval that doesn't parse has consumed the request: say so
      // rather than poll on to the expiry.
      Err(e) if says_approved(&body) => {
        return Err(e).context("parse the approved login response");
      }
      // Anything else (a proxy's page, say) is retried like a 5xx.
      Err(_) => continue,
    };
    match answer {
      PollAnswer::Approved(approved) => return Ok(approved),
      PollAnswer::Denied => bail!("the login request was denied in the browser"),
      PollAnswer::Expired => bail!("the login request expired; run `sectr auth login` again"),
      PollAnswer::Pending | PollAnswer::Unknown => {}
    }
  }
  bail!("the login request expired; run `sectr auth login` again")
}

/// Does a poll answer that didn't parse claim `"status": "approved"`?
fn says_approved(body: &[u8]) -> bool {
  #[derive(serde::Deserialize)]
  struct Status {
    status: String,
  }
  serde_json::from_slice::<Status>(body).is_ok_and(|s| s.status == "approved")
}

// ---- logout ----------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
enum Revoked {
  /// Pasted key (no `key_id`): nothing to revoke.
  NotOurs,
  Yes,
  /// The server no longer knew the key (401/404): already revoked.
  AlreadyGone,
  /// Best effort failed; the key may still be live.
  Failed,
}

async fn revoke_login_key(creds: &Credentials) -> Revoked {
  let Some(key_id) = &creds.key_id else {
    return Revoked::NotOurs;
  };
  let url = format!("{}/api-keys/{key_id}", creds.api.trim_end_matches('/'));
  let client = match reqwest::Client::builder()
    .timeout(Duration::from_secs(5))
    .build()
  {
    Ok(c) => c,
    Err(_) => return Revoked::Failed,
  };
  match client
    .delete(&url)
    .bearer_auth(&creds.platform_key)
    .send()
    .await
  {
    Ok(r) if r.status().is_success() => Revoked::Yes,
    Ok(r) if matches!(r.status().as_u16(), 401 | 404) => Revoked::AlreadyGone,
    _ => Revoked::Failed,
  }
}

async fn run_logout() -> anyhow::Result<()> {
  let revoked = match auth::load() {
    Ok(Some(creds)) => revoke_login_key(&creds).await,
    _ => Revoked::NotOurs,
  };
  let removed = auth::clear()?;
  if !removed {
    eprintln!("no stored credentials");
    return Ok(());
  }
  match revoked {
    Revoked::Yes | Revoked::AlreadyGone => eprintln!("logged out; the CLI key was revoked"),
    Revoked::Failed => eprintln!(
      "credentials forgotten, but the CLI key could not be revoked — remove it in Settings → API keys"
    ),
    Revoked::NotOurs => eprintln!("credentials forgotten (the key itself was not revoked)"),
  }
  Ok(())
}

async fn run_whoami(args: &AuthArgs) -> anyhow::Result<()> {
  // Explicit flags and env override the stored identity (so
  // `sectr auth whoami --key skp_new` previews a different identity). The
  // saved key still only goes to its own server.
  let target = auth::resolve_quietly(args.api.as_deref(), args.key.as_deref())?;
  let me = auth::whoami(&target.api, &target.key).await?;
  println!("server: {}", target.api);
  println!("org:    {} ({})", me.org_name, me.org_id);
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use axum::Router;
  use axum::extract::State;
  use axum::http::StatusCode;
  use axum::routing::{delete, post};
  use serde_json::{Value, json};
  use std::sync::{Arc, Mutex};

  /// A stub server whose poll endpoint replays `script` (last entry repeats).
  async fn stub(script: Vec<(u16, Value)>) -> (String, Arc<Mutex<usize>>) {
    #[derive(Clone)]
    struct S(Arc<Vec<(u16, Value)>>, Arc<Mutex<usize>>);
    async fn poll(State(s): State<S>) -> (StatusCode, axum::Json<Value>) {
      let mut n = s.1.lock().unwrap();
      let (code, body) = s.0[(*n).min(s.0.len() - 1)].clone();
      *n += 1;
      (StatusCode::from_u16(code).unwrap(), axum::Json(body))
    }
    let calls = Arc::new(Mutex::new(0));
    let app = Router::new()
      .route("/auth/cli/poll", post(poll))
      .with_state(S(Arc::new(script), calls.clone()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), calls)
  }

  fn grant(expires_in: u64) -> DeviceGrant {
    DeviceGrant {
      device_code: "dev".into(),
      user_code: "BBBB-CCCC".into(),
      verification_url: "https://app.sectr.dev/cli".into(),
      verification_url_complete: "https://app.sectr.dev/cli/BBBB-CCCC".into(),
      expires_in,
      interval: 0,
    }
  }

  fn approved_body() -> Value {
    json!({"status": "approved", "key": "skp_x", "key_id": KEY_ID,
           "org": {"id": "0190d8a4-0000-7000-8000-000000000002", "name": "acme"}})
  }

  const KEY_ID: &str = "0190d8a4-0000-7000-8000-000000000001";

  async fn poll(api: &str, expires_in: u64) -> anyhow::Result<ApprovedLogin> {
    poll_device_login(
      &reqwest::Client::new(),
      api,
      &grant(expires_in),
      Duration::ZERO,
    )
    .await
  }

  #[tokio::test]
  async fn poll_waits_through_pending_then_returns_the_key() {
    let (api, calls) = stub(vec![
      (200, json!({"status": "pending"})),
      (200, json!({"status": "pending"})),
      (200, approved_body()),
    ])
    .await;
    let approved = poll(&api, 30).await.unwrap();
    assert_eq!(
      (
        approved.key.as_str(),
        approved.key_id.to_string().as_str(),
        approved.org.name.as_str()
      ),
      ("skp_x", KEY_ID, "acme")
    );
    assert_eq!(*calls.lock().unwrap(), 3);
  }

  #[tokio::test]
  async fn poll_survives_a_transient_502() {
    let (api, _) = stub(vec![(502, json!({})), (200, approved_body())]).await;
    assert_eq!(poll(&api, 30).await.unwrap().key, "skp_x");
  }

  #[tokio::test]
  async fn poll_retries_a_200_that_isnt_a_login_answer() {
    let (api, calls) = stub(vec![
      (200, json!("<html>a proxy's page</html>")),
      (200, json!({})),
      (200, approved_body()),
    ])
    .await;
    assert_eq!(poll(&api, 30).await.unwrap().key, "skp_x");
    assert_eq!(*calls.lock().unwrap(), 3);
  }

  #[tokio::test]
  async fn poll_fails_on_an_approval_it_cant_read() {
    // The request is consumed: polling on would only end in "expired".
    let (api, calls) = stub(vec![(200, json!({"status": "approved", "key": "skp_x"}))]).await;
    let err = format!("{:#}", poll(&api, 30).await.unwrap_err());
    assert!(err.contains("approved login response"), "{err}");
    assert_eq!(*calls.lock().unwrap(), 1);
  }

  #[tokio::test]
  async fn poll_reports_denied_and_expired() {
    let (api, _) = stub(vec![(200, json!({"status": "denied"}))]).await;
    let err = poll(&api, 30).await.unwrap_err().to_string();
    assert!(err.contains("denied"), "{err}");
    let (api, _) = stub(vec![(200, json!({"status": "expired"}))]).await;
    let err = poll(&api, 30).await.unwrap_err().to_string();
    assert!(err.contains("expired"), "{err}");
  }

  #[tokio::test]
  async fn poll_gives_up_at_the_requests_own_expiry() {
    let (api, _) = stub(vec![(200, json!({"status": "pending"}))]).await;
    let err = poll(&api, 0).await.unwrap_err().to_string();
    assert!(err.contains("expired"), "{err}");
  }

  #[test]
  fn login_uses_the_flag_then_env_then_the_saved_server_then_hosted() {
    let saved = Credentials {
      api: "https://saved.example.com/".into(),
      platform_key: "skp_a".into(),
      key_id: None,
      dashboard: "https://saved.example.com".into(),
    };
    let flag = Some("https://flag.example.com/");
    let env = Some("https://env.example.com");
    assert_eq!(
      login_server(flag, env, Some(&saved)),
      "https://flag.example.com"
    );
    assert_eq!(
      login_server(None, env, Some(&saved)),
      "https://env.example.com"
    );
    assert_eq!(
      login_server(None, None, Some(&saved)),
      "https://saved.example.com"
    );
    assert_eq!(login_server(None, None, None), DEFAULT_API);
    assert_eq!(
      login_server(None, Some(" "), None),
      DEFAULT_API,
      "an empty env value counts as unset"
    );
  }

  #[test]
  fn credentials_without_key_id_parse_and_round_trip() {
    // Pasted keys have no key_id.
    let old: Credentials = toml::from_str(
      "api = \"https://api.sectr.dev\"\nplatform_key = \"skp_a\"\ndashboard = \"https://app.sectr.dev\"\n",
    )
    .unwrap();
    assert_eq!(old.key_id, None);
    assert!(!toml::to_string_pretty(&old).unwrap().contains("key_id"));
    let new = Credentials {
      key_id: Some("k".into()),
      ..old
    };
    assert!(
      toml::to_string_pretty(&new)
        .unwrap()
        .contains("key_id = \"k\"")
    );
  }

  #[tokio::test]
  async fn logout_revokes_only_keys_the_browser_login_made() {
    let hits = Arc::new(Mutex::new(Vec::<String>::new()));
    let h = hits.clone();
    let app = Router::new().route(
      "/api-keys/{id}",
      delete(
        move |axum::extract::Path(id): axum::extract::Path<String>| {
          let h = h.clone();
          async move {
            h.lock().unwrap().push(id.clone());
            if id == "gone" {
              StatusCode::NOT_FOUND
            } else {
              StatusCode::NO_CONTENT
            }
          }
        },
      ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let creds = |key_id: Option<&str>| Credentials {
      api: api.clone(),
      platform_key: "skp_x".into(),
      key_id: key_id.map(str::to_owned),
      dashboard: "https://app.sectr.test".into(),
    };
    assert_eq!(revoke_login_key(&creds(None)).await, Revoked::NotOurs);
    assert!(
      hits.lock().unwrap().is_empty(),
      "pasted keys send no DELETE"
    );
    assert_eq!(revoke_login_key(&creds(Some("k1"))).await, Revoked::Yes);
    assert_eq!(*hits.lock().unwrap(), vec!["k1".to_owned()]);
    assert_eq!(
      revoke_login_key(&creds(Some("gone"))).await,
      Revoked::AlreadyGone
    );
    let dead = Credentials {
      api: "http://127.0.0.1:1".into(),
      ..creds(Some("k"))
    };
    assert_eq!(revoke_login_key(&dead).await, Revoked::Failed);
  }
}
