//! Guarantees the `sectr-server` BINARY is built whenever tests run.
//!
//! Why: the restart-durability E2E (src/ingest/tests.rs,
//! `server_restart_durability_end_to_end`) is a UNIT test that execs the
//! built `sectr-server` binary (found via `current_exe().ancestors()`).
//! Cargo only builds a package's bin targets for INTEGRATION tests — a
//! plain `cargo test --workspace` on a fresh checkout otherwise never
//! builds the bin and the E2E fails with "server binary not built". This
//! target exists so cargo does. (No exec smoke here: the server has no
//! fast flags — it runs until SIGTERM — so existence is the assertion.)

#![allow(
  clippy::disallowed_macros,
  reason = "tests build JSON fixtures with `json!`"
)]

#[test]
fn server_binary_built() {
  let bin = env!("CARGO_BIN_EXE_sectr-server");
  let path = std::path::Path::new(bin);
  assert!(path.exists(), "sectr-server binary missing at {bin:?}");
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)
      .expect("stat sectr-server binary")
      .permissions()
      .mode();
    assert!(
      mode & 0o111 != 0,
      "sectr-server binary not executable: {mode:o}"
    );
  }
}
