//! [`GcsArtifactStore`] — the model (b) production store. Behind the `gcs` feature (default off) so
//! the default workspace build keeps zero GCP dependency weight.
//!
//! Keys are server-generated (`<org>/<app>/<version>.tar.gz` by convention,
//! the same layout as [`FilesystemArtifactStore`]); URIs are
//! `gs://<bucket>/<key>` — opaque strings end to end (deploy.rs never
//! parses them). Overwrites silently on key reuse, like the other impls.
//!
//! Client: the official `google-cloud-storage` crate (googleapis/
//! google-cloud-rust, 1.x stable — the yoshidan implementation was donated
//! to Google and renamed `gcloud-storage`). `SECTR_GCS_ENDPOINT` mode
//! (fake-gcs-server emulator) swaps ADC for anonymous credentials so tests
//! need no GCP identity.

use async_trait::async_trait;

use super::Result;
use crate::drivers::{ArtifactStore, DriverError};

pub struct GcsArtifactStore {
  bucket: String,
  client: google_cloud_storage::client::Storage,
}

impl GcsArtifactStore {
  /// Production constructor: Application Default Credentials, the global
  /// `storage.googleapis.com` endpoint.
  pub async fn new(bucket: impl Into<String>) -> Result<Self> {
    let client = google_cloud_storage::client::Storage::builder()
      .build()
      .await
      .map_err(|e| DriverError::Other(format!("connect to Cloud Storage: {e}")))?;
    Ok(GcsArtifactStore {
      bucket: bucket.into(),
      client,
    })
  }

  /// Emulator constructor (fake-gcs-server): custom endpoint + anonymous
  /// credentials — no GCP identity involved.
  pub async fn new_with_endpoint(bucket: impl Into<String>, endpoint: &str) -> Result<Self> {
    let client = google_cloud_storage::client::Storage::builder()
      .with_endpoint(endpoint)
      .with_credentials(google_cloud_auth::credentials::anonymous::Builder::new().build())
      .build()
      .await
      .map_err(|e| DriverError::Other(format!("connect to the GCS emulator: {e}")))?;
    Ok(GcsArtifactStore {
      bucket: bucket.into(),
      client,
    })
  }

  /// Split a `gs://<bucket>/<key>` URI belonging to THIS store's bucket.
  fn parse<'a>(&self, uri: &'a str) -> Result<&'a str> {
    let rest = uri
      .strip_prefix("gs://")
      .ok_or_else(|| DriverError::NotFound(format!("not a gs: artifact uri: {uri:?}")))?;
    let (bucket, key) = rest
      .split_once('/')
      .ok_or_else(|| DriverError::NotFound(format!("gs: uri without a key: {uri:?}")))?;
    if bucket != self.bucket {
      return Err(DriverError::NotFound(format!(
        "artifact {uri:?} is in bucket {bucket:?}, not {:?}",
        self.bucket
      )));
    }
    // Same key discipline as the filesystem impl: keys are
    // server-generated, treat `..`/empty-segment/absolute-looking keys
    // as misuse rather than a traversal bug.
    if key.is_empty()
      || key.starts_with('/')
      || key.split('/').any(|seg| seg == ".." || seg.is_empty())
    {
      return Err(DriverError::NotFound(format!("bad artifact key {key:?}")));
    }
    Ok(key)
  }
}

fn status_404(e: &google_cloud_storage::Error) -> bool {
  e.http_status_code() == Some(404)
}

#[async_trait]
impl ArtifactStore for GcsArtifactStore {
  async fn put(&self, key: &str, bytes: Vec<u8>) -> Result<String> {
    if key.is_empty()
      || key.starts_with('/')
      || key.split('/').any(|seg| seg == ".." || seg.is_empty())
    {
      return Err(DriverError::NotFound(format!("bad artifact key {key:?}")));
    }
    let bucket = format!("projects/_/buckets/{}", self.bucket);
    self
      .client
      .write_object(bucket, key, bytes::Bytes::from(bytes))
      .send_buffered()
      .await
      .map_err(|e| DriverError::Other(format!("GCS put {key:?}: {e}")))?;
    Ok(format!("gs://{}/{}", self.bucket, key))
  }

  async fn get(&self, uri: &str) -> Result<Vec<u8>> {
    let key = self.parse(uri)?;
    let bucket = format!("projects/_/buckets/{}", self.bucket);
    let mut resp = self
      .client
      .read_object(bucket, key)
      .send()
      .await
      .map_err(|e| {
        if status_404(&e) {
          DriverError::NotFound(format!("artifact {uri:?}"))
        } else {
          DriverError::Other(format!("GCS get {uri:?}: {e}"))
        }
      })?;
    let mut out = Vec::new();
    while let Some(chunk) = resp
      .next()
      .await
      .transpose()
      .map_err(|e| DriverError::Other(format!("GCS get {uri:?} (mid-stream): {e}")))?
    {
      out.extend_from_slice(&chunk);
    }
    Ok(out)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// The emulator endpoint (fsouza/fake-gcs-server, see
  /// docker-compose.test.yml). Tests are skipped unless it's up — the
  /// same skip pattern as the pgtemp-backed Postgres tests.
  fn emulator_url() -> Option<String> {
    match std::env::var("SECTR_TEST_GCS_URL") {
      Ok(url) if !url.is_empty() => Some(url),
      _ => {
        eprintln!("skipping: SECTR_TEST_GCS_URL not set (fake-gcs-server not up)");
        None
      }
    }
  }

  async fn emulator_store() -> Option<(GcsArtifactStore, String)> {
    let url = emulator_url()?;
    // One bucket per test run. The StorageControl client defaults to
    // gRPC transport, which fake-gcs-server (JSON API only) rejects —
    // create buckets via plain JSON API instead.
    let bucket = format!("sectr-test-{}", uuid::Uuid::now_v7());
    create_bucket(&url, &bucket).await;
    let store = GcsArtifactStore::new_with_endpoint(&bucket, &url)
      .await
      .expect("gcs store");
    Some((store, bucket))
  }

  async fn create_bucket(endpoint: &str, bucket: &str) {
    let status = reqwest::Client::new()
      .post(format!("{endpoint}/storage/v1/b"))
      .json(&serde_json::json!({ "name": bucket }))
      .send()
      .await
      .expect("create bucket request")
      .status();
    assert!(
      status.is_success(),
      "bucket create for {bucket} failed: {status}"
    );
  }

  #[tokio::test]
  async fn put_get_roundtrip() {
    let Some((store, bucket)) = emulator_store().await else {
      return;
    };
    let uri = store
      .put("org1/app1/3.tar.gz", b"tarball bytes".to_vec())
      .await
      .unwrap();
    assert_eq!(uri, format!("gs://{bucket}/org1/app1/3.tar.gz"));
    assert_eq!(store.get(&uri).await.unwrap(), b"tarball bytes");
  }

  #[tokio::test]
  async fn put_overwrites_and_missing_is_not_found() {
    let Some((store, _)) = emulator_store().await else {
      return;
    };
    store.put("k", b"one".to_vec()).await.unwrap();
    store.put("k", b"two".to_vec()).await.unwrap();
    assert_eq!(
      store
        .get(&format!("gs://{}/k", store.bucket))
        .await
        .unwrap(),
      b"two"
    );

    // Unknown object in OUR bucket → NotFound.
    let uri = format!("gs://{}/missing.tar.gz", store.bucket);
    assert!(matches!(
      store.get(&uri).await.unwrap_err(),
      DriverError::NotFound(_)
    ));
    // Foreign scheme / foreign bucket / bad key → NotFound, not panic.
    assert!(store.get("fs:bucket/x").await.is_err());
    assert!(store.get("gs://other-bucket/k").await.is_err());
    assert!(store.get("gs://no-key").await.is_err());
  }

  #[tokio::test]
  async fn traversal_keys_are_rejected() {
    let Some((store, _)) = emulator_store().await else {
      return;
    };
    assert!(store.put("../escape.tar.gz", vec![]).await.is_err());
    assert!(store.put("/abs.tar.gz", vec![]).await.is_err());
    assert!(store.put("", vec![]).await.is_err());
  }
}
