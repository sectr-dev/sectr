//! Test support: RSA keypairs for the Clerk JWT tests, generated
//! once per test run — no key material is ever committed to the repo.

#![cfg(test)]

use std::sync::OnceLock;

use aws_lc_rs::encoding::{AsDer, Pkcs8V1Der, PublicKeyX509Der};
use aws_lc_rs::rsa::{KeyPair, KeySize};
use aws_lc_rs::signature::KeyPair as _;
use base64::Engine as _;

pub struct TestKey {
  /// PKCS#8 private key as a PEM block (assembled at runtime), for signing
  /// test tokens via `EncodingKey::from_rsa_pem`.
  pub private_pem: String,
  /// Public key as the single-line base64 SPKI operators put in
  /// `SECTR_CLERK_JWT_KEY`.
  pub spki_b64: String,
  /// The same public key as a PEM block (what `ClerkConfig` holds).
  pub public_pem: String,
}

/// PEM-armor `der`, assembled at runtime so the repo never contains a key block.
fn pem(kind: &str, der: &[u8]) -> String {
  let body = base64::engine::general_purpose::STANDARD.encode(der);
  let mut out = format!("-----BEGIN {kind} KEY-----\n");
  for line in body.as_bytes().chunks(64) {
    out.push_str(std::str::from_utf8(line).expect("base64 is ascii"));
    out.push('\n');
  }
  out.push_str(&format!("-----END {kind} KEY-----\n"));
  out
}

fn generate() -> TestKey {
  let pair = KeyPair::generate(KeySize::Rsa2048).expect("RSA keygen");
  let pkcs8: Pkcs8V1Der<'static> = pair.as_der().expect("pkcs8 export");
  let spki: PublicKeyX509Der<'static> = pair.public_key().as_der().expect("spki export");
  let spki_b64 = base64::engine::general_purpose::STANDARD.encode(spki.as_ref());
  let public_pem =
    crate::config::spki_base64_to_pem(&spki_b64).expect("generated key is a valid SPKI");
  TestKey {
    private_pem: pem("PRIVATE", pkcs8.as_ref()),
    spki_b64,
    public_pem,
  }
}

/// The instance key the tests configure as Clerk's.
pub fn primary() -> &'static TestKey {
  static KEY: OnceLock<TestKey> = OnceLock::new();
  KEY.get_or_init(generate)
}

/// A different key — "signed by someone else" cases.
pub fn other() -> &'static TestKey {
  static KEY: OnceLock<TestKey> = OnceLock::new();
  KEY.get_or_init(generate)
}
