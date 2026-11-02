//! Dev [`SecretSource`]: the user's `.env` file, re-read on every
//! [`resolve`](crate::drivers::SecretSource::resolve) so edits apply on the
//! next turn without restarting `sectr dev`.
//!
//! Single-app by design: `app` is accepted for signature parity with the
//! production impl (which scopes by app) and ignored here.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use super::SecretSource;
use crate::drivers::{DriverError, Result};
use crate::registry::AppId;
use crate::secret::Secret;

/// Dev [`SecretSource`] over a `.env` file.
pub struct EnvFileSecretSource {
  path: PathBuf,
}

impl EnvFileSecretSource {
  pub fn new(path: impl Into<PathBuf>) -> Self {
    EnvFileSecretSource { path: path.into() }
  }

  /// Parse `.env` content: `KEY=VALUE` lines; blank lines and `# comments`
  /// skipped; surrounding single or double quotes stripped from values;
  /// `export ` prefix tolerated. Malformed lines are an error (explicit
  /// beats silently skipping a secret the user thinks they set).
  pub fn parse(path: &Path, content: &str) -> Result<Vec<(String, Secret)>> {
    let mut pairs = Vec::new();
    let mut seen: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for (idx, raw) in content.lines().enumerate() {
      let line_no = idx + 1;
      let line = raw.trim();
      if line.is_empty() || line.starts_with('#') {
        continue;
      }
      let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
      let Some((key, value)) = line.split_once('=') else {
        return Err(DriverError::EnvParse {
          path: path.display().to_string(),
          line: line_no,
          message: "expected KEY=VALUE".into(),
        });
      };
      let key = key.trim();
      if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(DriverError::EnvParse {
          path: path.display().to_string(),
          line: line_no,
          message: format!("invalid key {key:?}"),
        });
      }
      let mut value = value.trim();
      // Strip one matching pair of surrounding quotes.
      for q in ['"', '\''] {
        if value.starts_with(q) && value.ends_with(q) && value.len() >= 2 {
          value = &value[1..value.len() - 1];
          break;
        }
      }
      if seen.insert(key.to_string(), line_no).is_some() {
        // Last one wins, like shell env files; the earlier line is
        // shadowed. Not an error — users iteratively override.
        pairs.retain(|(k, _)| k != key);
      }
      pairs.push((key.to_string(), Secret::new(value)));
    }
    // Deterministic order (env pairs are a set; callers overlay them).
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(pairs)
  }
}

#[async_trait]
impl SecretSource for EnvFileSecretSource {
  async fn resolve(&self, _app: AppId) -> Result<Vec<(String, Secret)>> {
    let content = std::fs::read_to_string(&self.path)?;
    Self::parse(&self.path, &content)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use uuid::Uuid;

  #[tokio::test]
  async fn parse_quotes_comments_and_last_wins() {
    let path = std::env::temp_dir().join(format!("sectr-env-test-{}.env", Uuid::now_v7()));
    std::fs::write(
      &path,
      "# comment\n\
             OPENAI_API_KEY=\"sk-first\"\n\
             \n\
             export LOG_LEVEL=debug\n\
             QUOTED='hello world'\n\
             EMPTY=\n\
             OPENAI_API_KEY=sk-second\n",
    )
    .unwrap();
    let source = EnvFileSecretSource::new(&path);
    let resolved = source.resolve(Uuid::now_v7()).await.unwrap();
    assert_eq!(
      resolved,
      vec![
        ("EMPTY".to_string(), Secret::new("")),
        ("LOG_LEVEL".to_string(), Secret::new("debug")),
        ("OPENAI_API_KEY".to_string(), Secret::new("sk-second")), // last wins
        ("QUOTED".to_string(), Secret::new("hello world")),
      ]
    );
    // Redacted if the whole list gets logged by accident.
    assert!(!format!("{resolved:?}").contains("sk-second"));
    let _ = std::fs::remove_file(&path);
  }

  #[tokio::test]
  async fn malformed_line_is_an_error() {
    let path = std::env::temp_dir().join(format!("sectr-env-test-{}.env", Uuid::now_v7()));
    std::fs::write(&path, "GOOD=1\nNO_EQUALS_SIGN_HERE\n").unwrap();
    let source = EnvFileSecretSource::new(&path);
    let err = source.resolve(Uuid::now_v7()).await.unwrap_err();
    let DriverError::EnvParse { line, message, .. } = err else {
      panic!("expected EnvParse, got {err:?}");
    };
    assert_eq!(line, 2);
    assert!(message.contains("KEY=VALUE"));
    let _ = std::fs::remove_file(&path);
  }

  #[tokio::test]
  async fn missing_file_is_io_error() {
    let source = EnvFileSecretSource::new("/nonexistent/sectr.env");
    assert!(matches!(
      source.resolve(Uuid::now_v7()).await.unwrap_err(),
      DriverError::Io(_)
    ));
  }
}
