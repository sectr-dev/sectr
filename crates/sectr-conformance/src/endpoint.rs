/// The model endpoint shapes the mock serves. Every case runs once per
/// endpoint, against the same generated project: the runner's
/// `SECTR_CONFORMANCE_ENDPOINT` picks the client (see `compile::endpoint`),
/// and the mock parses each endpoint's requests into the same
/// [`Msg`](crate::msg::Msg) history and encodes the same
/// [`Reply`](crate::mock::Reply). The journal a case expects doesn't depend
/// on the endpoint.
///
/// Branch on this with an exhaustive `match`, like [`Adapter`](crate::adapter::Adapter).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Endpoint {
  /// OpenAI Chat Completions, streaming. It returns no reasoning, so a
  /// reply with reasoning fails the case.
  OpenAiChat,
  /// OpenRouter's Chat Completions, routed to a Claude model with thinking,
  /// as calibrated against the real API (`calibrate/reasoning.py`):
  /// reasoning streams as `delta.reasoning` plus signed
  /// `delta.reasoning_details`, and a signature sent back must be one the
  /// mock issued, unmodified (leaving reasoning out is fine).
  OpenRouter,
  /// The OpenAI Responses API, streaming, as calibrated against the real
  /// API (`calibrate/responses.py`): reasoning streams as a summary plus
  /// `encrypted_content`, which must come back intact if sent back
  /// (leaving it out is fine).
  OpenAiResponses,
  /// The Anthropic Messages API, streaming, as calibrated against the real
  /// API (`calibrate/anthropic.py`): reasoning streams as a signed
  /// `thinking` block, whose signature must come back intact if sent back
  /// (leaving it out is fine).
  Anthropic,
}

impl Endpoint {
  pub const ALL: [Endpoint; 4] = [
    Endpoint::OpenAiChat,
    Endpoint::OpenRouter,
    Endpoint::OpenAiResponses,
    Endpoint::Anthropic,
  ];

  /// The id in case ids and in `SECTR_CONFORMANCE_ENDPOINT`.
  pub fn id(self) -> &'static str {
    match self {
      Endpoint::OpenAiChat => "openai_chat",
      Endpoint::OpenRouter => "openrouter",
      Endpoint::OpenAiResponses => "openai_responses",
      Endpoint::Anthropic => "anthropic",
    }
  }

  /// The request path under the run's base URL.
  pub fn path(self) -> &'static str {
    match self {
      Endpoint::OpenAiChat | Endpoint::OpenRouter => "/v1/chat/completions",
      Endpoint::OpenAiResponses => "/v1/responses",
      Endpoint::Anthropic => "/v1/messages",
    }
  }
}
