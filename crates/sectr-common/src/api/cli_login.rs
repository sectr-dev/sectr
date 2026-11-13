//! Browser login's wire shapes (`POST /auth/cli`, `POST /auth/cli/poll`):
//! the server writes them and `sectr auth login` reads them.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::OrgRef;

/// `POST /auth/cli`'s body: the machine's name, shown on the approval page.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartRequest {
  #[serde(default)]
  pub client_name: String,
}

/// `POST /auth/cli`'s answer: the device code (the CLI's secret), the code
/// the user checks, and where to approve it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceGrant {
  pub device_code: String,
  pub user_code: String,
  pub verification_url: String,
  pub verification_url_complete: String,
  /// Seconds until the request expires.
  pub expires_in: u64,
  /// Seconds between polls.
  pub interval: u64,
}

/// `POST /auth/cli/poll`'s body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollRequest {
  pub device_code: String,
}

/// `POST /auth/cli/poll`'s answer. The key comes only with `approved`, on
/// the one call that consumes the request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PollAnswer {
  Pending,
  Denied,
  Expired,
  Approved(ApprovedLogin),
  /// A status newer than this reader: keep polling.
  #[serde(other, skip_serializing)]
  Unknown,
}

/// An approved login: the minted platform key, its id (to revoke it at
/// logout) and the org it belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovedLogin {
  pub key: String,
  pub key_id: Uuid,
  pub org: OrgRef,
}
