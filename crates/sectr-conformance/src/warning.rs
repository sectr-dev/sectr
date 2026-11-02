//! Warnings: something a case notices is wrong but doesn't stop for. A run
//! can collect any number; they never change its outcome or the exit code.
//! The report groups them by kind and by their known reason, so a new one
//! stands out as untracked.

use crate::adapter::Adapter;
use crate::endpoint::Endpoint;

/// An empty `MESSAGE_DELTA`. It carries nothing, and message text is
/// compared concatenated, so it would otherwise go unnoticed.
pub const EMPTY_DELTA: &str = "empty-delta";

/// An invocation's first model request sends the tools, the system prompt
/// or earlier history differently from the last request ([`crate::replay`]):
/// the provider's prompt cache misses from there on.
pub const REPLAY_CHANGED: &str = "replay-changed";

/// A [`REPLAY_CHANGED`] whose only difference is reasoning the last request
/// sent back with an assistant message and this one leaves out
/// ([`crate::replay::drops_only_reasoning`]): the model loses its earlier
/// reasoning, unlike when the SDK keeps its own history.
pub const REPLAY_DROPPED_REASONING: &str = "replay-dropped-reasoning";

#[derive(Debug, Clone)]
pub struct Warning {
  pub kind: &'static str,
  /// Where the case saw it, and the event.
  pub detail: String,
}

/// Known warnings: a kind on an adapter (and, optionally, only on one
/// endpoint). They're per adapter, so the same kind on another adapter is a
/// different bug, and shows as untracked until it's listed here.
const KNOWN: &[(&str, Adapter, Option<Endpoint>, &str)] = &[(
  REPLAY_CHANGED,
  Adapter::PydanticAi,
  None,
  "the retry prompt after an empty reply is dropped",
)];

/// The known reason for a warning, if any.
pub fn known(kind: &str, adapter: Adapter, endpoint: Endpoint) -> Option<&'static str> {
  KNOWN
    .iter()
    .find(|(k, a, e, _)| *k == kind && *a == adapter && e.is_none_or(|e| e == endpoint))
    .map(|(_, _, _, reason)| *reason)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_known_kind_is_tracked_only_on_its_adapter_and_endpoint() {
    for endpoint in Endpoint::ALL {
      assert_eq!(
        known(REPLAY_CHANGED, Adapter::PydanticAi, endpoint),
        Some("the retry prompt after an empty reply is dropped")
      );
      assert_eq!(known(EMPTY_DELTA, Adapter::OpenAiAgents, endpoint), None);
    }
    assert_eq!(
      known(REPLAY_CHANGED, Adapter::LangGraph, Endpoint::OpenAiChat),
      None
    );
    assert_eq!(
      known("other", Adapter::PydanticAi, Endpoint::OpenAiChat),
      None
    );
  }
}
