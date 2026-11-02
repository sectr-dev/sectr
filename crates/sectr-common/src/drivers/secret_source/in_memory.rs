//! In-memory [`SecretSource`], scoped per app — the fake behind engine tests.

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;

use super::SecretSource;
use crate::drivers::Result;
use crate::registry::AppId;
use crate::secret::Secret;

/// In-memory [`SecretSource`] — the test fake.
#[derive(Default)]
pub struct InMemorySecretSource {
  apps: Mutex<BTreeMap<AppId, Vec<(String, String)>>>,
}

impl InMemorySecretSource {
  pub fn new() -> Self {
    InMemorySecretSource::default()
  }

  /// Seed (or overwrite) an app's secret set (test constructor, off-trait).
  pub fn set(&self, app: AppId, pairs: Vec<(String, String)>) {
    self.apps.lock().unwrap().insert(app, pairs);
  }
}

#[async_trait]
impl SecretSource for InMemorySecretSource {
  async fn resolve(&self, app: AppId) -> Result<Vec<(String, Secret)>> {
    Ok(
      self
        .apps
        .lock()
        .unwrap()
        .get(&app)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|(k, v)| (k, Secret::new(v)))
        .collect(),
    )
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use uuid::Uuid;

  #[tokio::test]
  async fn scopes_by_app() {
    let source = InMemorySecretSource::new();
    let app_a = Uuid::now_v7();
    let app_b = Uuid::now_v7();
    source.set(app_a, vec![("KEY".to_string(), "a".to_string())]);
    source.set(app_b, vec![("KEY".to_string(), "b".to_string())]);
    assert_eq!(
      source.resolve(app_a).await.unwrap(),
      vec![("KEY".to_string(), Secret::new("a"))]
    );
    assert_eq!(
      source.resolve(app_b).await.unwrap(),
      vec![("KEY".to_string(), Secret::new("b"))]
    );
    assert!(source.resolve(Uuid::now_v7()).await.unwrap().is_empty());
  }
}
