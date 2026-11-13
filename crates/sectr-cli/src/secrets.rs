//! `sectr secrets` — set/list/remove an app's server-side secrets.
//!
//! Write-only by contract: values go UP (`sectr secrets set`), names come
//! back (`list`), values NEVER come back down (there is no `get` — the
//! server has no read API for values at all).

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use sectr_common::sidecar::{APPROVAL_POLICY_ENV, APPROVAL_POLICY_ENV_IS_RESERVED};

#[derive(Parser, Debug)]
pub struct SecretsArgs {
  #[command(subcommand)]
  pub command: SecretCommand,
  /// Project directory (must contain sectr.toml — the app is identified
  /// by its slug, exactly as `sectr deploy` creates it).
  #[arg(long, default_value = ".", global = true)]
  pub path: PathBuf,
  /// sectr.toml location relative to the project.
  #[arg(long, default_value = "sectr.toml", global = true)]
  pub config: String,
  /// The server base URL (`http(s)://host[:port]`). Defaults through
  /// `sectr auth login` (or SECTR_API_URL).
  #[arg(long, global = true)]
  pub api: Option<String>,
  /// Platform API key (`skp_…`). Defaults through `sectr auth login`
  /// (or SECTR_PLATFORM_KEY).
  #[arg(long, global = true)]
  pub key: Option<String>,
}

#[derive(Subcommand, Debug)]
pub enum SecretCommand {
  /// Store a secret (prompt hides input when `=VALUE` is omitted —
  /// avoiding both shell history and terminal echo).
  Set { key_value: String },
  /// List secret NAMES for the app (values are write-only).
  List,
  /// Remove a secret (idempotent).
  Rm { key: String },
}

/// Mirror the server's app-identity rule (sectr-server/src/deploy.rs):
/// slug of `[app].name`, falling back to the entry module.
fn app_slug(config_text: &str) -> anyhow::Result<String> {
  let value: toml::Value =
    toml::from_str(config_text).map_err(|e| anyhow::anyhow!("sectr.toml: {e}"))?;
  let entry = value
    .get("agent")
    .and_then(|a| a.get("entry"))
    .and_then(|e| e.as_str())
    .ok_or_else(|| anyhow::anyhow!("sectr.toml is missing agent.entry"))?;
  let module = entry.split(':').next().unwrap_or(entry);
  let name = value
    .get("app")
    .and_then(|a| a.get("name"))
    .and_then(|n| n.as_str())
    .unwrap_or(module);
  let slug: String = name
    .chars()
    .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
    .collect::<String>()
    .to_lowercase();
  let slug = slug.trim_matches('-');
  Ok(if slug.is_empty() {
    "app".into()
  } else {
    slug.to_owned()
  })
}

fn env_safe(key: &str) -> bool {
  !key.is_empty()
    && key
      .chars()
      .next()
      .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
    && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `PUT /secrets/{key}`'s body.
#[derive(serde::Serialize)]
struct PutSecret<'a> {
  value: &'a str,
}

async fn request(
  args: &SecretsArgs,
  method: reqwest::Method,
  path: &str,
  json_body: Option<&PutSecret<'_>>,
) -> anyhow::Result<reqwest::Response> {
  let target = crate::auth::resolve(args.api.as_deref(), args.key.as_deref())?;
  let key = target.key;
  let url = format!("{}{}", target.api, path);
  let client = reqwest::Client::new();
  let mut builder = client.request(method, &url).bearer_auth(&key);
  if let Some(body) = json_body {
    builder = builder.json(body);
  }
  Ok(builder.send().await?)
}

pub async fn run_secrets(args: SecretsArgs) -> anyhow::Result<()> {
  let config_path = args.path.join(&args.config);
  let config_text = std::fs::read_to_string(&config_path).map_err(|e| {
    anyhow::anyhow!(
      "read {}: {e} (sectr secrets identifies the app via sectr.toml)",
      config_path.display()
    )
  })?;
  let slug = app_slug(&config_text)?;

  match &args.command {
    SecretCommand::Set { key_value } => {
      // Before the value prompt: the server refuses this name too.
      let name = key_value
        .split_once('=')
        .map_or(key_value.as_str(), |(k, _)| k);
      if name == APPROVAL_POLICY_ENV {
        anyhow::bail!("{APPROVAL_POLICY_ENV} can't be a secret: {APPROVAL_POLICY_ENV_IS_RESERVED}");
      }
      let (key, value) = match key_value.split_once('=') {
        Some((k, v)) => (k.to_owned(), v.to_owned()),
        None => {
          // No `=VALUE` — prompt (input hidden; not in shell
          // history either way, but the terminal echo matters).
          eprint!("value for {key_value}: ");
          let value = rpassword::read_password().map_err(|e| anyhow::anyhow!("prompt: {e}"))?;
          (key_value.clone(), value)
        }
      };
      if !env_safe(&key) {
        anyhow::bail!(
          "secret keys must be env-safe: [A-Za-z_][A-Za-z0-9_]* (they become environment variables)"
        );
      }
      if value.is_empty() {
        anyhow::bail!("empty secret value");
      }
      let response = request(
        &args,
        reqwest::Method::PUT,
        &format!("/secrets/{key}?app={slug}"),
        Some(&PutSecret { value: &value }),
      )
      .await?;
      let status = response.status();
      if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("sectr secrets set failed ({status}): {body}");
      }
      eprintln!("set {key} for {slug}");
    }
    SecretCommand::List => {
      let response = request(
        &args,
        reqwest::Method::GET,
        &format!("/secrets?app={slug}"),
        None,
      )
      .await?;
      let status = response.status();
      let body: serde_json::Value = response.json().await.unwrap_or(serde_json::Value::Null);
      if !status.is_success() {
        anyhow::bail!("sectr secrets list failed ({status}): {body}");
      }
      let names = body
        .as_array()
        .map(|a| {
          a.iter()
            .filter_map(|m| m.get("name").and_then(|n| n.as_str()))
            .collect::<Vec<_>>()
        })
        .unwrap_or_default();
      if names.is_empty() {
        println!("no secrets for {slug}");
      } else {
        for name in names {
          println!("{name}");
        }
      }
    }
    SecretCommand::Rm { key } => {
      if !env_safe(key) {
        anyhow::bail!("secret keys must be env-safe: [A-Za-z_][A-Za-z0-9_]*");
      }
      let response = request(
        &args,
        reqwest::Method::DELETE,
        &format!("/secrets/{key}?app={slug}"),
        None,
      )
      .await?;
      let status = response.status();
      if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("sectr secrets rm failed ({status}): {body}");
      }
      eprintln!("removed {key} from {slug}");
    }
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test]
  async fn the_approval_policy_env_is_refused_before_any_request() {
    let dir = std::env::temp_dir().join(format!("sectr-secrets-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("sectr.toml"), "[agent]\nentry = \"main:app\"\n").unwrap();
    for key_value in [
      "SECTR_APPROVAL_POLICY=[\"refund\"]",
      "SECTR_APPROVAL_POLICY",
    ] {
      let err = run_secrets(SecretsArgs {
        command: SecretCommand::Set {
          key_value: key_value.into(),
        },
        path: dir.clone(),
        config: "sectr.toml".into(),
        // Nothing listens here: a request would fail differently.
        api: Some("http://127.0.0.1:9".into()),
        key: Some("skp_test".into()),
      })
      .await
      .unwrap_err()
      .to_string();
      assert!(err.contains("can't be a secret"), "{err}");
      assert!(err.contains("[deploy] approval_tools"), "{err}");
    }
  }
}
