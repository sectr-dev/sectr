//! Key format + hashing. Keys are minted server-side as
//! `skp_<43 base64url chars>` (platform) / `ski_<…>` (invocation) /
//! `sks_<…>` (user) — the class prefix is user-visible
//! identification only; auth goes through the stored SHA-256 hash
//! (`key_hash`, UNIQUE), never the value. The full value is returned
//! exactly once at creation and never persisted.

use base64::Engine as _;
use sha2::{Digest, Sha256};

use sectr_common::registry::ApiKeyKind;

/// Class prefix of a minted key: `skp` (platform) / `ski` (invocation) /
/// `sks` (user).
pub fn prefix(kind: ApiKeyKind) -> &'static str {
  match kind {
    ApiKeyKind::Platform => "skp",
    ApiKeyKind::Invocation => "ski",
    ApiKeyKind::User => "sks",
  }
}

/// Mint a full key value: `skp_|ski_` + 32 random bytes, base64url (43
/// chars). 256 bits of entropy — offline brute force is not a thing.
pub fn mint(kind: ApiKeyKind) -> String {
  mint_ref(&kind)
}

pub fn mint_ref(kind: &ApiKeyKind) -> String {
  let mut buf = [0u8; 32];
  getrandom::getrandom(&mut buf).expect("OS entropy");
  format!(
    "{}_{}",
    prefix(*kind),
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
  )
}

/// SHA-256 of the full key value, hex — what the store keeps.
pub fn hash(key: &str) -> String {
  let mut hasher = Sha256::new();
  hasher.update(key.as_bytes());
  format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn minted_keys_have_class_prefix_and_entropy() {
    let k1 = mint(ApiKeyKind::Platform);
    let k2 = mint(ApiKeyKind::Platform);
    assert!(k1.starts_with("skp_") && k2.starts_with("skp_"));
    assert_ne!(k1, k2, "two mints must differ (OS entropy)");
    assert_eq!(k1.len(), 4 + 43, "4-char prefix + 43 base64url chars");
    assert!(
      k1.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
      "base64url alphabet only"
    );

    let inv = mint(ApiKeyKind::Invocation);
    assert!(inv.starts_with("ski_"));

    let user = mint(ApiKeyKind::User);
    assert!(user.starts_with("sks_"));
    assert_eq!(user.len(), 4 + 43);
  }

  #[test]
  fn hash_is_deterministic_sha256_hex() {
    let h1 = hash("skp_abc");
    let h2 = hash("skp_abc");
    assert_eq!(h1, h2);
    assert_eq!(h1.len(), 64);
    assert!(h1.chars().all(|c| c.is_ascii_hexdigit()));
    assert_ne!(hash("skp_abc"), hash("skp_abd"));
  }
}
