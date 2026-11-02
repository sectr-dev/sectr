//! [`Secret`] — a string that refuses to be looked at by accident.
//!
//! The type system, not vigilance, guards secret values in flight:
//! `Debug`/`Display` print `Secret("***")`, there is no `Deref` (so `.field`
//! or method auto-access can't leak), there are **no serde impls** (a
//! `#[derive(Serialize)]` on any struct containing a `Secret` fails to
//! compile instead of silently leaking), and every read goes through
//! [`expose`](Secret::expose) / [`into_inner`](Secret::into_inner) — a call
//! site you can grep for.
//!
//! Scope: the wrapper guards a secret's *intermediate* life — resolved from
//! a store, carried through dispatch, passing near logging. The final
//! consumer (a runner process env) is inherently plaintext; exposure is
//! expected to happen exactly once, where the env overlay is built.

use std::fmt;

#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
  pub fn new(value: impl Into<String>) -> Self {
    Secret(value.into())
  }

  /// The one deliberate read.
  pub fn expose(&self) -> &str {
    &self.0
  }

  /// Owned read for building env maps / passing into process spawns.
  pub fn into_inner(self) -> String {
    self.0
  }
}

impl fmt::Debug for Secret {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "Secret(\"***\")")
  }
}

impl fmt::Display for Secret {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "Secret(\"***\")")
  }
}

// Deliberately NOT implemented: serde Serialize/Deserialize (derives must
// fail to compile), AsRef<str>, Deref, From<&str>/From<String> in the
// *outward* direction (reads must be visible calls, not coercions).

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn debug_and_display_are_redacted() {
    let s = Secret::new("sk-really-secret");
    assert_eq!(format!("{s:?}"), "Secret(\"***\")");
    assert_eq!(format!("{s}"), "Secret(\"***\")");
  }

  #[test]
  fn accident_scenario_debugging_a_collection_is_redacted() {
    // The actual failure mode: `tracing::debug!("{pairs:?}")` on the
    // resolved secret list.
    let pairs = vec![
      ("OPENAI_API_KEY".to_string(), Secret::new("sk-1")),
      ("OTHER".to_string(), Secret::new("x")),
    ];
    let logged = format!("{pairs:?}");
    assert!(logged.contains("Secret(\"***\")"));
    assert!(!logged.contains("sk-1"), "leaked via Debug: {logged}");
  }

  #[test]
  fn reads_are_explicit() {
    let s = Secret::new(String::from("v"));
    assert_eq!(s.expose(), "v");
    assert_eq!(s.clone().into_inner(), "v");
    // Equality works (tests, dedup) without exposing.
    assert_eq!(s, Secret::new("v"));
  }
}
