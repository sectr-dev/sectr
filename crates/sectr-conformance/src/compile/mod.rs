//! The compiler: every (scenario, adapter) becomes a sectr project under
//! `sdk-python/tests/conformance/gen/<adapter>/<NN>_<scenario>/`, all members of
//! one uv workspace rooted there. The output is checked in; `gen --check`
//! keeps it in sync.
//!
//! Generated code is case-agnostic: the harness sets `SECTR_CONFORMANCE_URL`
//! and `SECTR_CONFORMANCE_ENDPOINT` per case run, and the agent finds its
//! model and its tools' backend under the URL. Every project gets
//! `endpoint.py` (`model()`, the framework's client for the endpoint). A
//! project with tools also gets `tools.py` (the stubs,
//! written the framework's way) and `conformance.py` (the stubs' client for
//! the backend).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::adapter::Adapter;
use crate::agent::{Model, Subagent, ToolSpec, Ty};
use crate::scenario::Scenario;
use crate::scenarios;

mod endpoint;
mod langgraph;
mod openai_agents;
mod pydantic_ai;

/// File path (relative to the output root) → contents.
pub type Tree = BTreeMap<PathBuf, String>;

/// Paths under the output root that `gen` doesn't own.
const UNOWNED: &[&str] = &["uv.lock", ".venv", "__pycache__", ".agent"];

/// Where `gen` writes by default: `sdk-python/tests/conformance/gen/`.
pub fn default_out() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sdk-python/tests/conformance/gen")
}

/// An adapter's emitters.
struct Backend {
  main_py: fn(&Scenario) -> String,
  /// Only called for scenarios with tools.
  tools_py: fn(&Scenario) -> String,
}

fn backend(adapter: Adapter) -> Backend {
  match adapter {
    Adapter::OpenAiAgents => Backend {
      main_py: openai_agents::main_py,
      tools_py: openai_agents::tools_py,
    },
    Adapter::LangGraph => Backend {
      main_py: langgraph::main_py,
      tools_py: langgraph::tools_py,
    },
    Adapter::PydanticAi => Backend {
      main_py: pydantic_ai::main_py,
      tools_py: pydantic_ai::tools_py,
    },
  }
}

/// The whole generated tree.
pub fn emit_all() -> Tree {
  let mut tree = Tree::new();
  let mut members = Vec::new();
  for (i, scenario) in scenarios::all().iter().enumerate() {
    for &adapter in scenario.adapters() {
      let backend = backend(adapter);
      let dir = project_dir(adapter, i, scenario);
      let name = project_name(adapter, scenario);
      tree.insert(dir.join("sectr.toml"), sectr_toml(&name));
      tree.insert(
        dir.join("pyproject.toml"),
        pyproject(&name, adapter, scenario),
      );
      tree.insert(dir.join("main.py"), (backend.main_py)(scenario));
      tree.insert(dir.join("endpoint.py"), endpoint::endpoint_py(adapter));
      if !local_tools(scenario).is_empty() {
        tree.insert(dir.join("tools.py"), (backend.tools_py)(scenario));
        tree.insert(dir.join("conformance.py"), CONFORMANCE_PY.into());
      }
      members.push(dir);
    }
  }
  tree.insert("pyproject.toml".into(), workspace_pyproject(&members));
  tree.insert(".python-version".into(), "3.12\n".into());
  tree.insert(
    ".gitignore".into(),
    ".venv/\n__pycache__/\n.agent/\n".into(),
  );
  tree
}

/// Write `tree` under `out`, removing owned files that are no longer emitted.
/// Logs one line per file, then a summary.
pub fn write(out: &Path, tree: &Tree) -> Result<()> {
  std::fs::create_dir_all(out)?;
  println!("gen: writing to {}", display_dir(out));
  let (mut created, mut updated, mut unchanged) = (0, 0, 0);
  let mut deps_changed = false;
  for (rel, contents) in tree {
    let path = out.join(rel);
    std::fs::create_dir_all(path.parent().expect("emitted paths have a parent"))?;
    let status = match std::fs::read_to_string(&path) {
      Ok(on_disk) if on_disk == *contents => {
        unchanged += 1;
        "unchanged"
      }
      Ok(_) => {
        updated += 1;
        "updated"
      }
      Err(_) => {
        created += 1;
        "created"
      }
    };
    if status != "unchanged" {
      deps_changed |= is_pyproject(rel);
      std::fs::write(&path, contents).with_context(|| format!("writing {}", path.display()))?;
    }
    println!("  {status:>9}  {}", rel.display());
  }
  let stale = stale(out, tree)?;
  for rel in &stale {
    std::fs::remove_file(out.join(rel))?;
    deps_changed |= is_pyproject(rel);
    println!("  {:>9}  {}", "removed", rel.display());
  }
  println!(
    "gen: {} files ({created} created, {updated} updated, {unchanged} unchanged, {} removed)",
    tree.len(),
    stale.len()
  );
  if deps_changed {
    println!(
      "gen: a pyproject.toml changed; run `uv lock` in {}",
      display_dir(out)
    );
  }
  Ok(())
}

fn is_pyproject(rel: &Path) -> bool {
  rel.file_name().is_some_and(|n| n == "pyproject.toml")
}

/// `out` without `..` segments, for log lines.
fn display_dir(out: &Path) -> String {
  out
    .canonicalize()
    .unwrap_or_else(|_| out.to_path_buf())
    .display()
    .to_string()
}

/// Fail unless the files under `out` are exactly `tree`.
pub fn check(out: &Path, tree: &Tree) -> Result<()> {
  let mut problems = Vec::new();
  for (rel, contents) in tree {
    match std::fs::read_to_string(out.join(rel)) {
      Ok(on_disk) if on_disk == *contents => {}
      Ok(_) => problems.push(format!("differs: {}", rel.display())),
      Err(_) => problems.push(format!("missing: {}", rel.display())),
    }
  }
  for rel in stale(out, tree)? {
    problems.push(format!("stale: {}", rel.display()));
  }
  if !problems.is_empty() {
    bail!(
      "generated conformance projects are out of date (run `sectr-conformance gen`):\n  {}",
      problems.join("\n  ")
    );
  }
  println!(
    "gen --check: {} files up to date in {}",
    tree.len(),
    display_dir(out)
  );
  Ok(())
}

/// Owned files under `out` that `tree` doesn't contain.
fn stale(out: &Path, tree: &Tree) -> Result<Vec<PathBuf>> {
  let mut found = Vec::new();
  if out.exists() {
    walk(out, Path::new(""), &mut found)?;
  }
  Ok(
    found
      .into_iter()
      .filter(|rel| !tree.contains_key(rel))
      .collect(),
  )
}

fn walk(root: &Path, rel: &Path, found: &mut Vec<PathBuf>) -> Result<()> {
  for entry in std::fs::read_dir(root.join(rel))? {
    let entry = entry?;
    let name = entry.file_name();
    if UNOWNED.iter().any(|u| name == *u) {
      continue;
    }
    let child = rel.join(&name);
    if entry.file_type()?.is_dir() {
      walk(root, &child, found)?;
    } else {
      found.push(child);
    }
  }
  Ok(())
}

/// A generated project, relative to the output root. `index` is the
/// scenario's position in the catalog.
pub fn project_dir(adapter: Adapter, index: usize, scenario: &Scenario) -> PathBuf {
  PathBuf::from(adapter.dir()).join(format!("{:02}_{}", index + 1, scenario.id))
}

fn project_name(adapter: Adapter, scenario: &Scenario) -> String {
  format!("conf-{}-{}", adapter.dir(), scenario.id).replace('_', "-")
}

fn sectr_toml(name: &str) -> String {
  format!(
    "{HEADER_TOML}[app]\nname = \"{name}\"\n\n[agent]\nentry = \"main:app\"\npython = \"3.12\"\n"
  )
}

fn pyproject(name: &str, adapter: Adapter, scenario: &Scenario) -> String {
  // The adapter's extra, the clients for every endpoint, and httpx for
  // `conformance.py`.
  let mut deps: Vec<&str> = match adapter {
    Adapter::OpenAiAgents => vec!["sectr[openai-agents]"],
    Adapter::LangGraph => vec!["sectr[langgraph]", "langchain>=1.0"],
    Adapter::PydanticAi => vec!["sectr[pydantic-ai]"],
  };
  // The framework's MCP client (openai-agents ships one).
  if has_mcp(scenario) {
    match adapter {
      Adapter::OpenAiAgents => {}
      Adapter::LangGraph => deps.push("langchain-mcp-adapters"),
      Adapter::PydanticAi => deps.push("pydantic-ai-slim[mcp]"),
    }
  }
  deps.extend(endpoint::deps(adapter));
  deps.push("httpx");
  let deps = deps
    .iter()
    .map(|d| format!("\"{d}\""))
    .collect::<Vec<_>>()
    .join(", ");
  format!(
    "{HEADER_TOML}[project]\nname = \"{name}\"\nversion = \"0.0.0\"\nrequires-python = \">=3.12\"\n\
     dependencies = [{deps}]\n"
  )
}

fn workspace_pyproject(members: &[PathBuf]) -> String {
  let members: String = members
    .iter()
    .map(|m| format!("    \"{}\",\n", m.display()))
    .collect();
  format!(
    "{HEADER_TOML}# One uv workspace (one lock, one venv) for every generated project.\n\
     [tool.uv.workspace]\nmembers = [\n{members}]\n\n\
     [tool.uv.sources]\nsectr = {{ path = \"../../..\", editable = true }}\n"
  )
}

const HEADER_TOML: &str =
  "# Generated by sectr-conformance. Do not edit: run `sectr-conformance gen`.\n\n";

/// A Python string literal. JSON string escapes are valid Python.
fn py_str(s: &str) -> String {
  serde_json::to_string(s).expect("strings serialize")
}

/// The scenario's tools as async stubs calling the backend, each under its
/// `decorator` (if any), two blank lines before each.
fn tool_stubs(scenario: &Scenario, decorator: fn(&ToolSpec) -> Option<&'static str>) -> String {
  local_tools(scenario)
    .into_iter()
    .map(|t| {
      assert!(
        !t.description.contains("\"\"\""),
        "{}: a description can't contain triple quotes",
        t.name
      );
      let params: Vec<String> = t
        .params
        .iter()
        .map(|(p, ty)| format!("{p}: {}", ty.python()))
        .collect();
      let args: String = t.params.iter().map(|(p, _)| format!(", {p}={p}")).collect();
      let decorator = decorator(t).map(|d| format!("{d}\n")).unwrap_or_default();
      let call = format!("await run_tool({}{args})", py_str(t.name));
      let value = match t.returns {
        Ty::Model(model) => format!("{}.model_validate({call})", model.name),
        Ty::Py(py) => py.build.replace("{}", &call),
        _ => call,
      };
      format!(
        "\n\n{decorator}async def {name}({params}) -> {ret}:\n    \"\"\"{description}\"\"\"\n    \
         return {value}\n",
        name = t.name,
        params = params.join(", "),
        ret = t.returns.python(),
        description = t.description,
      )
    })
    .collect()
}

/// The pydantic models the scenario's tools return ([`Ty::Model`]): the
/// `BaseModel` import for `tools.py`'s third-party block, and the classes,
/// each once, to go ahead of the stubs.
struct Models {
  import: &'static str,
  classes: String,
}

fn models(scenario: &Scenario) -> Models {
  let mut seen: Vec<&Model> = Vec::new();
  for t in local_tools(scenario) {
    if let Ty::Model(model) = t.returns
      && !seen.iter().any(|m| m.name == model.name)
    {
      seen.push(model);
    }
  }
  if seen.is_empty() {
    return Models {
      import: "",
      classes: String::new(),
    };
  }
  let classes = seen
    .iter()
    .map(|m| {
      assert!(
        m.fields
          .iter()
          .all(|(_, ty)| matches!(ty, Ty::Str | Ty::Int)),
        "{}: model fields are plain types",
        m.name
      );
      let fields: String = m
        .fields
        .iter()
        .map(|(name, ty)| format!("    {name}: {}\n", ty.python()))
        .collect();
      format!("\n\nclass {}(BaseModel):\n{fields}", m.name)
    })
    .collect();
  Models {
    import: "from pydantic import BaseModel\n",
    classes,
  }
}

/// The agent's structured output model, if any (`AgentSpec::output`): its
/// import, the `Answer` class (after the imports), and `arg`, the agent
/// argument naming it.
struct Output {
  import: &'static str,
  class: String,
  arg: String,
}

fn output(scenario: &Scenario, param: &str) -> Output {
  let Some(fields) = scenario.agent.output else {
    return Output {
      import: "",
      class: String::new(),
      arg: String::new(),
    };
  };
  assert!(
    fields.iter().all(|(_, ty)| !ty.uses_any()),
    "output fields are plain types"
  );
  let fields: String = fields
    .iter()
    .map(|(name, ty)| format!("    {name}: {}\n", ty.python()))
    .collect();
  Output {
    import: "from pydantic import BaseModel\n",
    class: format!("\n\nclass Answer(BaseModel):\n{fields}\n"),
    arg: format!("    {param}=Answer,\n"),
  }
}

/// `tools.py`'s standard-library imports, as their own block: `typing.Any`
/// if its stubs need it, and what each [`Ty::Py`] return needs.
fn std_imports(scenario: &Scenario) -> String {
  let tools = local_tools(scenario);
  let mut imports: Vec<&str> = tools
    .iter()
    .filter_map(|t| match t.returns {
      Ty::Py(py) => py.import,
      _ => None,
    })
    .collect();
  if tools
    .iter()
    .any(|t| t.returns.uses_any() || t.params.iter().any(|(_, ty)| ty.uses_any()))
  {
    imports.push("from typing import Any");
  }
  imports.sort_unstable();
  imports.dedup();
  if imports.is_empty() {
    String::new()
  } else {
    format!("{}\n\n", imports.join("\n"))
  }
}

/// The harness's MCP server URL, as a Python expression.
const MCP_URL: &str = r#"os.environ["SECTR_CONFORMANCE_URL"] + "/mcp""#;

/// Whether the agent has MCP tools ([`crate::agent::AgentSpec::mcp`]).
fn has_mcp(scenario: &Scenario) -> bool {
  !scenario.agent.mcp.tools.is_empty()
}

/// The MCP tools gated in code, as a Python collection's items: `"a", "b"`.
/// Each backend gates them the framework's way.
fn gated_mcp(scenario: &Scenario) -> Option<String> {
  let names: Vec<String> = scenario
    .agent
    .mcp
    .tools
    .iter()
    .filter(|t| t.gated)
    .map(|t| py_str(t.name))
    .collect();
  (!names.is_empty()).then(|| names.join(", "))
}

/// The stubs in `tools.py`: the agent's tools, then its subagents', then
/// its handoff agents' (each name once).
fn local_tools(scenario: &Scenario) -> Vec<&'static ToolSpec> {
  let mut tools: Vec<&'static ToolSpec> = Vec::new();
  let inner = subagents(scenario).into_iter().flat_map(|a| a.tools);
  let handed = scenario.agent.handoffs.iter().flat_map(|a| a.tools);
  for t in scenario.agent.tools.iter().chain(inner).chain(handed) {
    if !tools.iter().any(|known| known.name == t.name) {
      tools.push(t);
    }
  }
  tools
}

/// The agents the subagent tools run, each once.
fn subagents(scenario: &Scenario) -> Vec<&'static Subagent> {
  let mut agents: Vec<&'static Subagent> = Vec::new();
  for t in scenario.agent.subagents {
    if !agents.iter().any(|a| a.name == t.agent.name) {
      agents.push(t.agent);
    }
  }
  agents
}

/// Every stub's name, comma-separated, for `tools.py`'s import.
fn tool_names(scenario: &Scenario) -> String {
  let names: Vec<&str> = local_tools(scenario).iter().map(|t| t.name).collect();
  names.join(", ")
}

/// Names, comma-separated.
fn names(tools: &[ToolSpec]) -> String {
  let names: Vec<&str> = tools.iter().map(|t| t.name).collect();
  names.join(", ")
}

/// The tool stubs' client for the harness's tool backend.
const CONFORMANCE_PY: &str = include_str!("conformance.py");

#[cfg(test)]
mod tests {
  use super::*;

  /// The checked-in tree is the golden set: it must match what we emit.
  #[test]
  fn checked_in_tree_is_current() {
    check(&default_out(), &emit_all()).unwrap();
  }

  #[test]
  fn py_str_escapes() {
    assert_eq!(py_str("a \"b\"\n"), r#""a \"b\"\n""#);
  }
}
