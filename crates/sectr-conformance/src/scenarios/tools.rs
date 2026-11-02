//! Tools shared by scenarios.

use crate::agent::{ToolSpec, Ty};

pub const LOOKUP_ORDER: ToolSpec = ToolSpec {
  name: "lookup_order",
  description: "Look up the status of an order.",
  params: &[("order_id", Ty::Str)],
  returns: Ty::Str,
  gated: false,
};

/// Natively gated.
pub const REFUND: ToolSpec = ToolSpec {
  name: "refund",
  description: "Refund an order (requires manager approval).",
  params: &[("order_id", Ty::Str)],
  returns: Ty::Str,
  gated: true,
};

/// Not gated in code; scenarios gate it by deployment policy.
pub const CANCEL_ORDER: ToolSpec = ToolSpec {
  name: "cancel_order",
  description: "Cancel an order.",
  params: &[("order_id", Ty::Str)],
  returns: Ty::Str,
  gated: false,
};

/// The result every adapter journals for a denied call, and sends the model.
pub const REJECTION: &str = "Tool execution was not approved.";

/// What openai-agents sends the model, as the tool's result, for a tool that
/// raised: the default `failure_error_function`'s text, which doesn't include
/// the exception's message.
pub const OPENAI_AGENTS_TOOL_ERROR: &str =
  "An error occurred while running the tool. Please try again.";

/// The fold's answer to a call with no result that the conversation moved
/// past (`sectr/context.py`, `INTERRUPTED_TOOL_RESULT`).
pub const UNFINISHED: &str = "This tool call did not complete; no result was recorded.";
