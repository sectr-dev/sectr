//! In-memory [`ArtifactStore`]; URIs are `mem://{key}`.

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;

use super::ArtifactStore;
use crate::drivers::{DriverError, Result};

/// In-memory [`ArtifactStore`] — the test fake.
#[derive(Default)]
pub struct InMemoryArtifactStore {
  blobs: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl InMemoryArtifactStore {
  pub fn new() -> Self {
    InMemoryArtifactStore::default()
  }

  pub const URI_SCHEME: &'static str = "mem";
}

#[async_trait]
impl ArtifactStore for InMemoryArtifactStore {
  async fn put(&self, key: &str, bytes: Vec<u8>) -> Result<String> {
    self.blobs.lock().unwrap().insert(key.to_string(), bytes);
    Ok(format!("{}://{key}", Self::URI_SCHEME))
  }

  async fn get(&self, uri: &str) -> Result<Vec<u8>> {
    let key = uri
      .strip_prefix(&format!("{}://", Self::URI_SCHEME))
      .ok_or_else(|| DriverError::NotFound(uri.to_string()))?;
    self
      .blobs
      .lock()
      .unwrap()
      .get(key)
      .cloned()
      .ok_or_else(|| DriverError::NotFound(uri.to_string()))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test]
  async fn put_get_round_trip() {
    let store = InMemoryArtifactStore::new();
    let uri = store
      .put("app-1/v3.tar.gz", b"tarball".to_vec())
      .await
      .unwrap();
    assert_eq!(uri, "mem://app-1/v3.tar.gz");
    assert_eq!(store.get(&uri).await.unwrap(), b"tarball".to_vec());
  }

  #[tokio::test]
  async fn put_overwrites_and_unknown_uri_is_not_found() {
    let store = InMemoryArtifactStore::new();
    store.put("k", b"one".to_vec()).await.unwrap();
    store.put("k", b"two".to_vec()).await.unwrap();
    assert_eq!(store.get("mem://k").await.unwrap(), b"two".to_vec());

    assert!(matches!(
      store.get("mem://nope").await.unwrap_err(),
      DriverError::NotFound(_)
    ));
    // Foreign scheme -> not found, not a panic.
    assert!(matches!(
      store.get("s3://bucket/k").await.unwrap_err(),
      DriverError::NotFound(_)
    ));
  }
}
