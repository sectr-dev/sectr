/// The adapters under test. Branch on this with an exhaustive `match`, so a
/// new adapter is a compile error at every place that has to decide about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Adapter {
  OpenAiAgents,
  LangGraph,
  PydanticAi,
}

impl Adapter {
  pub const ALL: [Adapter; 3] = [
    Adapter::OpenAiAgents,
    Adapter::LangGraph,
    Adapter::PydanticAi,
  ];

  /// Directory name under `sdk-python/tests/conformance/gen/`.
  pub fn dir(self) -> &'static str {
    match self {
      Adapter::OpenAiAgents => "openai_agents",
      Adapter::LangGraph => "langgraph",
      Adapter::PydanticAi => "pydantic_ai",
    }
  }
}
