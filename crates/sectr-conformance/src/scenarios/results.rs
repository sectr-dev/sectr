//! Tool results of each JSON type, pydantic models, and Python values JSON
//! has no form for, in both modes. The rule (revised 2026-10-12): a later
//! turn sends the model exactly what the first turn sent, so the provider's
//! prompt cache still matches. The adapter journals what the framework
//! stores for the result, when that comes through the journal unchanged: a
//! string does, an object's key order doesn't (the worker sorts keys).
//!
//! Here every framework stores a string, and the journal holds it:
//! openai-agents `str(value)` in the `function_call_output`; langgraph's
//! `ToolNode` the `ToolMessage` text (`json.dumps(value)`, else
//! `str(value)`). pydantic-ai stores the value itself and renders it per
//! provider, so its adapter replaces the result with its
//! `model_response_str()` (compact JSON, `""` for `None`), which every
//! provider then gets. Each case checks the first turn (what the
//! model gets, what's journaled) and the next one (the result replayed as
//! sent).

use anyhow::Result;
use serde_json::{Value, json};

use crate::adapter::Adapter;
use crate::agent::{AgentSpec, Mcp, Model, PyType, ToolSpec, Ty};
use crate::case;
use crate::case::{Case, call, exec, text};
use crate::ev;
use crate::msg::{assistant, tool_call, tool_result, user};
use crate::scenario::{CaseDef, Scenario, both_modes};

const fn order_tool(name: &'static str, description: &'static str, returns: Ty) -> ToolSpec {
  ToolSpec {
    name,
    description,
    params: &[("order_id", Ty::Str)],
    returns,
    gated: false,
  }
}

const TOOLS: &[ToolSpec] = &[
  order_tool("order_status", "The status of an order.", Ty::Str),
  order_tool("order_total", "An order's total, in cents.", Ty::Int),
  order_tool("order_details", "An order's details.", Ty::Object),
  order_tool("order_items", "The items in an order.", Ty::List),
  order_tool("order_note", "An order's note, if any.", Ty::Optional),
  order_tool("order_summary", "An order's summary.", Ty::Model(&SUMMARY)),
  order_tool(
    "order_shipment",
    "An order's shipment.",
    Ty::Model(&SHIPMENT),
  ),
  order_tool("order_record", "An order's record.", Ty::Py(&UNSORTED)),
  order_tool(
    "order_placed",
    "When an order was placed.",
    Ty::Py(&DATETIME),
  ),
  order_tool("order_ref", "An order's reference.", Ty::Py(&UUID)),
  order_tool(
    "order_balance",
    "An order's balance, in dollars.",
    Ty::Py(&DECIMAL),
  ),
  order_tool("order_label", "An order's shipping label.", Ty::Py(&BYTES)),
  order_tool(
    "order_line",
    "An order's first line: item and quantity.",
    Ty::Py(&TUPLE),
  ),
];

/// What `order_summary` returns: the stub builds one from the backend's
/// value (its fields, as an object).
const SUMMARY: Model = Model {
  name: "OrderSummary",
  fields: &[("id", Ty::Str), ("status", Ty::Str)],
};

/// A model whose fields aren't in alphabetical order, which the journal
/// would sort if it held the model as an object.
const SHIPMENT: Model = Model {
  name: "Shipment",
  fields: &[("status", Ty::Str), ("id", Ty::Str)],
};

/// A dict whose keys aren't in alphabetical order: the backend's object
/// (which comes sorted), its keys reversed.
const UNSORTED: PyType = PyType {
  python: "dict[str, Any]",
  import: Some("from typing import Any"),
  build: "dict(sorted(({}).items(), reverse=True))",
};

const DATETIME: PyType = PyType {
  python: "datetime",
  import: Some("from datetime import datetime"),
  build: "datetime.fromisoformat({})",
};

const UUID: PyType = PyType {
  python: "UUID",
  import: Some("from uuid import UUID"),
  build: "UUID({})",
};

const DECIMAL: PyType = PyType {
  python: "Decimal",
  import: Some("from decimal import Decimal"),
  build: "Decimal({})",
};

const BYTES: PyType = PyType {
  python: "bytes",
  import: None,
  build: "({}).encode()",
};

const TUPLE: PyType = PyType {
  python: "tuple[str, int]",
  import: None,
  build: "tuple({})",
};

pub fn scenarios() -> Vec<Scenario> {
  both_modes(
    "results",
    AgentSpec {
      instructions: "You are a customer-support agent for a small shop.",
      tools: TOOLS,
      output: None,
      mcp: Mcp::NONE,
      subagents: &[],
      handoffs: &[],
    },
    |_mode| {
      let case = |id: &'static str, run| CaseDef::new(id, run);
      vec![
        case("string", case!(string)),
        case("number", case!(number)),
        case("object", case!(object)),
        case("list", case!(list)),
        case("model", case!(model)),
        case("null", case!(null)),
        case("unsorted_object", case!(unsorted_object)),
        case("tuple", case!(tuple)),
        case("unsorted_model", case!(unsorted_model)),
        case("datetime", case!(datetime)),
        case("uuid", case!(uuid)),
        case("decimal", case!(decimal)),
        case("bytes", case!(bytes)),
      ]
    },
  )
}

/// What a tool returns.
#[derive(Clone)]
enum Returned {
  /// A JSON value, as is.
  Value(Value),
  /// A pydantic model, which the stub builds from these fields.
  Model(&'static Model, Value),
  /// A [`Ty::Py`] value, which the stub builds from `backend`.
  Py(Value, Stored),
}

/// What each framework stores for a [`Returned::Py`] value (established
/// by running each one).
#[derive(Clone, Copy)]
struct Stored {
  /// `str(value)`.
  openai_agents: &'static str,
  /// `json.dumps(value)`, else `str(value)`.
  langgraph: &'static str,
  /// `model_response_str()`.
  pydantic_ai: &'static str,
}

impl Returned {
  /// What the tool backend answers with.
  fn backend(&self) -> Value {
    match self {
      Returned::Value(value) | Returned::Model(_, value) | Returned::Py(value, _) => value.clone(),
    }
  }
}

/// The string the framework stores for a tool's result: what the model
/// gets on every provider, on the first turn and every later one, and what
/// the journal holds as `TOOL_RESULT.result`. A string is as is.
fn stored(adapter: Adapter, returned: &Returned) -> String {
  match (adapter, returned) {
    (_, Returned::Value(Value::String(s))) => s.clone(),
    (Adapter::OpenAiAgents, Returned::Py(_, py)) => py.openai_agents.into(),
    (Adapter::LangGraph, Returned::Py(_, py)) => py.langgraph.into(),
    (Adapter::PydanticAi, Returned::Py(_, py)) => py.pydantic_ai.into(),
    // The `function_call_output` item's `output`: `str(value)`.
    (Adapter::OpenAiAgents, Returned::Value(value)) => py_repr(value),
    (Adapter::OpenAiAgents, Returned::Model(model, fields)) => py_model_str(model, fields),
    // `ToolMessage.content`: `json.dumps(value)`, or `str(value)` when that
    // fails, as it does on a model.
    (Adapter::LangGraph, Returned::Value(value)) => py_json(value),
    (Adapter::LangGraph, Returned::Model(model, fields)) => py_model_str(model, fields),
    // `ToolReturnPart.model_response_str()`: empty for `None`, else compact
    // JSON.
    (Adapter::PydanticAi, Returned::Value(Value::Null)) => String::new(),
    (Adapter::PydanticAi, Returned::Value(value)) => value.to_string(),
    (Adapter::PydanticAi, Returned::Model(model, fields)) => model_json(model, fields),
  }
}

/// A pydantic model's `str()`: `field=repr(value)`, space-separated, in
/// the order the class declares its fields.
fn py_model_str(model: &Model, fields: &Value) -> String {
  let fields: Vec<String> = model
    .fields
    .iter()
    .map(|(name, _)| format!("{name}={}", py_repr(&fields[*name])))
    .collect();
  fields.join(" ")
}

/// A pydantic model's compact JSON (`model_dump_json()`), in the order the
/// class declares its fields.
fn model_json(model: &Model, fields: &Value) -> String {
  let fields: Vec<String> = model
    .fields
    .iter()
    .map(|(name, _)| format!("{}:{}", json!(name), fields[*name]))
    .collect();
  format!("{{{}}}", fields.join(","))
}

/// Python's `repr` of a JSON value (as `str()` renders a dict or list).
fn py_repr(value: &Value) -> String {
  match value {
    Value::Null => "None".into(),
    Value::Bool(b) => if *b { "True" } else { "False" }.into(),
    Value::Number(n) => n.to_string(),
    Value::String(s) => format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'")),
    Value::Array(items) => {
      let items: Vec<String> = items.iter().map(py_repr).collect();
      format!("[{}]", items.join(", "))
    }
    Value::Object(map) => {
      let items: Vec<String> = map
        .iter()
        .map(|(k, v)| format!("{}: {}", py_repr(&Value::String(k.clone())), py_repr(v)))
        .collect();
      format!("{{{}}}", items.join(", "))
    }
  }
}

/// Python's `json.dumps` of a JSON value (`", "` and `": "` separators).
fn py_json(value: &Value) -> String {
  match value {
    Value::Array(items) => {
      let items: Vec<String> = items.iter().map(py_json).collect();
      format!("[{}]", items.join(", "))
    }
    Value::Object(map) => {
      let items: Vec<String> = map
        .iter()
        .map(|(k, v)| format!("{}: {}", Value::String(k.clone()), py_json(v)))
        .collect();
      format!("{{{}}}", items.join(", "))
    }
    scalar => scalar.to_string(),
  }
}

const ASK: &str = "Tell me about order 42.";
const SAY: &str = "Let me look.";
const ANSWER: &str = "Here you go.";

/// The model calls `name` on order 42, which returns `returned`; the next
/// turn's request replays the result exactly as the first turn sent it.
async fn returns(h: &mut Case, name: &str, returned: Returned) -> Result<()> {
  let (ask, say, answer) = (ASK, SAY, ANSWER);
  let order = json!({"order_id": "42"});
  let result = stored(h.adapter, &returned);
  h.user(ask).await?;
  h.model()
    .await?
    .expect_history(&[user(ask)])?
    .reply(text(say).and(call("call_1", name, order.clone())))
    .await?;
  h.tool(exec(name, order.clone()).returns(returned.backend()))
    .await?;
  let mut history = vec![
    user(ask),
    assistant(say),
    tool_call("call_1", name, order.clone()),
    tool_result("call_1", &result),
  ];
  h.model()
    .await?
    .expect_history(&history)?
    .reply(text(answer))
    .await?;
  h.journal(&[
    ev::assistant(say),
    ev::tool_call("call_1", name, order),
    ev::tool_result("call_1", json!(result)),
    ev::assistant(answer),
    ev::turn_end(),
  ])
  .await?;
  // The adapter rebuilds this turn's history from the journal, not from
  // the SDK's run: it must send the result as the first turn did. A later
  // turn mustn't rewrite earlier history: it breaks the prompt cache.
  let (next, reply) = ("Thanks. Anything else?", "That's all.");
  h.user(next).await?;
  history.extend([assistant(answer), user(next)]);
  h.model()
    .await?
    .expect_history(&history)?
    .reply(text(reply))
    .await?;
  h.journal(&[ev::assistant(reply), ev::turn_end()]).await
}

async fn string(h: &mut Case) -> Result<()> {
  returns(h, "order_status", Returned::Value(json!("shipped"))).await
}

async fn number(h: &mut Case) -> Result<()> {
  returns(h, "order_total", Returned::Value(json!(4200))).await
}

async fn object(h: &mut Case) -> Result<()> {
  let value = json!({"id": "42", "status": "shipped", "items": 2});
  returns(h, "order_details", Returned::Value(value)).await
}

async fn list(h: &mut Case) -> Result<()> {
  returns(h, "order_items", Returned::Value(json!(["socks", "hat"]))).await
}

/// `None`.
async fn null(h: &mut Case) -> Result<()> {
  returns(h, "order_note", Returned::Value(json!(null))).await
}

async fn model(h: &mut Case) -> Result<()> {
  let fields = json!({"id": "42", "status": "shipped"});
  returns(h, "order_summary", Returned::Model(&SUMMARY, fields)).await
}

/// A dict whose keys aren't in alphabetical order.
async fn unsorted_object(h: &mut Case) -> Result<()> {
  let value = json!({"id": "42", "items": 2, "status": "shipped"});
  let stored = Stored {
    openai_agents: "{'status': 'shipped', 'items': 2, 'id': '42'}",
    langgraph: r#"{"status": "shipped", "items": 2, "id": "42"}"#,
    pydantic_ai: r#"{"status":"shipped","items":2,"id":"42"}"#,
  };
  returns(h, "order_record", Returned::Py(value, stored)).await
}

/// A model whose fields aren't in alphabetical order.
async fn unsorted_model(h: &mut Case) -> Result<()> {
  let fields = json!({"id": "42", "status": "shipped"});
  returns(h, "order_shipment", Returned::Model(&SHIPMENT, fields)).await
}

async fn datetime(h: &mut Case) -> Result<()> {
  let stored = Stored {
    openai_agents: "2026-10-12 09:30:00",
    langgraph: "2026-10-12 09:30:00",
    pydantic_ai: r#""2026-10-12T09:30:00""#,
  };
  let returned = Returned::Py(json!("2026-10-12T09:30:00"), stored);
  returns(h, "order_placed", returned).await
}

async fn uuid(h: &mut Case) -> Result<()> {
  const ID: &str = "6f1c2d3e-4a5b-4c6d-8e7f-9a0b1c2d3e4f";
  let stored = Stored {
    openai_agents: ID,
    langgraph: ID,
    pydantic_ai: r#""6f1c2d3e-4a5b-4c6d-8e7f-9a0b1c2d3e4f""#,
  };
  returns(h, "order_ref", Returned::Py(json!(ID), stored)).await
}

async fn decimal(h: &mut Case) -> Result<()> {
  let stored = Stored {
    openai_agents: "42.50",
    langgraph: "42.50",
    pydantic_ai: r#""42.50""#,
  };
  returns(h, "order_balance", Returned::Py(json!("42.50"), stored)).await
}

/// `b"shipped"`: pydantic-ai serializes bytes as base64.
async fn bytes(h: &mut Case) -> Result<()> {
  let stored = Stored {
    openai_agents: "b'shipped'",
    langgraph: "b'shipped'",
    pydantic_ai: r#""c2hpcHBlZA==""#,
  };
  returns(h, "order_label", Returned::Py(json!("shipped"), stored)).await
}

async fn tuple(h: &mut Case) -> Result<()> {
  let stored = Stored {
    openai_agents: "('socks', 2)",
    langgraph: r#"["socks", 2]"#,
    pydantic_ai: r#"["socks",2]"#,
  };
  returns(h, "order_line", Returned::Py(json!(["socks", 2]), stored)).await
}
