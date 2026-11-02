//! Best-effort browser open, shared by `sectr dev` and `sectr auth login`.

/// Best-effort browser open — no third-party dep: `open` (macOS),
/// `xdg-open` (Linux), `cmd /c start` (Windows). Failure is non-fatal
/// (the URL is printed either way).
pub fn open_browser(url: &str) -> bool {
  #[cfg(target_os = "macos")]
  let spawned = std::process::Command::new("open").arg(url).spawn();
  #[cfg(target_os = "linux")]
  let spawned = std::process::Command::new("xdg-open").arg(url).spawn();
  #[cfg(windows)]
  let spawned = std::process::Command::new("cmd")
    .args(["/C", "start", "", url])
    .spawn();
  spawned.is_ok()
}
