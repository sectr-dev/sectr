//! One platform object for every consumer, with endpoint deployments
//! routed to their own backend.
//!
//! The deploy path, the hub, the dispatcher and the reaper all hold ONE
//! [`RunnerPlatform`]. [`RoutingPlatform`] wraps the server's managed
//! platform (Docker, Cloud Run or E2B) and the [`EndpointPlatform`]:
//! a spawn for a deployment with an `endpoint_url` wakes the customer's
//! endpoint, everything else goes to the managed platform. The rule that
//! builders and spawners can't be mixed still holds: endpoint deployments
//! have no builder.

use std::sync::Arc;

use async_trait::async_trait;
use uuid::Uuid;

use sectr_common::registry::Registry;

use crate::dispatch::DispatchError;
use crate::platform::endpoint::{EndpointPlatform, HANDLE_PREFIX};
use crate::platform::{BuildError, BuildOutput, BuildRequest, RunnerInfo, RunnerPlatform};

pub struct RoutingPlatform {
  managed: Arc<dyn RunnerPlatform>,
  endpoint: Arc<EndpointPlatform>,
  registry: Arc<dyn Registry>,
}

impl RoutingPlatform {
  pub fn new(
    managed: Arc<dyn RunnerPlatform>,
    endpoint: Arc<EndpointPlatform>,
    registry: Arc<dyn Registry>,
  ) -> Self {
    RoutingPlatform {
      managed,
      endpoint,
      registry,
    }
  }
}

#[async_trait]
impl RunnerPlatform for RoutingPlatform {
  async fn build(&self, req: BuildRequest) -> Result<BuildOutput, BuildError> {
    self.managed.build(req).await
  }

  fn provisional_build_ref(&self, deployment_id: Uuid, version: i64) -> String {
    self.managed.provisional_build_ref(deployment_id, version)
  }

  /// One registry read per turn decides the backend.
  async fn spawn(
    &self,
    session: Uuid,
    deployment_id: Uuid,
    invocation_id: Uuid,
    ingest_token: &str,
  ) -> Result<Option<String>, DispatchError> {
    let deployment = self
      .registry
      .get_deployment(deployment_id)
      .await
      .map_err(|e| DispatchError::Unavailable(e.to_string()))?;
    match deployment.endpoint_url {
      Some(endpoint_url) => {
        self
          .endpoint
          .wake(session, invocation_id, ingest_token, &endpoint_url)
          .await
      }
      None => {
        self
          .managed
          .spawn(session, deployment_id, invocation_id, ingest_token)
          .await
      }
    }
  }

  async fn teardown(&self, session: Uuid, handle: Option<&str>) {
    match handle {
      Some(h) if h.starts_with(HANDLE_PREFIX) => self.endpoint.teardown(session, handle).await,
      _ => self.managed.teardown(session, handle).await,
    }
  }

  /// The managed platform's answer: tearing down an endpoint turn at a
  /// boundary is safe either way (it only aborts the server's half of the
  /// wake request, which has already answered or soon will).
  fn self_terminating(&self) -> bool {
    self.managed.self_terminating()
  }

  /// Sectr can't list a customer's instances; the reaper sees only the
  /// managed platform's runners.
  async fn list_runners(&self) -> Result<Option<Vec<RunnerInfo>>, DispatchError> {
    self.managed.list_runners().await
  }
}
