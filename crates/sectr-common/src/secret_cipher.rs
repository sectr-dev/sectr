//! At-rest encryption for secret values.
//!
//! The server holds ONE master key (env: `SECTR_SECRETS_MASTER_KEY`); every
//! secret value is stored as a **libsodium sealed box** ([`crypto_box`] —
//! ephemeral X25519 + XSalsa20-Poly1305, the libsodium-interoperable
//! construction): the writer needs only the master key's public half, the
//! reader needs the secret half, and there is no nonce to manage or
//! misreuse. Ciphertext format (the registry's `value_encrypted` column):
//!
//! ```text
//! v1:<base64url(nopad) of sealed-box bytes>
//! ```
//!
//! The `v1:` version tag is deliberate: a KMS-per-org scheme (V1.5) or a
//! re-key migration can introduce `v2:` and migrate lazily on read.
//!
//! Master-key parsing (from the env string) — decode-or-derive, documented
//! so operators know what they're getting:
//! 1. 64 hex chars → 32 raw bytes (`openssl rand -hex 32`),
//! 2. base64 (std or url alphabet, padding tolerated) decoding to exactly
//!    32 bytes (`openssl rand -base64 32`),
//! 3. anything else → SHA-256 of the UTF-8 bytes. Deterministic, no KDF
//!    stretching — fine for a high-entropy generated key; a human-chosen
//!    passphrase is NOT the intended use (and the docs say so).

use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use crypto_box::{PublicKey, SecretKey, aead::OsRng};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::secret::Secret;

#[derive(Debug, Error)]
pub enum CipherError {
  #[error("not a sectr ciphertext (missing/unknown version tag): {0}")]
  BadFormat(String),
  #[error("ciphertext failed authentication (wrong master key or corrupted value)")]
  AuthFailed,
  #[error("base64: {0}")]
  Base64(#[from] base64::DecodeError),
}

/// The sealed-box writer/reader for one deployment's master key.
pub struct SecretCipher {
  secret_key: SecretKey,
  public_key: PublicKey,
}

impl std::fmt::Debug for SecretCipher {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    // The key material must never render.
    f.debug_struct("SecretCipher").finish_non_exhaustive()
  }
}

impl SecretCipher {
  /// Build from the raw 32-byte master key.
  pub fn from_bytes(raw: &[u8; 32]) -> Self {
    let secret_key = SecretKey::from_bytes(*raw);
    let public_key = secret_key.public_key();
    SecretCipher {
      secret_key,
      public_key,
    }
  }

  /// Parse the `SECTR_SECRETS_MASTER_KEY` env value (see module docs for
  /// the decode-or-derive rules).
  pub fn from_env_value(value: &str) -> Self {
    if let Some(raw) = hex_32(value) {
      return Self::from_bytes(&raw);
    }
    if let Some(raw) = base64_32(value) {
      return Self::from_bytes(&raw);
    }
    // Passphrase fallback: SHA-256 of the UTF-8 bytes (documented — a
    // generated key is the intended use).
    let digest: [u8; 32] = Sha256::digest(value.as_bytes()).into();
    Self::from_bytes(&digest)
  }

  /// Seal to the master key's public half → `v1:<b64url>`.
  pub fn encrypt(&self, plaintext: &str) -> Result<String, CipherError> {
    let boxed = self
      .public_key
      .seal(&mut OsRng, plaintext.as_bytes())
      .map_err(|_| CipherError::AuthFailed)?;
    Ok(format!("v1:{}", URL_SAFE_NO_PAD.encode(boxed)))
  }

  /// Open a stored value. Errors are deliberately opaque — "wrong key"
  /// and "corrupted" are indistinguishable by design and neither is an
  /// oracle.
  pub fn decrypt(&self, stored: &str) -> Result<Secret, CipherError> {
    let raw = stored
      .strip_prefix("v1:")
      .ok_or_else(|| CipherError::BadFormat(short(stored)))?;
    let bytes = URL_SAFE_NO_PAD.decode(raw)?;
    let plaintext = self
      .secret_key
      .unseal(&bytes)
      .map_err(|_| CipherError::AuthFailed)?;
    String::from_utf8(plaintext)
      .map(Secret::new)
      .map_err(|_| CipherError::AuthFailed)
  }
}

fn hex_32(s: &str) -> Option<[u8; 32]> {
  let s = s.trim();
  if s.len() != 64 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
    return None;
  }
  let mut out = [0u8; 32];
  for (i, byte) in out.iter_mut().enumerate() {
    *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
  }
  Some(out)
}

fn base64_32(s: &str) -> Option<[u8; 32]> {
  let decoded = STANDARD.decode(s.trim()).ok()?;
  decoded.try_into().ok()
}

fn short(s: &str) -> String {
  let head: String = s.chars().take(12).collect();
  format!("{head}…({} bytes)", s.len())
}

/// Shared handle type — the server holds one cipher for the process.
pub type SharedCipher = Arc<SecretCipher>;

#[cfg(test)]
mod tests {
  use super::*;

  const KEY: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

  #[test]
  fn roundtrips() {
    let cipher = SecretCipher::from_env_value(KEY);
    let ct = cipher.encrypt("sk-openai-prod").unwrap();
    assert!(ct.starts_with("v1:"));
    assert!(!ct.contains("openai"));
    assert_eq!(cipher.decrypt(&ct).unwrap().expose(), "sk-openai-prod");
  }

  #[test]
  fn ciphertext_is_nondeterministic_sealed_box() {
    let cipher = SecretCipher::from_env_value(KEY);
    let a = cipher.encrypt("same").unwrap();
    let b = cipher.encrypt("same").unwrap();
    assert_ne!(a, b, "ephemeral keys differ per seal");
  }

  #[test]
  fn wrong_key_fails_closed() {
    let cipher = SecretCipher::from_env_value(KEY);
    let other = SecretCipher::from_env_value(
      "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210",
    );
    let ct = cipher.encrypt("value").unwrap();
    assert!(matches!(other.decrypt(&ct), Err(CipherError::AuthFailed)));
  }

  #[test]
  fn garbage_and_foreign_formats_rejected() {
    let cipher = SecretCipher::from_env_value(KEY);
    assert!(matches!(
      cipher.decrypt("plaintext-value"),
      Err(CipherError::BadFormat(_))
    ));
    assert!(matches!(
      cipher.decrypt("v1:!!!not base64!!!"),
      Err(CipherError::Base64(_))
    ));
    // valid b64, but the embedded ephemeral key fails to open
    let zeros = format!("v1:{}", "A".repeat(64));
    assert!(matches!(
      cipher.decrypt(&zeros),
      Err(CipherError::AuthFailed)
    ));
  }

  #[test]
  fn key_parsing_rules() {
    // hex-64 → raw key
    let a = SecretCipher::from_env_value(KEY);
    // base64 of the same 32 bytes → same key
    let mut raw = [0u8; 32];
    for (i, byte) in raw.iter_mut().enumerate() {
      *byte = u8::from_str_radix(&KEY[i * 2..i * 2 + 2], 16).unwrap();
    }
    let b64 = STANDARD.encode(raw);
    let b = SecretCipher::from_env_value(&b64);
    let ct = a.encrypt("x").unwrap();
    assert_eq!(b.decrypt(&ct).unwrap().expose(), "x");
    // passphrase fallback = SHA-256 (deterministic across processes)
    let c1 = SecretCipher::from_env_value("hunter2");
    let c2 = SecretCipher::from_env_value("hunter2");
    let ct2 = c1.encrypt("y").unwrap();
    assert_eq!(c2.decrypt(&ct2).unwrap().expose(), "y");
  }

  #[test]
  fn debug_never_shows_key() {
    let cipher = SecretCipher::from_env_value(KEY);
    let rendered = format!("{cipher:?}");
    assert!(!rendered.contains("00112233"));
  }
}
