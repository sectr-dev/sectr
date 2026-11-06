//! [`ArtifactStore`] — deployment tarball blobs. The deploy flow (Phase 5+)
//! uploads the user's app bundle, the build step reads it to produce the
//! runner image, and `deployments.artifact_uri` points at the result.
//!
//! Impls land with their writers: the dev filesystem impl and the
//! object-storage prod impl arrive with the deploy flow itself — building a
//! blob store before there's a writer is speculation. `sectr dev` skips
//! artifacts entirely (it runs the user's code in place).

mod filesystem;
/// GCS-backed store. Feature-gated:
/// the default workspace build compiles no GCP code.
#[cfg(feature = "gcs")]
mod gcs;
mod in_memory;

pub use filesystem::FilesystemArtifactStore;
#[cfg(feature = "gcs")]
pub use gcs::GcsArtifactStore;
pub use in_memory::InMemoryArtifactStore;

use async_trait::async_trait;

use super::Result;

#[async_trait]
pub trait ArtifactStore: Send + Sync {
  /// Store a blob under `key`; returns its URI (the value destined for
  /// `deployments.artifact_uri`). Overwrites silently on key reuse.
  async fn put(&self, key: &str, bytes: Vec<u8>) -> Result<String>;
  /// Fetch a blob by URI.
  async fn get(&self, uri: &str) -> Result<Vec<u8>>;
  /// [`get`](Self::get), for a blob that something other than the server
  /// wrote: fails once it passes `max_bytes`, without reading the rest.
  async fn get_at_most(&self, uri: &str, max_bytes: usize) -> Result<Vec<u8>>;
}

/// The error for a blob over a [`ArtifactStore::get_at_most`] limit.
pub(crate) fn too_large(uri: &str, max_bytes: usize) -> super::DriverError {
  super::DriverError::Other(format!(
    "artifact {uri:?} is larger than the {max_bytes}-byte limit"
  ))
}

#[cfg(test)]
mod conformance_tests {
  //! Trait conformance parameterized over impls: every store must
  //! round-trip through its returned (opaque) URI, overwrite on key
  //! reuse, and 404 cleanly on missing/foreign URIs. The GCS impl joins
  //! the matrix when the `gcs` feature is on and the emulator is up.

  use super::*;

  async fn roundtrip(store: &dyn ArtifactStore) {
    let uri = store
      .put("org/app/1.tar.gz", b"payload v1".to_vec())
      .await
      .unwrap();
    assert_eq!(store.get(&uri).await.unwrap(), b"payload v1");

    // Overwrite on key reuse; the SAME uri now yields the new bytes.
    let uri2 = store
      .put("org/app/1.tar.gz", b"payload v2".to_vec())
      .await
      .unwrap();
    assert_eq!(uri2, uri);
    assert_eq!(store.get(&uri).await.unwrap(), b"payload v2");
  }

  async fn limited(store: &dyn ArtifactStore) {
    let uri = store.put("org/app/limited", vec![b'x'; 100]).await.unwrap();
    assert_eq!(store.get_at_most(&uri, 100).await.unwrap().len(), 100);
    let err = store.get_at_most(&uri, 99).await.unwrap_err().to_string();
    assert!(err.contains("99-byte limit"), "{err}");
    assert!(
      store
        .get_at_most(&format!("{uri}-missing"), 100)
        .await
        .is_err()
    );
  }

  async fn not_found(store: &dyn ArtifactStore) {
    let uri = store.put("k", b"v".to_vec()).await.unwrap();
    assert!(store.get(&format!("{uri}-missing")).await.is_err());
    // Foreign schemes are a clean NotFound, never a panic.
    let bogus = if uri.starts_with("gs:") { "fs:" } else { "gs:" };
    assert!(store.get(&format!("{bogus}nowhere/x")).await.is_err());
  }

  #[tokio::test]
  async fn in_memory_conformance() {
    let store = InMemoryArtifactStore::new();
    roundtrip(&store).await;
    limited(&store).await;
    not_found(&store).await;
  }

  #[tokio::test]
  async fn filesystem_conformance() {
    let root = std::env::temp_dir().join(format!("sectr-artifacts-conf-{}", uuid::Uuid::now_v7()));
    let store = FilesystemArtifactStore::new(&root);
    roundtrip(&store).await;
    limited(&store).await;
    not_found(&store).await;
    std::fs::remove_dir_all(&root).ok();
  }

  #[cfg(feature = "gcs")]
  #[tokio::test]
  async fn gcs_conformance() {
    let Ok(url) = std::env::var("SECTR_TEST_GCS_URL") else {
      eprintln!("skipping: SECTR_TEST_GCS_URL not set (fake-gcs-server not up)");
      return;
    };
    let bucket = format!("sectr-conf-{}", uuid::Uuid::now_v7());
    let status = reqwest::Client::new()
      .post(format!("{url}/storage/v1/b"))
      .json(&serde_json::json!({ "name": bucket }))
      .send()
      .await
      .expect("create bucket request")
      .status();
    assert!(
      status.is_success(),
      "bucket create for {bucket} failed: {status}"
    );
    let store = GcsArtifactStore::new_with_endpoint(&bucket, &url)
      .await
      .expect("gcs store");
    roundtrip(&store).await;
    limited(&store).await;
    not_found(&store).await;
  }
}
