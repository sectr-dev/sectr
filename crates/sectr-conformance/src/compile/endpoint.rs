//! `endpoint.py`: `model()`, the adapter's model client for the run's
//! `SECTR_CONFORMANCE_ENDPOINT`, pointed at the mock under
//! `SECTR_CONFORMANCE_URL`. One branch per [`Endpoint`], so the agent code
//! in `main.py` is the same on every endpoint.

use crate::adapter::Adapter;
use crate::endpoint::Endpoint;

/// How an adapter's framework reaches one endpoint.
struct Client {
  imports: &'static [&'static str],
  /// Dependencies beyond the adapter's own.
  deps: &'static [&'static str],
  /// The branch's body: builds and returns the model from `url`.
  body: &'static str,
  /// openai-agents only: the body of `model_settings()`, for the agent's
  /// `model_settings` (the model takes none of its own); `None` for the
  /// defaults.
  settings: Option<&'static str>,
}

fn client(adapter: Adapter, endpoint: Endpoint) -> Client {
  match (adapter, endpoint) {
    (Adapter::OpenAiAgents, Endpoint::OpenAiChat) => Client {
      imports: &[
        "from agents import OpenAIChatCompletionsModel",
        "from openai import AsyncOpenAI",
      ],
      deps: &[],
      body: r#"client = AsyncOpenAI(base_url=url + "/v1", api_key="conformance", max_retries=0)
return OpenAIChatCompletionsModel(model="mock", openai_client=client)"#,
      settings: None,
    },
    (Adapter::LangGraph, Endpoint::OpenAiChat) => Client {
      imports: &["from langchain_openai import ChatOpenAI"],
      deps: &["langchain-openai"],
      body: r#"return ChatOpenAI(
    model="mock", base_url=url + "/v1", api_key="conformance", max_retries=0
)"#,
      settings: None,
    },
    (Adapter::PydanticAi, Endpoint::OpenAiChat) => Client {
      imports: &[
        "from openai import AsyncOpenAI",
        "from pydantic_ai.models.openai import OpenAIChatModel",
        "from pydantic_ai.providers.openai import OpenAIProvider",
      ],
      deps: &["pydantic-ai-slim[openai]"],
      body: r#"client = AsyncOpenAI(base_url=url + "/v1", api_key="conformance", max_retries=0)
return OpenAIChatModel("mock", provider=OpenAIProvider(openai_client=client))"#,
      settings: None,
    },
    // The model name is the one the endpoint was calibrated against: some
    // clients key behavior on it (pydantic-ai picks its profile by name).
    // openai-agents has no OpenRouter client; its examples use Chat
    // Completions against OpenRouter's base URL.
    (Adapter::OpenAiAgents, Endpoint::OpenRouter) => Client {
      imports: &[
        "from agents import OpenAIChatCompletionsModel",
        "from openai import AsyncOpenAI",
      ],
      deps: &[],
      body: r#"client = AsyncOpenAI(base_url=url + "/v1", api_key="conformance", max_retries=0)
return OpenAIChatCompletionsModel(model="anthropic/claude-haiku-5.5", openai_client=client)"#,
      settings: None,
    },
    (Adapter::LangGraph, Endpoint::OpenRouter) => Client {
      imports: &["from langchain_openrouter import ChatOpenRouter"],
      deps: &["langchain-openrouter"],
      body: r#"return ChatOpenRouter(
    model="anthropic/claude-haiku-5.5",
    base_url=url + "/v1",
    api_key="conformance",
    max_retries=0,
)"#,
      settings: None,
    },
    // The Responses API, with the model it was calibrated against.
    (Adapter::OpenAiAgents, Endpoint::OpenAiResponses) => Client {
      imports: &[
        "from agents import OpenAIResponsesModel",
        "from openai import AsyncOpenAI",
      ],
      deps: &[],
      body: r#"client = AsyncOpenAI(base_url=url + "/v1", api_key="conformance", max_retries=0)
return OpenAIResponsesModel(model="gpt-5.4-nano", openai_client=client)"#,
      settings: None,
    },
    (Adapter::LangGraph, Endpoint::OpenAiResponses) => Client {
      imports: &["from langchain_openai import ChatOpenAI"],
      deps: &["langchain-openai"],
      body: r#"return ChatOpenAI(
    model="gpt-5.4-nano",
    base_url=url + "/v1",
    api_key="conformance",
    max_retries=0,
    use_responses_api=True,
)"#,
      settings: None,
    },
    (Adapter::PydanticAi, Endpoint::OpenAiResponses) => Client {
      imports: &[
        "from openai import AsyncOpenAI",
        "from pydantic_ai.models.openai import OpenAIResponsesModel",
        "from pydantic_ai.providers.openai import OpenAIProvider",
      ],
      deps: &["pydantic-ai-slim[openai]"],
      body: r#"client = AsyncOpenAI(base_url=url + "/v1", api_key="conformance", max_retries=0)
return OpenAIResponsesModel("gpt-5.4-nano", provider=OpenAIProvider(openai_client=client))"#,
      settings: None,
    },
    // The Messages API, with the model it was calibrated against and
    // thinking on, as it must be for a model to reason (summarized, so the
    // thinking has text).
    // openai-agents has no Anthropic client; its docs route other providers
    // through LiteLLM, which sends thinking back only when the agent asks
    // for reasoning. The LiteLLM it resolves to with openai 3 (1.83.0;
    // later ones need openai<3) maps that to `thinking.type: enabled`,
    // which this model rejects, so the thinking is also given explicitly.
    (Adapter::OpenAiAgents, Endpoint::Anthropic) => Client {
      imports: &[
        "from agents import ModelSettings",
        "from openai.types.shared import Reasoning",
      ],
      deps: &["openai-agents[litellm]"],
      body: r#"# Imported here: LiteLLM takes about a second to import.
from agents.extensions.models.litellm_model import LitellmModel

return LitellmModel(
    model="anthropic/claude-haiku-5-5", base_url=url, api_key="conformance"
)"#,
      settings: Some(
        r#"return ModelSettings(
    reasoning=Reasoning(effort="high"),
    extra_args={"thinking": {"type": "adaptive", "display": "summarized"}},
)"#,
      ),
    },
    (Adapter::LangGraph, Endpoint::Anthropic) => Client {
      imports: &["from langchain_anthropic import ChatAnthropic"],
      deps: &["langchain-anthropic"],
      body: r#"return ChatAnthropic(
    model="claude-haiku-5-5",
    base_url=url,
    api_key="conformance",
    max_retries=0,
    thinking={"type": "adaptive", "display": "summarized"},
)"#,
      settings: None,
    },
    (Adapter::PydanticAi, Endpoint::Anthropic) => Client {
      imports: &[
        "from anthropic import AsyncAnthropic",
        "from pydantic_ai.models.anthropic import AnthropicModel, AnthropicModelSettings",
        "from pydantic_ai.providers.anthropic import AnthropicProvider",
      ],
      deps: &["pydantic-ai-slim[anthropic]"],
      body: r#"client = AsyncAnthropic(base_url=url, api_key="conformance", max_retries=0)
return AnthropicModel(
    "claude-haiku-5-5",
    provider=AnthropicProvider(anthropic_client=client),
    settings=AnthropicModelSettings(
        anthropic_thinking={"type": "adaptive", "display": "summarized"}
    ),
)"#,
      settings: None,
    },
    (Adapter::PydanticAi, Endpoint::OpenRouter) => Client {
      imports: &[
        "from openai import AsyncOpenAI",
        "from pydantic_ai.models.openrouter import OpenRouterModel",
        "from pydantic_ai.providers.openrouter import OpenRouterProvider",
      ],
      deps: &["pydantic-ai-slim[openrouter]"],
      body: r#"client = AsyncOpenAI(base_url=url + "/v1", api_key="conformance", max_retries=0)
return OpenRouterModel(
    "anthropic/claude-haiku-5.5", provider=OpenRouterProvider(openai_client=client)
)"#,
      settings: None,
    },
  }
}

/// The adapter's dependencies for every endpoint's client, in endpoint
/// order, without duplicates.
pub fn deps(adapter: Adapter) -> Vec<&'static str> {
  let mut deps = Vec::new();
  for endpoint in Endpoint::ALL {
    for dep in client(adapter, endpoint).deps {
      if !deps.contains(dep) {
        deps.push(*dep);
      }
    }
  }
  deps
}

pub fn endpoint_py(adapter: Adapter) -> String {
  let mut imports: Vec<&str> = Endpoint::ALL
    .iter()
    .flat_map(|e| client(adapter, *e).imports)
    .copied()
    .collect();
  let settings = adapter == Adapter::OpenAiAgents;
  if settings {
    imports.push("from agents import ModelSettings");
  }
  imports.sort_unstable();
  imports.dedup();
  let branches = |body: &dyn Fn(Client) -> Option<&'static str>| -> String {
    Endpoint::ALL
      .iter()
      .filter_map(|e| Some((e, body(client(adapter, *e))?)))
      .map(|(e, body)| {
        let body: String = body
          .lines()
          .map(|l| match l {
            "" => "\n".to_owned(),
            l => format!("        {l}\n"),
          })
          .collect();
        format!("    if endpoint == \"{}\":\n{body}", e.id())
      })
      .collect()
  };
  let model_settings = if settings {
    format!(
      r#"

def model_settings():
    """The agent's model settings for the run's endpoint."""
    endpoint = os.environ["SECTR_CONFORMANCE_ENDPOINT"]
{}    return ModelSettings()
"#,
      branches(&|c| c.settings)
    )
  } else {
    String::new()
  };
  let branches = branches(&|c| Some(c.body));
  format!(
    r#""""The model client for the run's endpoint (`SECTR_CONFORMANCE_ENDPOINT`),
pointed at the harness's mock model under `SECTR_CONFORMANCE_URL`.

Generated by sectr-conformance. Do not edit: run `sectr-conformance gen`.
"""

import os

{imports}


def model():
    url = os.environ["SECTR_CONFORMANCE_URL"]
    endpoint = os.environ["SECTR_CONFORMANCE_ENDPOINT"]
{branches}    raise ValueError(f"unknown conformance endpoint {{endpoint!r}}")
{model_settings}"#,
    imports = imports.join("\n"),
  )
}
