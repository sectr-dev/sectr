//! The mock MCP server, on rmcp's streamable HTTP transport: one per case
//! run whose agent has MCP tools, at `<base>/mcp`. `tools/list` lists the
//! agent's MCP tools; a `tools/call` reaches the case as a [`ToolExec`],
//! exactly like a stub's call, and its outcome becomes the call's result:
//! a returned string as one text block (anything else as its JSON), an MCP
//! result as given ([`ToolOutcome::Mcp`]), a raise as an error result. A
//! tool returning a model ([`Ty::Model`]) declares it as its output schema.

use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use rmcp::model::{
  CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
  PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;

use super::{Incoming, Shared, ToolAnswer, ToolOutcome, Unanswered, error, execute};
use crate::agent::{ToolSpec, Ty};

pub(super) type Service = StreamableHttpService<Handler, LocalSessionManager>;

/// The MCP server for one case run.
pub(super) fn service(
  tools: &'static [ToolSpec],
  tx: mpsc::UnboundedSender<Incoming>,
  ahead: Arc<Mutex<Vec<ToolAnswer>>>,
) -> Service {
  let handler = Handler { tools, tx, ahead };
  StreamableHttpService::new(
    move || Ok(handler.clone()),
    Arc::default(),
    StreamableHttpServerConfig::default(),
  )
}

/// Any request to `<base>/mcp`, handed to the run's MCP server.
pub(super) async fn request(
  State(shared): State<Arc<Shared>>,
  Path(run): Path<String>,
  request: axum::extract::Request,
) -> Response {
  let service = shared
    .runs
    .lock()
    .unwrap()
    .get(&run)
    .map(|state| state.mcp.clone());
  match service {
    None => error(
      StatusCode::NOT_FOUND,
      &format!("no case run `{run}` is registered"),
    ),
    Some(None) => error(
      StatusCode::NOT_FOUND,
      &format!("case run `{run}`'s agent has no MCP tools"),
    ),
    Some(Some(service)) => service.handle(request).await.into_response(),
  }
}

#[derive(Clone)]
pub(super) struct Handler {
  tools: &'static [ToolSpec],
  tx: mpsc::UnboundedSender<Incoming>,
  ahead: Arc<Mutex<Vec<ToolAnswer>>>,
}

impl ServerHandler for Handler {
  fn get_info(&self) -> ServerConfig {
    ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
  }

  async fn list_tools(
    &self,
    _request: Option<PaginatedRequestParams>,
    _context: RequestContext<RoleServer>,
  ) -> Result<ListToolsResult, ErrorData> {
    Ok(ListToolsResult::with_all_items(
      self.tools.iter().map(tool).collect(),
    ))
  }

  async fn call_tool(
    &self,
    request: CallToolRequestParams,
    _context: RequestContext<RoleServer>,
  ) -> Result<CallToolResponse, ErrorData> {
    let name = request.name.to_string();
    if !self.tools.iter().any(|t| t.name == name) {
      return Err(ErrorData::invalid_params(
        format!("no MCP tool `{name}`"),
        None,
      ));
    }
    let args = Value::Object(request.arguments.unwrap_or_default());
    let outcome = match execute(&self.tx, &self.ahead, name, args).await {
      Ok(outcome) => outcome,
      Err(Unanswered::Ended) => {
        return Err(ErrorData::internal_error("the case run has ended", None));
      }
      Err(Unanswered::Dropped) => {
        return Err(ErrorData::internal_error(
          "the case dropped this tool execution without deciding it",
          None,
        ));
      }
    };
    let result = match outcome {
      ToolOutcome::Return(Value::String(text)) => {
        CallToolResult::success(vec![ContentBlock::text(text)])
      }
      ToolOutcome::Return(value) => {
        CallToolResult::success(vec![ContentBlock::text(value.to_string())])
      }
      ToolOutcome::Mcp { text, structured } => {
        let mut result =
          CallToolResult::success(text.into_iter().map(ContentBlock::text).collect());
        result.structured_content = structured;
        result
      }
      ToolOutcome::Raise(message) => CallToolResult::error(vec![ContentBlock::text(message)]),
      // The tool runs here, not in the runner: there's no process to exit.
      ToolOutcome::Exit(_) => {
        return Err(ErrorData::internal_error(
          "an MCP tool can't exit the runner",
          None,
        ));
      }
    };
    Ok(result.into())
  }
}

/// A tool's MCP definition: its parameters as a JSON Schema object, all
/// required, and a returned model's fields as its output schema.
fn tool(spec: &ToolSpec) -> Tool {
  let tool = Tool::new(spec.name, spec.description, object_schema(spec.params));
  match spec.returns {
    Ty::Model(model) => tool.with_raw_output_schema(Arc::new(object_schema(model.fields))),
    _ => tool,
  }
}

/// A JSON Schema object with these properties, all required.
fn object_schema(fields: &[(&str, Ty)]) -> Map<String, Value> {
  let properties: Map<String, Value> = fields
    .iter()
    .map(|(name, ty)| ((*name).to_string(), ty.json_schema()))
    .collect();
  let required: Vec<&str> = fields.iter().map(|(name, _)| *name).collect();
  let schema = json!({"type": "object", "properties": properties, "required": required});
  let Value::Object(schema) = schema else {
    unreachable!("built as an object")
  };
  schema
}

#[cfg(test)]
mod tests {
  use rmcp::ServiceExt;
  use rmcp::model::CallToolRequestParams;
  use rmcp::transport::StreamableHttpClientTransport;
  use serde_json::json;

  use super::*;
  use crate::agent::Model;
  use crate::endpoint::Endpoint;
  use crate::mock::{MockServer, Request};

  const TRACK: ToolSpec = ToolSpec {
    name: "track_parcel",
    description: "Track a parcel.",
    params: &[("order_id", Ty::Str)],
    returns: Ty::Str,
    gated: false,
  };

  const STATUS: Model = Model {
    name: "Status",
    fields: &[("status", Ty::Str)],
  };

  const STATUS_TOOL: ToolSpec = ToolSpec {
    name: "status",
    description: "A status.",
    params: &[],
    returns: Ty::Model(&STATUS),
    gated: false,
  };

  #[tokio::test]
  async fn mcp_tools_are_listed_and_calls_reach_the_case() {
    let server = MockServer::start().await.unwrap();
    let mut run = server.register("r1", Endpoint::OpenAiChat, &[TRACK]);
    let transport = StreamableHttpClientTransport::from_uri(format!("{}/mcp", run.base_url()));
    let client = ().serve(transport).await.unwrap();

    let tools = client.list_all_tools().await.unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "track_parcel");
    assert_eq!(
      Value::Object((*tools[0].input_schema).clone()),
      json!({
        "type": "object",
        "properties": {"order_id": {"type": "string"}},
        "required": ["order_id"],
      })
    );

    for (outcome, texts, structured, is_error) in [
      (
        ToolOutcome::Return(json!("In transit")),
        vec!["In transit"],
        None,
        false,
      ),
      (
        ToolOutcome::Return(json!({"at": "depot"})),
        vec![r#"{"at":"depot"}"#],
        None,
        false,
      ),
      (
        ToolOutcome::Mcp {
          text: vec!["a".into(), "b".into()],
          structured: Some(json!({"n": 1})),
        },
        vec!["a", "b"],
        Some(json!({"n": 1})),
        false,
      ),
      (ToolOutcome::Raise("boom".into()), vec!["boom"], None, true),
    ] {
      let peer = client.peer().clone();
      let call = tokio::spawn(async move {
        let args = json!({"order_id": "42"}).as_object().unwrap().clone();
        peer
          .call_tool(CallToolRequestParams::new("track_parcel").with_arguments(args))
          .await
      });
      let Some(Ok(Request::Tool(exec))) = run.next().await else {
        panic!("expected a tool execution");
      };
      assert_eq!(exec.name(), "track_parcel");
      assert_eq!(exec.args(), &json!({"order_id": "42"}));
      exec.decide(outcome).unwrap();
      let result = call.await.unwrap().unwrap();
      assert_eq!(result.is_error, Some(is_error));
      let got: Vec<&str> = result
        .content
        .iter()
        .map(|c| c.as_text().unwrap().text.as_str())
        .collect();
      assert_eq!(got, texts);
      assert_eq!(result.structured_content, structured);
    }
    client.cancel().await.unwrap();
  }

  #[test]
  fn a_tool_returning_a_model_declares_it_as_its_output_schema() {
    assert_eq!(tool(&TRACK).output_schema, None);
    let schema = tool(&STATUS_TOOL).output_schema.unwrap();
    assert_eq!(
      Value::Object((*schema).clone()),
      json!({
        "type": "object",
        "properties": {"status": {"type": "string"}},
        "required": ["status"],
      })
    );
  }

  #[tokio::test]
  async fn a_run_without_mcp_tools_has_no_mcp_server() {
    let server = MockServer::start().await.unwrap();
    let run = server.register("r1", Endpoint::OpenAiChat, &[]);
    let response = reqwest::Client::new()
      .post(format!("{}/mcp", run.base_url()))
      .json(&json!({}))
      .send()
      .await
      .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
  }
}
