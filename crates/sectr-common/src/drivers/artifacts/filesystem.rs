//! [`FilesystemArtifactStore`] — the MVP artifact store (deploy-path
//! decision: local FS on the build host now, R2 later for multi-host).
//!
//! Blobs land under a root directory keyed by the deploy request's key
//! (`<org>/<app>/<version>.tar.gz` by convention); URIs are `fs:<relpath>`
//! so [`ArtifactStore::get`] round-trips without the caller knowing the
//! root. The server holds one store for its lifetime — the root is the
//! VPS's artifact volume (compose mount).

use std::path::PathBuf;

use async_trait::async_trait;

use super::Result;
use crate::drivers::{ArtifactStore, DriverError};

pub struct FilesystemArtifactStore {
  root: PathBuf,
}

impl FilesystemArtifactStore {
  /// `root` is created on demand (idempotent) — the compose volume mount
  /// may not exist yet when the server boots.
  pub fn new(root: impl Into<PathBuf>) -> Self {
    FilesystemArtifactStore { root: root.into() }
  }

  fn resolve(&self, key: &str) -> Result<PathBuf> {
    // No escapes: keys are server-generated (`<org>/<app>/<version>`),
    // but treat `..`/absolute as misuse rather than a traversal bug.
    if key.starts_with('/') || key.split('/').any(|seg| seg == ".." || seg.is_empty()) {
      return Err(DriverError::NotFound(format!("bad artifact key {key:?}")));
    }
    Ok(self.root.join(key))
  }
}

#[async_trait]
impl ArtifactStore for FilesystemArtifactStore {
  async fn put(&self, key: &str, bytes: Vec<u8>) -> Result<String> {
    let path = self.resolve(key)?;
    if let Some(parent) = path.parent() {
      tokio::fs::create_dir_all(parent)
        .await
        .map_err(|e| DriverError::Other(format!("mkdir {}: {e}", parent.display())))?;
    }
    tokio::fs::write(&path, bytes)
      .await
      .map_err(|e| DriverError::Other(format!("write {}: {e}", path.display())))?;
    Ok(format!("fs:{key}"))
  }

  async fn get(&self, uri: &str) -> Result<Vec<u8>> {
    let key = uri
      .strip_prefix("fs:")
      .ok_or_else(|| DriverError::NotFound(format!("not a filesystem artifact uri: {uri:?}")))?;
    let path = self.resolve(key)?;
    tokio::fs::read(&path)
      .await
      .map_err(|_| DriverError::NotFound(format!("artifact {uri:?}")))
  }

  async fn get_at_most(&self, uri: &str, max_bytes: usize) -> Result<Vec<u8>> {
    use tokio::io::AsyncReadExt as _;
    let key = uri
      .strip_prefix("fs:")
      .ok_or_else(|| DriverError::NotFound(format!("not a filesystem artifact uri: {uri:?}")))?;
    let path = self.resolve(key)?;
    let file = tokio::fs::File::open(&path)
      .await
      .map_err(|_| DriverError::NotFound(format!("artifact {uri:?}")))?;
    // One byte past the limit is enough to know it's over.
    let mut bytes = Vec::new();
    file
      .take(max_bytes as u64 + 1)
      .read_to_end(&mut bytes)
      .await
      .map_err(|e| DriverError::Other(format!("read {}: {e}", path.display())))?;
    if bytes.len() > max_bytes {
      return Err(super::too_large(uri, max_bytes));
    }
    Ok(bytes)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test]
  async fn put_get_roundtrip() {
    let root = std::env::temp_dir().join(format!("sectr-artifacts-{}", uuid::Uuid::now_v7()));
    let store = FilesystemArtifactStore::new(&root);
    let uri = store
      .put("org1/app1/3.tar.gz", b"tarball bytes".to_vec())
      .await
      .unwrap();
    assert_eq!(uri, "fs:org1/app1/3.tar.gz");
    let bytes = store.get(&uri).await.unwrap();
    assert_eq!(bytes, b"tarball bytes");

    // The file really is under the root at the relative key.
    assert!(root.join("org1/app1/3.tar.gz").is_file());
    std::fs::remove_dir_all(&root).ok();
  }

  #[tokio::test]
  async fn missing_artifact_is_not_found() {
    let store = FilesystemArtifactStore::new(std::env::temp_dir().join("sectr-artifacts-none"));
    assert!(store.get("fs:nope/missing.tar.gz").await.is_err());
    assert!(store.get("s3:bucket/x").await.is_err());
  }

  #[tokio::test]
  async fn traversal_keys_are_rejected() {
    let store = FilesystemArtifactStore::new(std::env::temp_dir().join("sectr-artifacts-none"));
    assert!(store.put("../escape.tar.gz", vec![]).await.is_err());
    assert!(store.put("/abs.tar.gz", vec![]).await.is_err());
  }
}
