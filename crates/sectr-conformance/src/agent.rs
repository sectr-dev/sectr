/// The agent under test, as data: the compiler emits it once per adapter.
/// Grows a field at a time (gating, policy, output type) as scenarios need.
#[derive(Debug, Clone)]
pub struct AgentSpec {
  /// The agent's system prompt. Every model request must carry it.
  pub instructions: &'static str,
  /// Every model request must offer these.
  pub tools: &'static [ToolSpec],
  /// A structured output type: a pydantic `Answer` model with these fields,
  /// declared the framework's way (`output_type=`, `response_format=`).
  pub output: Option<&'static [(&'static str, Ty)]>,
  /// The harness's MCP server, if the agent attaches one.
  pub mcp: Mcp,
  /// Other agents this one calls as tools.
  pub subagents: &'static [SubagentTool],
  /// Agents this one hands off to (openai-agents' `handoffs=[...]`). A
  /// scenario with any runs on openai-agents alone: the other frameworks
  /// have no handoffs.
  pub handoffs: &'static [HandoffAgent],
}

/// An agent the conversation is handed off to, which then answers the
/// user: its model requests stream and carry its own instructions and
/// tools ([`crate::case::Case::handoff_model`]). The SDK offers the handoff
/// as the tool `transfer_to_<name>`.
#[derive(Debug)]
pub struct HandoffAgent {
  /// The Python variable, and the agent's name.
  pub name: &'static str,
  pub instructions: &'static str,
  pub tools: &'static [ToolSpec],
  /// It hands back to the agent under test.
  pub back: bool,
}

/// A tool that runs another agent and returns its final answer, written the
/// framework's usual way: openai-agents `Agent.as_tool()`; on langgraph a
/// Sectr `@tool` awaiting a `create_agent(name=...)` agent; on pydantic-ai a
/// tool awaiting the subagent's `run()` (agent delegation). It takes one
/// `input: str` (`as_tool()`'s parameter), which the subagent gets as its
/// user message.
#[derive(Debug, Clone, Copy)]
pub struct SubagentTool {
  pub name: &'static str,
  pub description: &'static str,
  /// Gated in code, the framework's way (`as_tool(needs_approval=True)`,
  /// `@tool(needs_approval=True)`, `Tool(..., requires_approval=True)`).
  pub gated: bool,
  pub agent: &'static Subagent,
}

/// The agent a [`SubagentTool`] runs. Its model requests must carry its
/// instructions and offer its tools ([`crate::case::Case::subagent_model`]).
#[derive(Debug)]
pub struct Subagent {
  /// The Python variable, and the agent's name.
  pub name: &'static str,
  pub instructions: &'static str,
  pub tools: &'static [ToolSpec],
}

/// The harness's MCP server (streamable HTTP, at
/// `$SECTR_CONFORMANCE_URL/mcp`), attached the framework's native way.
#[derive(Debug, Clone, Copy)]
pub struct Mcp {
  /// The tools it serves. Every model request must offer these too. A call
  /// reaches the case like a stub's ([`crate::case::Case::tool`]).
  pub tools: &'static [ToolSpec],
  /// Turn on the framework's switch for preferring a result's
  /// `structuredContent` over its text, where it has one (openai-agents'
  /// `use_structured_content`).
  pub structured: bool,
}

impl Mcp {
  /// No MCP server.
  pub const NONE: Mcp = Mcp {
    tools: &[],
    structured: false,
  };
}

/// A tool, emitted as a typed stub that calls the harness's tool backend:
/// the case decides each execution's outcome ([`crate::case::Case::tool`]).
#[derive(Debug, Clone)]
pub struct ToolSpec {
  pub name: &'static str,
  /// The tool's docstring, which every SDK sends as its description.
  pub description: &'static str,
  pub params: &'static [(&'static str, Ty)],
  pub returns: Ty,
  /// Gated the framework's native way (`needs_approval=True`,
  /// `requires_approval=True`): every call pauses the turn for approval.
  pub gated: bool,
}

/// A parameter or return type.
#[derive(Debug, Clone, Copy)]
pub enum Ty {
  Str,
  Int,
  /// A JSON object.
  Object,
  /// A JSON array.
  List,
  /// A string or `None`.
  Optional,
  /// A pydantic model, emitted as a class in `tools.py`. Returned, the stub
  /// builds it from the backend's value, as a user's tool would return one.
  Model(&'static Model),
  /// A Python value JSON has no form for (a `datetime`, a tuple…), which
  /// the stub builds from the backend's value. Only returned.
  Py(&'static PyType),
}

/// How a stub builds a [`Ty::Py`] value.
#[derive(Debug)]
pub struct PyType {
  /// The return annotation.
  pub python: &'static str,
  /// The standard-library import it needs, if any.
  pub import: Option<&'static str>,
  /// The expression that builds it, `{}` standing for the backend's value.
  pub build: &'static str,
}

/// A pydantic `BaseModel` subclass.
#[derive(Debug)]
pub struct Model {
  pub name: &'static str,
  pub fields: &'static [(&'static str, Ty)],
}

impl Ty {
  /// Whether the Python type needs `typing.Any`.
  pub fn uses_any(self) -> bool {
    matches!(self, Ty::Object | Ty::List)
  }

  /// The type's JSON Schema, for an MCP tool's input schema.
  pub fn json_schema(self) -> serde_json::Value {
    let ty = match self {
      Ty::Str => "string",
      Ty::Int => "integer",
      Ty::Object | Ty::Model(_) => "object",
      Ty::List => "array",
      Ty::Optional => return serde_json::json!({ "type": ["string", "null"] }),
      Ty::Py(py) => panic!("{}: only a return type", py.python),
    };
    serde_json::json!({ "type": ty })
  }

  pub fn python(self) -> &'static str {
    match self {
      Ty::Str => "str",
      Ty::Int => "int",
      Ty::Object => "dict[str, Any]",
      Ty::List => "list[Any]",
      Ty::Optional => "str | None",
      Ty::Model(model) => model.name,
      Ty::Py(py) => py.python,
    }
  }
}
