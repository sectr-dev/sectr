//! Running the SDK inspector (`python -m sectr.inspect`): a real import of
//! the app that yields its manifest. Every path that needs a manifest runs
//! it: managed builds inside the built image (the Docker platform through
//! [`import_command`]; Cloud Build as a shell step), the sidecar's serve
//! mode inside the customer's image, and `sectr dev` / `sectr init` on the
//! developer's machine. The last two start it as a local process, through
//! [`run_import`].
//!
//! The manifest always comes back in a file (`SECTR_MANIFEST_OUT`), never
//! on stdout: the user's module may print while it's imported (route
//! banners, framework logs).

use std::fmt;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use tokio::io::AsyncWrite;

use crate::drivers::StderrTail;

/// The manifest file the import writes is refused above this size.
pub const MANIFEST_CAP: u64 = 16 * 1024 * 1024;

/// Whether the app's dependencies live in its own uv environment: it ships
/// a `pyproject.toml`.
pub fn uses_uv(app_dir: &Path) -> bool {
  app_dir.join("pyproject.toml").is_file()
}

/// The import inside a built image: `uv run --no-sync <python> -m
/// sectr.inspect` for a uv app (its dependencies were synced at build,
/// where the system interpreter can't see them; there's no syncing at run
/// time), otherwise `<python> -m sectr.inspect`. The runner's command
/// follows the same choice, so the import sees what the agent will.
pub fn import_command(uses_uv: bool, python: &str) -> Vec<String> {
  let mut command: Vec<String> = if uses_uv {
    vec!["uv".into(), "run".into(), "--no-sync".into(), python.into()]
  } else {
    vec![python.into()]
  };
  command.extend(["-m".into(), "sectr.inspect".into()]);
  command
}

/// Why an import produced no manifest.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportFailure {
  /// The import's exit code: `None` when it never exited on its own
  /// (couldn't start, timed out, killed by a signal), `Some(0)` when it
  /// exited 0 without a usable manifest.
  pub exit_code: Option<i32>,
  /// The end of its stderr (the last [`crate::drivers::STDERR_TAIL_BYTES`]).
  pub stderr_tail: String,
  /// Why, when the exit code alone doesn't say.
  pub message: Option<String>,
}

impl fmt::Display for ImportFailure {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match (&self.message, self.exit_code) {
      (Some(message), _) => write!(f, "{message}")?,
      (None, Some(code)) => write!(f, "the import exited {code}")?,
      (None, None) => write!(f, "the import failed")?,
    }
    let tail = self.stderr_tail.trim_end();
    if !tail.is_empty() {
      write!(f, ":\n{tail}")?;
    }
    Ok(())
  }
}

impl std::error::Error for ImportFailure {}

/// Import `entry` by running `command` (from [`import_command`], or the
/// CLI's syncing `uv run`) as a local process in `dir`, and return the
/// manifest.
///
/// The process inherits none of the caller's `SECTR_*` settings, as in a
/// managed build, where the image sets only the import's own. Its stdout
/// is discarded; its stderr goes to `forward`, and its end is kept for the
/// failure. With a `timeout`, an import that runs longer is killed.
pub async fn run_import(
  command: &[String],
  dir: &Path,
  entry: &str,
  timeout: Option<Duration>,
  forward: impl AsyncWrite + Unpin + Send + 'static,
) -> Result<Value, ImportFailure> {
  let out = std::env::temp_dir().join(format!("sectr-manifest-{}.json", uuid::Uuid::now_v7()));
  let result = import_to(command, dir, entry, timeout, forward, &out).await;
  let _ = tokio::fs::remove_file(&out).await;
  result
}

async fn import_to(
  command: &[String],
  dir: &Path,
  entry: &str,
  timeout: Option<Duration>,
  forward: impl AsyncWrite + Unpin + Send + 'static,
  out: &Path,
) -> Result<Value, ImportFailure> {
  let failure = |exit_code, stderr_tail, message: String| ImportFailure {
    exit_code,
    stderr_tail,
    message: Some(message),
  };
  let (program, args) = command
    .split_first()
    .ok_or_else(|| failure(None, String::new(), "no import command".into()))?;
  let mut cmd = tokio::process::Command::new(program);
  cmd.args(args).current_dir(dir);
  for key in std::env::vars_os().map(|(key, _)| key) {
    if key.as_encoded_bytes().starts_with(b"SECTR_") {
      cmd.env_remove(key);
    }
  }
  cmd
    .env("SECTR_MANIFEST", "1")
    .env("SECTR_ENTRY", entry)
    .env("SECTR_MANIFEST_OUT", out)
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::piped())
    .kill_on_drop(true);
  let mut child = cmd.spawn().map_err(|e| {
    failure(
      None,
      String::new(),
      format!("start `{}`: {e}", command.join(" ")),
    )
  })?;
  let stderr = StderrTail::tee(child.stderr.take().expect("stderr is piped"), forward);

  let waited = match timeout {
    Some(limit) => tokio::time::timeout(limit, child.wait()).await.ok(),
    None => Some(child.wait().await),
  };
  let status = match waited {
    Some(Ok(status)) => status,
    Some(Err(e)) => {
      let _ = child.kill().await;
      let message = format!("wait for the import: {e}");
      return Err(failure(None, stderr.text().await, message));
    }
    None => {
      let _ = child.kill().await;
      let limit = timeout.expect("only a timeout gives up waiting");
      let message = format!("the import didn't finish within {limit:?}");
      return Err(failure(None, stderr.text().await, message));
    }
  };
  let stderr_tail = stderr.text().await;
  if !status.success() {
    return Err(ImportFailure {
      exit_code: status.code(),
      message: status
        .code()
        .is_none()
        .then(|| format!("the import ended without an exit code ({status})")),
      stderr_tail,
    });
  }

  let Ok(meta) = tokio::fs::metadata(out).await else {
    let message = "the import exited 0 but wrote no manifest".into();
    return Err(failure(Some(0), stderr_tail, message));
  };
  if meta.len() > MANIFEST_CAP {
    let message = format!(
      "the manifest is {} bytes, over the {MANIFEST_CAP}-byte limit",
      meta.len()
    );
    return Err(failure(Some(0), stderr_tail, message));
  }
  let bytes = match tokio::fs::read(out).await {
    Ok(bytes) => bytes,
    Err(e) => {
      return Err(failure(
        Some(0),
        stderr_tail,
        format!("read the manifest: {e}"),
      ));
    }
  };
  serde_json::from_slice(&bytes).map_err(|e| {
    failure(
      Some(0),
      stderr_tail,
      format!("the manifest is not JSON: {e}"),
    )
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  fn sh(script: &str) -> Vec<String> {
    vec!["sh".into(), "-c".into(), script.into()]
  }

  async fn run(command: &[String], timeout: Option<Duration>) -> Result<Value, ImportFailure> {
    run_import(
      command,
      &std::env::temp_dir(),
      "app:app",
      timeout,
      tokio::io::sink(),
    )
    .await
  }

  #[test]
  fn the_command_follows_the_app() {
    assert_eq!(
      import_command(false, "python3"),
      ["python3", "-m", "sectr.inspect"]
    );
    assert_eq!(
      import_command(true, "python3"),
      ["uv", "run", "--no-sync", "python3", "-m", "sectr.inspect"]
    );
    let dir = std::env::temp_dir().join(format!("sectr-uses-uv-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    assert!(!uses_uv(&dir));
    std::fs::write(dir.join("pyproject.toml"), "[project]\nname = \"x\"\n").unwrap();
    assert!(uses_uv(&dir));
    std::fs::remove_dir_all(&dir).unwrap();
  }

  #[tokio::test]
  async fn the_manifest_comes_from_the_file_not_stdout() {
    let script = r#"echo banner; printf '{"entry":"%s"}' "$SECTR_ENTRY" > "$SECTR_MANIFEST_OUT""#;
    let manifest = run(&sh(script), None).await.unwrap();
    assert_eq!(manifest, serde_json::json!({"entry": "app:app"}));
  }

  #[tokio::test]
  async fn failures_say_why() {
    let failed = run(&sh("echo 'ImportError: no module x' >&2; exit 3"), None)
      .await
      .unwrap_err();
    assert_eq!(failed.exit_code, Some(3));
    assert_eq!(failed.message, None);
    assert_eq!(
      failed.to_string(),
      "the import exited 3:\nImportError: no module x"
    );

    let silent = run(&sh("exit 0"), None).await.unwrap_err();
    assert_eq!(silent.exit_code, Some(0));
    assert!(silent.to_string().contains("wrote no manifest"), "{silent}");

    let garbage = run(&sh(r#"echo nope > "$SECTR_MANIFEST_OUT""#), None)
      .await
      .unwrap_err();
    assert!(garbage.to_string().contains("not JSON"), "{garbage}");

    let slow = run(&sh("sleep 5"), Some(Duration::from_millis(200)))
      .await
      .unwrap_err();
    assert_eq!(slow.exit_code, None);
    assert!(slow.to_string().contains("didn't finish within"), "{slow}");

    let missing = run(&["sectr-no-such-binary".to_owned()], None)
      .await
      .unwrap_err();
    assert_eq!(missing.exit_code, None);
    assert!(
      missing
        .to_string()
        .starts_with("start `sectr-no-such-binary`"),
      "{missing}"
    );
  }
}
