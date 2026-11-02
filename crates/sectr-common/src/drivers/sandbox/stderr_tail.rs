//! A runner's stderr, forwarded where it went before (the `sectr dev`
//! terminal, the sidecar's log) while its last few KB are kept, so a runner
//! that fails to start can say why in the journal's `SESSION_ERROR`: an
//! import error's traceback, an adapter rejecting the agent.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::watch;

use super::RunnerExit;

/// How much of the end of a runner's stderr is kept: enough for a Python
/// traceback's last frames and its exception.
pub const STDERR_TAIL_BYTES: usize = 4096;

/// How long [`StderrTail::text`] waits for the rest of stderr once the
/// runner has exited: a process it left behind can hold the pipe open.
const DRAIN_WAIT: Duration = Duration::from_secs(1);

/// The end of a runner's stderr. Cheap to clone.
#[derive(Clone)]
pub struct StderrTail {
  kept: Arc<Mutex<Kept>>,
  done: watch::Receiver<bool>,
}

#[derive(Default)]
struct Kept {
  bytes: VecDeque<u8>,
  /// Whether earlier output was dropped to stay within the bound.
  truncated: bool,
}

impl StderrTail {
  /// Read `stderr` to its end on a background task, writing all of it to
  /// `forward` and keeping the last [`STDERR_TAIL_BYTES`].
  pub fn tee(
    mut stderr: impl AsyncRead + Unpin + Send + 'static,
    mut forward: impl AsyncWrite + Unpin + Send + 'static,
  ) -> Self {
    let kept = Arc::new(Mutex::new(Kept::default()));
    let (done_tx, done) = watch::channel(false);
    let tail = kept.clone();
    tokio::spawn(async move {
      let mut buf = [0u8; 4096];
      loop {
        let n = match stderr.read(&mut buf).await {
          Ok(0) | Err(_) => break,
          Ok(n) => n,
        };
        // Best effort: a closed terminal mustn't stop the tail.
        let _ = forward.write_all(&buf[..n]).await;
        let _ = forward.flush().await;
        let mut tail = tail.lock().unwrap();
        tail.bytes.extend(&buf[..n]);
        let excess = tail.bytes.len().saturating_sub(STDERR_TAIL_BYTES);
        if excess > 0 {
          tail.bytes.drain(..excess);
          tail.truncated = true;
        }
      }
      let _ = done_tx.send(true);
    });
    StderrTail { kept, done }
  }

  /// What the runner wrote, once stderr is closed (or [`DRAIN_WAIT`] has
  /// passed): trimmed, starting at a whole line, with `...` first if
  /// earlier output was dropped.
  pub async fn text(&self) -> String {
    let mut done = self.done.clone();
    let _ = tokio::time::timeout(DRAIN_WAIT, done.wait_for(|done| *done)).await;
    let kept = self.kept.lock().unwrap();
    let bytes: Vec<u8> = kept.bytes.iter().copied().collect();
    let text = String::from_utf8_lossy(&bytes);
    if !kept.truncated {
      return text.trim().to_string();
    }
    // The first line is likely cut, maybe mid-character.
    let rest = text.split_once('\n').map_or("", |(_, rest)| rest);
    format!("...\n{}", rest.trim())
  }
}

/// The `SESSION_ERROR` message for a runner that didn't start: `what`
/// happened, then the end of its stderr, which says why.
pub fn startup_failure_message(what: &str, stderr_tail: &str) -> String {
  if stderr_tail.is_empty() {
    format!("{what}, with nothing on stderr")
  } else {
    format!("{what}. The end of its stderr:\n{stderr_tail}")
  }
}

impl std::fmt::Display for RunnerExit {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match (self.code, self.signal) {
      (Some(code), _) => write!(f, "exit code {code}"),
      (None, Some(signal)) => write!(f, "signal {signal}"),
      (None, None) => write!(f, "no exit code"),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test]
  async fn keeps_the_end_and_forwards_everything() {
    let (mut writer, reader) = tokio::io::duplex(64);
    let (forward, mut forwarded) = tokio::io::duplex(1 << 16);
    let tail = StderrTail::tee(reader, forward);

    let mut written = String::new();
    for i in 0..1000 {
      let line = format!("line {i}\n");
      writer.write_all(line.as_bytes()).await.unwrap();
      written.push_str(&line);
    }
    drop(writer);

    let text = tail.text().await;
    assert!(text.starts_with("...\nline "), "{text}");
    assert!(text.ends_with("line 999"), "{text}");
    assert!(text.len() <= STDERR_TAIL_BYTES + 4);
    // Each kept line is whole.
    for line in text.lines().skip(1) {
      assert!(written.contains(&format!("{line}\n")), "{line:?}");
    }

    let mut all = String::new();
    forwarded.read_to_string(&mut all).await.unwrap();
    assert_eq!(all, written);
  }

  #[tokio::test]
  async fn short_output_is_kept_whole() {
    let (mut writer, reader) = tokio::io::duplex(64);
    let tail = StderrTail::tee(reader, tokio::io::sink());
    writer
      .write_all(b"Traceback (most recent call last):\n  ...\nValueError: nope\n")
      .await
      .unwrap();
    drop(writer);
    assert_eq!(
      tail.text().await,
      "Traceback (most recent call last):\n  ...\nValueError: nope"
    );
  }

  #[test]
  fn the_message_says_when_stderr_was_empty() {
    assert_eq!(
      startup_failure_message("the runner exited during startup (exit code 1)", ""),
      "the runner exited during startup (exit code 1), with nothing on stderr"
    );
    assert_eq!(
      startup_failure_message("the runner exited during startup (exit code 1)", "boom"),
      "the runner exited during startup (exit code 1). The end of its stderr:\nboom"
    );
  }
}
