//! `sectr init`: scaffold a new agent project —
//! the first-run experience.
//!
//! Flow: uv check (hard-error BEFORE anything is written) → interactive
//! framework picker when `--template` is absent, then a frontend picker
//! unless `--frontend` was given (crossterm; non-TTY stdin errors with the
//! `--template` escape hatch) → node/npm check when the frontend needs them
//! (also before anything is written) → name resolution (arg / prompt /
//! default) → target-dir safety (refuse non-empty unless `--force`) → emit
//! the template files with stamps → `uv sync` → `sectr inspect` smoke check
//! (real import — the same machinery deploy uses; on failure the error is
//! printed and the files KEPT — debuggable, not a black hole) → `npm
//! install` when there's a frontend → next steps.
//!
//! Where it runs: a third picker (or `--endpoint lambda|cloud-run|container`)
//! chooses Sectr's runners or the developer's own platform. Choosing a
//! platform fetches the server's base image and ingest URL right after the
//! pickers, before anything is written, and adds a `Dockerfile`, a
//! `.dockerignore` and `[deploy] endpoint = ""` to the agent. In a directory
//! that already holds a `sectr.toml`, `sectr init --endpoint <platform>`
//! (no name) adds only those (see [`crate::endpoint`]).
//!
//! Layout: without a frontend the project directory IS the agent. With
//! one, it's the `sectr-copilotkit-starter` layout: the Next.js app at the
//! root and the agent in `agent/` (byte-for-byte what `--frontend none`
//! emits), so `sectr deploy` from `agent/` never packages the Next app or
//! its `.next/` build output.
//!
//! The templates are SEPARATE artifacts from `examples/`:
//! simplified variants on the standard OpenAI API + key-free
//! mocks. Both frameworks serve the same support agent on the same `/chat`
//! route with the same file names, so the docs, the next steps and the
//! frontend don't depend on the framework. The frontend is the
//! `sectr-copilotkit-starter` app cut down to one route; changes to either
//! copy are made in both by hand.

use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use clap::Parser;
use crossterm::event::{Event, KeyCode, KeyModifiers, read};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

use crate::endpoint::Platform;

/// Stamp tokens in the template files.
const STAMP_APP_NAME: &str = "__SECTR_APP_NAME__";
const STAMP_PROJECT_NAME: &str = "__SECTR_PROJECT_NAME__";
const STAMP_VERSION: &str = "__SECTR_VERSION__";
const DEFAULT_NAME: &str = "my-agent";
/// With a frontend, the agent lives in this subdirectory.
const AGENT_DIR: &str = "agent";
/// The oldest Node the frontend supports (`package.json` `engines`).
const MIN_NODE_MAJOR: u32 = 20;

#[derive(Parser, Debug)]
pub struct InitArgs {
  /// Project directory name (created if missing; refused if non-empty
  /// unless --force).
  pub name: Option<String>,
  /// Agent framework: `openai-agents`, `langgraph` or `pydantic-ai`. Given → fully
  /// non-interactive (docs/CI). Omitted → interactive picker (needs a
  /// terminal).
  #[arg(long)]
  pub template: Option<String>,
  /// Chat frontend: `none` (agent only) or `copilotkit` (a Next.js +
  /// CopilotKit app at the root, the agent in `agent/`; needs Node 20+).
  /// Omitted → `none` with --template, else an interactive picker.
  #[arg(long)]
  pub frontend: Option<String>,
  /// Run it on your own platform: `lambda`, `cloud-run` or `container`.
  /// Adds a Dockerfile pinned to your server and `[deploy] endpoint` to
  /// sectr.toml; in an existing project (a sectr.toml here, no name), adds
  /// only those. Needs a server (`sectr auth login`, or --api).
  /// Omitted → Sectr's runners with --template, else an interactive picker.
  #[arg(long)]
  pub endpoint: Option<String>,
  /// The server whose base image and ingest URL an endpoint's Dockerfile
  /// pins. Defaults through `sectr auth login` (or SECTR_API_URL).
  #[arg(long)]
  pub api: Option<String>,
  /// Platform API key (`skp_…`). Defaults through `sectr auth login` (or
  /// SECTR_PLATFORM_KEY).
  #[arg(long)]
  pub key: Option<String>,
  /// Scaffold into a non-empty directory anyway (and, with --endpoint,
  /// replace an existing Dockerfile and .dockerignore).
  #[arg(long)]
  pub force: bool,
}

/// One picker entry: the value, or None for a grayed "coming soon" entry.
type PickerItem<T> = (Option<T>, &'static str);

/// The framework catalog. The grayed-out entries exist so the picker
/// teaches the roadmap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Template {
  OpenAiAgents,
  LangGraph,
  PydanticAi,
}

impl Template {
  /// All picker entries in display order (None = grayed "coming soon").
  pub fn picker_items() -> &'static [PickerItem<Template>] {
    &[
      (Some(Template::OpenAiAgents), "OpenAI Agents SDK"),
      (Some(Template::LangGraph), "LangGraph"),
      (Some(Template::PydanticAi), "Pydantic AI"),
      (None, "Claude Agent SDK (coming soon)"),
    ]
  }

  fn from_arg(arg: &str) -> anyhow::Result<Self> {
    match arg {
      "openai-agents" => Ok(Template::OpenAiAgents),
      "langgraph" => Ok(Template::LangGraph),
      "pydantic-ai" => Ok(Template::PydanticAi),
      other => bail!(
        "unknown template {other:?} — the templates are `openai-agents`, \
                 `langgraph` and `pydantic-ai` (Claude Agent SDK is coming)"
      ),
    }
  }

  /// The `--template` value (and the manifest's route `framework`).
  fn arg(&self) -> &'static str {
    match self {
      Template::OpenAiAgents => "openai-agents",
      Template::LangGraph => "langgraph",
      Template::PydanticAi => "pydantic-ai",
    }
  }

  /// Pyproject dependency spec: `>=`, NOT `==` — alpha users WANT the new
  /// breaking changes (the release train publishes the SDK at the same
  /// version as the CLI). Asserted in
  /// tests; the template's pyproject.toml carries the literal form.
  #[cfg_attr(not(test), allow(dead_code))]
  fn sdk_dependency(&self) -> &'static str {
    match self {
      Template::OpenAiAgents => "sectr[openai-agents]",
      Template::LangGraph => "sectr[langgraph]",
      Template::PydanticAi => "sectr[pydantic-ai]",
    }
  }

  /// The agent files: (relative path, contents). Order = write order
  /// = printed summary order. Raw contents carry the stamps; `scaffold`
  /// applies them. All templates emit the same file names.
  fn files(&self) -> Vec<(&'static str, &'static str)> {
    let mut files = match self {
      Template::OpenAiAgents => vec![
        ("app.py", include_str!("../templates/openai-agents/app.py")),
        (
          "fake.py",
          include_str!("../templates/openai-agents/fake.py"),
        ),
        (
          "tools.py",
          include_str!("../templates/openai-agents/tools.py"),
        ),
      ],
      Template::LangGraph => vec![
        ("app.py", include_str!("../templates/langgraph/app.py")),
        ("fake.py", include_str!("../templates/langgraph/fake.py")),
        ("tools.py", include_str!("../templates/langgraph/tools.py")),
      ],
      Template::PydanticAi => vec![
        ("app.py", include_str!("../templates/pydantic-ai/app.py")),
        ("fake.py", include_str!("../templates/pydantic-ai/fake.py")),
        (
          "tools.py",
          include_str!("../templates/pydantic-ai/tools.py"),
        ),
      ],
    };
    // sectr.toml doesn't depend on the framework.
    files.push((
      "sectr.toml",
      include_str!("../templates/openai-agents/sectr.toml"),
    ));
    files.extend(match self {
      Template::OpenAiAgents => [
        (
          "pyproject.toml",
          include_str!("../templates/openai-agents/pyproject.toml"),
        ),
        (
          "README.md",
          include_str!("../templates/openai-agents/README.md"),
        ),
        (
          ".env.example",
          include_str!("../templates/openai-agents/.env.example"),
        ),
      ],
      Template::LangGraph => [
        (
          "pyproject.toml",
          include_str!("../templates/langgraph/pyproject.toml"),
        ),
        (
          "README.md",
          include_str!("../templates/langgraph/README.md"),
        ),
        (
          ".env.example",
          include_str!("../templates/langgraph/.env.example"),
        ),
      ],
      Template::PydanticAi => [
        (
          "pyproject.toml",
          include_str!("../templates/pydantic-ai/pyproject.toml"),
        ),
        (
          "README.md",
          include_str!("../templates/pydantic-ai/README.md"),
        ),
        (
          ".env.example",
          include_str!("../templates/pydantic-ai/.env.example"),
        ),
      ],
    });
    // .gitignore is CLI-authored (no template file): the same for
    // every scaffold; git itself is NOT initialized.
    files.push((
      ".gitignore",
      ".venv/\n__pycache__/\n*.pyc\n.env\n.agent/\n.mypy_cache/\n\
                 .pytest_cache/\n.ruff_cache/\n",
    ));
    files
  }
}

/// The optional chat frontend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frontend {
  None,
  CopilotKit,
}

impl Frontend {
  /// Picker entries in display order; the first is the default.
  pub fn picker_items() -> &'static [PickerItem<Frontend>] {
    &[
      (Some(Frontend::None), "None (agent only)"),
      (Some(Frontend::CopilotKit), "CopilotKit chat (Next.js)"),
    ]
  }

  fn from_arg(arg: &str) -> anyhow::Result<Self> {
    match arg {
      "none" => Ok(Frontend::None),
      "copilotkit" => Ok(Frontend::CopilotKit),
      other => bail!("unknown frontend {other:?} — the frontends are `none` and `copilotkit`"),
    }
  }

  fn arg(&self) -> &'static str {
    match self {
      Frontend::None => "none",
      Frontend::CopilotKit => "copilotkit",
    }
  }

  /// The frontend files, written at the project root (the agent goes in
  /// `agent/`). Same conventions as `Template::files`.
  fn files(&self) -> Vec<(&'static str, &'static str)> {
    match self {
      Frontend::None => vec![],
      Frontend::CopilotKit => vec![
        (
          "package.json",
          include_str!("../templates/copilotkit/package.json"),
        ),
        (
          "tsconfig.json",
          include_str!("../templates/copilotkit/tsconfig.json"),
        ),
        (
          "next.config.ts",
          include_str!("../templates/copilotkit/next.config.ts"),
        ),
        (
          "eslint.config.mjs",
          include_str!("../templates/copilotkit/eslint.config.mjs"),
        ),
        ("proxy.ts", include_str!("../templates/copilotkit/proxy.ts")),
        (
          "app/layout.tsx",
          include_str!("../templates/copilotkit/app/layout.tsx"),
        ),
        (
          "app/globals.css",
          include_str!("../templates/copilotkit/app/globals.css"),
        ),
        (
          "app/providers.tsx",
          include_str!("../templates/copilotkit/app/providers.tsx"),
        ),
        (
          "app/chat.tsx",
          include_str!("../templates/copilotkit/app/chat.tsx"),
        ),
        (
          "app/page.tsx",
          include_str!("../templates/copilotkit/app/page.tsx"),
        ),
        (
          "app/api/copilotkit/[[...slug]]/route.ts",
          include_str!("../templates/copilotkit/app/api/copilotkit/[[...slug]]/route.ts"),
        ),
        (
          "app/api/agent-token/route.ts",
          include_str!("../templates/copilotkit/app/api/agent-token/route.ts"),
        ),
        (
          ".env.development",
          include_str!("../templates/copilotkit/.env.development"),
        ),
        (
          ".env.example",
          include_str!("../templates/copilotkit/.env.example"),
        ),
        (
          "README.md",
          include_str!("../templates/copilotkit/README.md"),
        ),
        // Stored without the dot so it doesn't apply to this repo.
        (
          ".gitignore",
          include_str!("../templates/copilotkit/gitignore"),
        ),
      ],
    }
  }
}

/// Where the agent runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunsOn {
  /// `sectr deploy` builds it and Sectr's runners run it.
  Sectr,
  /// The developer's own platform; `sectr deploy` registers its URL.
  Endpoint(Platform),
}

impl RunsOn {
  /// Picker entries in display order; the first is the default.
  pub fn picker_items() -> &'static [PickerItem<RunsOn>] {
    &[
      (
        Some(RunsOn::Sectr),
        "Sectr runs it (sectr deploy builds and runs it)",
      ),
      (
        Some(RunsOn::Endpoint(Platform::Lambda)),
        "Your own AWS Lambda",
      ),
      (
        Some(RunsOn::Endpoint(Platform::CloudRun)),
        "Your own Google Cloud Run",
      ),
      (
        Some(RunsOn::Endpoint(Platform::Container)),
        "Your own container platform (Fly, Render, ECS, …)",
      ),
    ]
  }

  fn arg(&self) -> &'static str {
    match self {
      RunsOn::Sectr => "sectr",
      RunsOn::Endpoint(platform) => platform.arg(),
    }
  }
}

/// Entry point.
pub async fn run_init(args: InitArgs) -> anyhow::Result<()> {
  // Validate a given --endpoint before anything else.
  let endpoint_arg = args
    .endpoint
    .as_deref()
    .map(Platform::from_arg)
    .transpose()?;

  // An existing project: add only the endpoint files.
  if let Some(platform) = endpoint_arg
    && args.name.is_none()
    && is_agent_dir(Path::new("."), platform)?
  {
    return add_endpoint_to_existing(&args, platform).await;
  }

  // Fail BEFORE any interaction or file write: `sectr` requires `uv` —
  // every later step (`uv sync`, the inspector, `sectr dev`) drives it.
  require_uv().await?;

  // Validate a given --frontend before showing any picker.
  let frontend_arg = args
    .frontend
    .as_deref()
    .map(Frontend::from_arg)
    .transpose()?;

  // Template: --template skips the pickers entirely (docs/CI path).
  let (template, frontend, runs_on) = match &args.template {
    Some(arg) => (
      Template::from_arg(arg)?,
      frontend_arg.unwrap_or(Frontend::None),
      endpoint_arg.map_or(RunsOn::Sectr, RunsOn::Endpoint),
    ),
    None => {
      let template = pick(
        "Choose a template",
        "no terminal for the template picker — pass --template \
                 openai-agents, --template langgraph or --template pydantic-ai \
                 (non-interactive)",
        Template::picker_items(),
      )?;
      println!("→ template: {}", template.arg());
      let frontend = match frontend_arg {
        Some(frontend) => frontend,
        None => {
          let frontend = pick(
            "Add a chat frontend?",
            "no terminal for the frontend picker — pass --frontend none \
                     or --frontend copilotkit",
            Frontend::picker_items(),
          )?;
          println!("→ frontend: {}", frontend.arg());
          frontend
        }
      };
      let runs_on = match endpoint_arg {
        Some(platform) => RunsOn::Endpoint(platform),
        None => {
          let runs_on = pick(
            "Where will it run?",
            "no terminal for the where-it-runs picker — pass --endpoint lambda, \
             cloud-run or container to run it on your own platform",
            RunsOn::picker_items(),
          )?;
          println!("→ runs on: {}", runs_on.arg());
          runs_on
        }
      };
      (template, frontend, runs_on)
    }
  };

  // The frontend needs Node: checked before anything is written, like uv.
  if frontend == Frontend::CopilotKit {
    require_node().await?;
  }
  // An endpoint's Dockerfile pins the server's values: also fetched before
  // anything is written.
  let endpoint = match runs_on {
    RunsOn::Sectr => None,
    RunsOn::Endpoint(platform) => Some((
      platform,
      crate::endpoint::fetch_pins(args.api.as_deref(), args.key.as_deref()).await?,
    )),
  };

  // Name: arg → prompt (default my-agent) → validate/normalize.
  let name = match &args.name {
    Some(n) => n.clone(),
    None => prompt_name()?,
  };
  let name = normalize_name(&name).context("project name")?;

  let target = std::env::current_dir()?.join(&name);
  ensure_target_dir(&target, args.force)?;

  let mut written = scaffold(&target, template, frontend, &name)?;
  if let Some((platform, pins)) = &endpoint {
    let agent = agent_dir(&target, frontend);
    crate::endpoint::write_files(&agent, *platform, pins, false)?;
    let prefix = agent.strip_prefix(&target).unwrap_or(Path::new(""));
    written.push(prefix.join("Dockerfile"));
    written.push(prefix.join(".dockerignore"));
  }
  println!("Scaffolded {name} ({} files):", written.len());
  for path in &written {
    println!("  {}", Path::new(&name).join(path).display());
  }

  // Post-scaffold: resolve deps, then the smoke check — a real import,
  // the same machinery `sectr deploy` runs server-side. Failure keeps
  // the files and prints the error: debuggable, not a black hole.
  let agent_dir = agent_dir(&target, frontend);
  let agent_cd = match frontend {
    Frontend::None => name.clone(),
    Frontend::CopilotKit => format!("{name}/{AGENT_DIR}"),
  };
  println!("Running `uv sync` …");
  let status = tokio::process::Command::new("uv")
    .arg("sync")
    .current_dir(&agent_dir)
    .status()
    .await
    .context("running `uv sync` (is uv installed?)")?;
  if !status.success() {
    bail!(
      "`uv sync` failed — the files are kept; fix and rerun \
             (`cd {agent_cd} && uv sync`, then `sectr deploy`)."
    );
  }

  match crate::dev::run_inspect(&agent_dir, "app:app").await {
    Ok(_) => {
      println!("✓ agent imported OK");
    }
    Err(e) => bail!("agent failed to import. please report this as a bug:\n{e:#}"),
  }

  if frontend == Frontend::CopilotKit {
    println!("Running `npm install` …");
    let status = tokio::process::Command::new("npm")
      .arg("install")
      .current_dir(&target)
      .status()
      .await
      .context("running `npm install` (is npm installed?)")?;
    if !status.success() {
      bail!(
        "`npm install` failed — the files are kept; fix and rerun \
               (`cd {name} && npm install`)."
      );
    }
  }

  println!();
  println!("Next steps:");
  match frontend {
    Frontend::None => {
      println!("  cd {name}");
      println!("  sectr dev          # run locally (dashboard opens; mock model — no key needed)");
      println!("  sectr deploy       # ship it (needs `sectr auth login` first)");
    }
    Frontend::CopilotKit => {
      println!("  cd {agent_cd}");
      println!("  sectr dev          # run the agent locally (mock model — no key needed)");
      println!();
      println!("  # then, in a second terminal:");
      println!("  cd {name}");
      println!("  npm run dev        # the chat app → http://localhost:3000");
      println!();
      println!("  See {name}/README.md to deploy both.");
    }
  }
  if let Some((platform, _)) = endpoint {
    println!();
    println!(
      "It runs on {}: `sectr deploy` registers it once it's deployed.",
      platform.name()
    );
    println!("From {agent_cd}:");
    crate::endpoint::print_next_steps(platform, &name);
  }
  Ok(())
}

/// Whether `dir` is an existing project's agent directory (it has a
/// `sectr.toml`). At the root of a project with a frontend, where the
/// agent is in `agent/`, it refuses: going on would scaffold a new project
/// inside that one.
fn is_agent_dir(dir: &Path, platform: Platform) -> anyhow::Result<bool> {
  if dir.join("sectr.toml").is_file() {
    return Ok(true);
  }
  if dir.join(AGENT_DIR).join("sectr.toml").is_file() {
    bail!(
      "this project's agent is in {AGENT_DIR}/, where its sectr.toml is: run \
       `cd {AGENT_DIR} && sectr init --endpoint {}` instead",
      platform.arg()
    );
  }
  Ok(false)
}

/// `sectr init --endpoint <platform>` in a directory with a `sectr.toml`:
/// add the Dockerfile, `.dockerignore` and `[deploy] endpoint`, nothing
/// else.
async fn add_endpoint_to_existing(args: &InitArgs, platform: Platform) -> anyhow::Result<()> {
  let dir = std::env::current_dir()?;
  let pins = crate::endpoint::fetch_pins(args.api.as_deref(), args.key.as_deref()).await?;
  let added = crate::endpoint::write_files(&dir, platform, &pins, args.force)?;
  println!(
    "✓ wrote Dockerfile        ({}, dials {})",
    pins.base_image, pins.ingest_url
  );
  println!("✓ wrote .dockerignore");
  if added {
    println!("✓ added [deploy] endpoint = \"\" to sectr.toml");
  } else {
    println!("✓ sectr.toml already has [deploy] endpoint (kept)");
  }
  println!();
  let name = crate::endpoint::app_name(&dir)
    .or_else(|| dir.file_name().map(|n| n.to_string_lossy().into_owned()))
    .unwrap_or_else(|| DEFAULT_NAME.to_owned());
  let name = normalize_name(&name).unwrap_or_else(|_| DEFAULT_NAME.to_owned());
  crate::endpoint::print_next_steps(platform, &name);
  Ok(())
}

/// Where the agent goes: the project directory itself, or `agent/` under
/// it when there's a frontend.
fn agent_dir(target: &Path, frontend: Frontend) -> PathBuf {
  match frontend {
    Frontend::None => target.to_path_buf(),
    Frontend::CopilotKit => target.join(AGENT_DIR),
  }
}

/// `uv` must be reachable BEFORE anything happens. Delegates to
/// `require_uv_with` so tests can exercise the failure path without
/// mutating PATH.
async fn require_uv() -> anyhow::Result<()> {
  require_uv_with("uv").await
}

async fn require_uv_with(program: &str) -> anyhow::Result<()> {
  match tokio::process::Command::new(program)
    .arg("--version")
    .output()
    .await
  {
    Ok(out) if out.status.success() => Ok(()),
    Ok(out) => bail!(
      "sectr requires `uv` — `{program} --version` failed:\n{}",
      String::from_utf8_lossy(&out.stderr)
    ),
    Err(e) => bail!(
      "sectr requires `uv` — not found ({e}). Install it first:\n  \
             curl -LsSf https://astral.sh/uv/install.sh | sh"
    ),
  }
}

/// The CopilotKit frontend needs Node (>= 20, its `engines`) and npm.
async fn require_node() -> anyhow::Result<()> {
  require_node_with("node", "npm").await
}

async fn require_node_with(node: &str, npm: &str) -> anyhow::Result<()> {
  const HINT: &str = "Install Node.js 20 or newer (https://nodejs.org), or \
                      scaffold without the frontend (--frontend none).";
  let version = match tokio::process::Command::new(node)
    .arg("--version")
    .output()
    .await
  {
    Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_string(),
    Ok(_) | Err(_) => {
      bail!("the CopilotKit frontend requires Node.js — `{node}` not found. {HINT}")
    }
  };
  match node_major(&version) {
    Some(major) if major >= MIN_NODE_MAJOR => {}
    _ => bail!(
      "the CopilotKit frontend requires Node.js {MIN_NODE_MAJOR} or newer — \
             found {version:?}. {HINT}"
    ),
  }
  match tokio::process::Command::new(npm)
    .arg("--version")
    .output()
    .await
  {
    Ok(out) if out.status.success() => Ok(()),
    Ok(_) | Err(_) => bail!("the CopilotKit frontend requires npm — `{npm}` not found. {HINT}"),
  }
}

/// `v20.11.1` → 20.
fn node_major(version: &str) -> Option<u32> {
  version.strip_prefix('v')?.split('.').next()?.parse().ok()
}

/// Interactive picker (bare invocation): up/down to move (only selectable
/// items), enter to confirm, q/Esc/Ctrl-C to abort. Grayed "coming soon"
/// entries are skipped by the cursor and drawn dim. The first enabled item
/// starts selected.
fn pick<T: Copy>(title: &str, no_terminal: &str, items: &[PickerItem<T>]) -> anyhow::Result<T> {
  if !io::stdin().is_terminal() {
    bail!("{no_terminal}");
  }
  let mut sel = items
    .iter()
    .position(|(item, _)| item.is_some())
    .context("picker with no enabled item")?;

  enable_raw_mode().context("terminal raw mode")?;
  let result = (|| {
    let mut stdout = io::stdout();
    let mut first = true;
    loop {
      draw_picker(&mut stdout, title, items, sel, first)?;
      first = false;
      match read().context("reading key event")? {
        Event::Key(key) if key.kind == crossterm::event::KeyEventKind::Press => {
          match key.code {
            KeyCode::Up => sel = move_sel(items, sel, -1),
            KeyCode::Down => sel = move_sel(items, sel, 1),
            KeyCode::Enter => {
              // Cursor can only rest on enabled items.
              return items[sel].0.context("cursor on a grayed item");
            }
            KeyCode::Esc | KeyCode::Char('q') => bail!("aborted"),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
              bail!("aborted")
            }
            _ => {}
          }
        }
        _ => {}
      }
    }
  })();
  disable_raw_mode()?;
  result
}

/// Draw the picker: title + one line per entry, cursor-marked. After the
/// first draw, the cursor is rewound to the title line so each redraw
/// overwrites in place (raw mode: explicit `\r\n`).
fn draw_picker<W: io::Write, T>(
  stdout: &mut W,
  title: &str,
  items: &[PickerItem<T>],
  sel: usize,
  first: bool,
) -> anyhow::Result<()> {
  use crossterm::cursor::{MoveToColumn, MoveUp};
  use crossterm::style::{Color, Print, ResetColor, SetForegroundColor};

  if !first {
    // The previous draw emitted `items.len() + 1` lines (title + one per
    // entry), each terminated by `\r\n`, so the cursor now sits on the
    // line BELOW the block: rewind `picker_height` steps to reach the
    // title line. Moving up only `items.len()` lands on the first item,
    // shifting every redraw down one line — garbled output.
    crossterm::execute!(
      stdout,
      MoveToColumn(0),
      MoveUp(picker_height(items.len()) as u16)
    )?;
  }
  crossterm::execute!(
    stdout,
    Print(format!("{title} (↑/↓, enter to confirm):\r\n"))
  )?;
  for (i, (item, label)) in items.iter().enumerate() {
    let prefix = if i == sel { "❯ " } else { "  " };
    match item {
      Some(_) if i == sel => crossterm::execute!(
        stdout,
        SetForegroundColor(Color::Green),
        Print(format!("{prefix}{label}\r\n")),
        ResetColor
      )?,
      Some(_) => crossterm::execute!(stdout, Print(format!("{prefix}{label}\r\n")))?,
      // Grayed "coming soon" entries.
      None => crossterm::execute!(
        stdout,
        SetForegroundColor(Color::DarkGrey),
        Print(format!("{prefix}{label}\r\n")),
        ResetColor
      )?,
    }
  }
  stdout.flush()?;
  Ok(())
}

/// Height in terminal lines of one picker draw: title + one line per
/// entry. Every line ends in `\r\n`, so a completed draw leaves the
/// cursor one line below the block — rewinding to the title takes
/// exactly this many `MoveUp` steps.
fn picker_height(item_count: usize) -> usize {
  item_count + 1
}

/// Pure cursor math for the picker: move `delta` steps among ENABLED
/// items only (grayed entries are skipped), wrapping. Returns the same
/// index when there is no other enabled item.
fn move_sel<T>(items: &[PickerItem<T>], current: usize, delta: isize) -> usize {
  let n = items.len();
  if n == 0 {
    return current;
  }
  let enabled = |i: usize| items[i].0.is_some();
  if delta == 0 || !enabled(current) {
    return current;
  }
  let step = if delta < 0 { n - 1 } else { 1 };
  let steps = delta.unsigned_abs().min(n);
  let mut i = current;
  for _ in 0..steps {
    i = (i + step) % n;
    while !enabled(i) {
      i = (i + step) % n;
    }
  }
  i
}

/// Name: arg, else a prompt with a default (non-TTY stdin → EOF → default,
/// keeping `--template` CI runs deterministic).
fn prompt_name() -> anyhow::Result<String> {
  if !io::stdin().is_terminal() {
    return Ok(DEFAULT_NAME.into());
  }
  print!("Project name [{DEFAULT_NAME}]: ");
  io::stdout().flush().context("prompt")?;
  let mut line = String::new();
  io::stdin().read_line(&mut line).context("read name")?;
  let trimmed = line.trim();
  if trimmed.is_empty() {
    Ok(DEFAULT_NAME.into())
  } else {
    Ok(trimmed.into())
  }
}

/// Normalize a project name to a pyproject-valid distribution name:
/// lowercase, `[a-z0-9-]` only (spaces/underscores → `-`), collapsed and
/// trimmed dashes. Refuses anything that doesn't survive that (empty).
/// Path separators normalize away — no traversal.
fn normalize_name(raw: &str) -> anyhow::Result<String> {
  let mut out = String::new();
  let mut last_dash = false;
  for ch in raw.trim().chars() {
    let c = if ch.is_ascii_alphanumeric() {
      ch.to_ascii_lowercase()
    } else {
      '-'
    };
    if c == '-' {
      if last_dash || out.is_empty() {
        continue; // collapse + don't lead
      }
      last_dash = true;
      out.push('-');
    } else {
      last_dash = false;
      out.push(c);
    }
  }
  let out = out.trim_end_matches('-').to_string();
  if out.is_empty() {
    bail!("project name {raw:?} normalizes to an empty name");
  }
  Ok(out)
}

/// Target-directory safety: refuse an EXISTING non-empty directory unless
/// `--force`; create the directory when missing (parents included).
fn ensure_target_dir(target: &Path, force: bool) -> anyhow::Result<()> {
  if target.exists() {
    let empty = std::fs::read_dir(target)
      .with_context(|| format!("reading {}", target.display()))?
      .next()
      .is_none();
    if !empty && !force {
      let existing_project = if target.join("sectr.toml").is_file() {
        "\nto run that project on your own platform, cd into it and use \
         `sectr init --endpoint lambda|cloud-run|container`"
      } else {
        ""
      };
      bail!(
        "{} exists and is not empty — refusing to scaffold over it \
                 (pass --force to scaffold anyway){existing_project}",
        target.display()
      );
    }
  } else {
    std::fs::create_dir_all(target).with_context(|| format!("creating {}", target.display()))?;
  }
  Ok(())
}

/// Emit the template files with stamps applied: the agent into `target`,
/// or, with a frontend, the frontend into `target` and the agent into
/// `target/agent`. Returns the paths written, relative to `target`, for the
/// printed summary.
fn scaffold(
  target: &Path,
  template: Template,
  frontend: Frontend,
  app_name: &str,
) -> anyhow::Result<Vec<PathBuf>> {
  let agent_prefix = match frontend {
    Frontend::None => PathBuf::new(),
    Frontend::CopilotKit => PathBuf::from(AGENT_DIR),
  };
  let files = frontend
    .files()
    .into_iter()
    .map(|(path, raw)| (PathBuf::from(path), raw))
    .chain(
      template
        .files()
        .into_iter()
        .map(|(path, raw)| (agent_prefix.join(path), raw)),
    );
  let mut written = Vec::new();
  for (rel, raw) in files {
    let content = raw
      .replace(STAMP_VERSION, env!("CARGO_PKG_VERSION"))
      .replace(STAMP_APP_NAME, app_name)
      .replace(STAMP_PROJECT_NAME, app_name); // name already normalized
    let path = target.join(&rel);
    if let Some(parent) = path.parent() {
      std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&path, content).with_context(|| format!("writing {}", path.display()))?;
    written.push(rel);
  }
  Ok(written)
}

#[cfg(test)]
mod tests {
  use super::*;

  const AGENT_FILES: [&str; 8] = [
    "app.py",
    "fake.py",
    "tools.py",
    "sectr.toml",
    "pyproject.toml",
    "README.md",
    ".env.example",
    ".gitignore",
  ];
  const FRONTEND_FILES: [&str; 16] = [
    "package.json",
    "tsconfig.json",
    "next.config.ts",
    "eslint.config.mjs",
    "proxy.ts",
    "app/layout.tsx",
    "app/globals.css",
    "app/providers.tsx",
    "app/chat.tsx",
    "app/page.tsx",
    "app/api/copilotkit/[[...slug]]/route.ts",
    "app/api/agent-token/route.ts",
    ".env.development",
    ".env.example",
    "README.md",
    ".gitignore",
  ];
  const TEMPLATES: [Template; 3] = [
    Template::OpenAiAgents,
    Template::LangGraph,
    Template::PydanticAi,
  ];

  /// Every file `scaffold` should write for this combination, relative to
  /// the project directory.
  fn expected_files(frontend: Frontend) -> Vec<String> {
    match frontend {
      Frontend::None => AGENT_FILES.iter().map(|f| f.to_string()).collect(),
      Frontend::CopilotKit => FRONTEND_FILES
        .iter()
        .map(|f| f.to_string())
        .chain(AGENT_FILES.iter().map(|f| format!("{AGENT_DIR}/{f}")))
        .collect(),
    }
  }

  #[test]
  fn scaffold_matrix_emits_all_files_with_stamps() {
    let version = env!("CARGO_PKG_VERSION");
    for template in TEMPLATES {
      for frontend in [Frontend::None, Frontend::CopilotKit] {
        let case = format!("{template:?}/{frontend:?}");
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("my-agent");
        ensure_target_dir(&target, false).unwrap();
        let written = scaffold(&target, template, frontend, "my-agent").unwrap();

        let expected = expected_files(frontend);
        let written: Vec<String> = written
          .iter()
          .map(|p| p.to_string_lossy().into_owned())
          .collect();
        assert_eq!(written, expected, "{case}");
        for f in &expected {
          let content = std::fs::read_to_string(target.join(f)).unwrap();
          // No stamps left in ANY emitted file.
          assert!(!content.contains("__SECTR_"), "{case}: stale stamp in {f}");
        }

        let agent = agent_dir(&target, frontend);
        let pyproject = std::fs::read_to_string(agent.join("pyproject.toml")).unwrap();
        // Stamped >= spec with THIS CLI's version; NO editable path dep.
        assert!(
          pyproject.contains(&format!("{}>={version}", template.sdk_dependency())),
          "{case} pyproject:\n{pyproject}"
        );
        assert!(!pyproject.contains("tool.uv.sources"));
        assert!(pyproject.contains("name = \"my-agent\""));

        let toml = std::fs::read_to_string(agent.join("sectr.toml")).unwrap();
        assert!(toml.contains("name = \"my-agent\""));
        assert!(toml.contains("entry = \"app:app\""));
        assert!(toml.contains("python = \"3.12\""));

        let app = std::fs::read_to_string(agent.join("app.py")).unwrap();
        assert!(app.contains("AgentApp(name=\"my-agent\""), "{case}");
        assert!(app.contains("app.add_route(\"/chat\", "), "{case}");

        if frontend == Frontend::CopilotKit {
          let env = std::fs::read_to_string(target.join(".env.development")).unwrap();
          assert!(
            env.contains("SECTR_AGUI_URL=https://local.sectr.dev:8420/ag-ui/my-agent/chat\n"),
            "{case} .env.development:\n{env}"
          );
          let package = std::fs::read_to_string(target.join("package.json")).unwrap();
          assert!(package.contains("\"name\": \"my-agent\""), "{case}");
          assert!(
            package.contains(&format!("\"@sectr/copilotkit\": \">={version}\"")),
            "{case} package.json:\n{package}"
          );
        }
      }
    }
  }

  #[test]
  fn templates_differ_only_in_framework_code() {
    let dir = tempfile::tempdir().unwrap();
    let oai = dir.path().join("oai");
    let lg = dir.path().join("lg");
    scaffold(&oai, Template::OpenAiAgents, Frontend::None, "x").unwrap();
    scaffold(&lg, Template::LangGraph, Frontend::None, "x").unwrap();
    for same in ["sectr.toml", ".env.example", ".gitignore"] {
      assert_eq!(
        std::fs::read_to_string(oai.join(same)).unwrap(),
        std::fs::read_to_string(lg.join(same)).unwrap(),
        "{same}"
      );
    }
    let app = std::fs::read_to_string(lg.join("app.py")).unwrap();
    assert!(app.contains("langgraph(graph)"));

    let pai = dir.path().join("pai");
    scaffold(&pai, Template::PydanticAi, Frontend::None, "x").unwrap();
    for same in ["sectr.toml", ".env.example", ".gitignore"] {
      assert_eq!(
        std::fs::read_to_string(oai.join(same)).unwrap(),
        std::fs::read_to_string(pai.join(same)).unwrap(),
        "{same}"
      );
    }
    let app = std::fs::read_to_string(pai.join("app.py")).unwrap();
    assert!(app.contains("pydantic_ai(agent)"));
  }

  #[test]
  fn dir_safety_refuses_non_empty_without_force() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("occupied");
    std::fs::create_dir(&target).unwrap();
    std::fs::write(target.join("precious.txt"), "keep").unwrap();

    assert!(ensure_target_dir(&target, false).is_err());
    // --force lets it through.
    ensure_target_dir(&target, true).unwrap();
  }

  #[test]
  fn dir_safety_allows_empty_and_missing() {
    let dir = tempfile::tempdir().unwrap();
    let empty = dir.path().join("empty");
    std::fs::create_dir(&empty).unwrap();
    ensure_target_dir(&empty, false).unwrap();

    let missing = dir.path().join("nested").join("fresh");
    ensure_target_dir(&missing, false).unwrap();
    assert!(missing.is_dir());
  }

  #[tokio::test]
  async fn uv_check_fails_for_missing_binary() {
    let err = require_uv_with("sectr-init-definitely-not-a-binary")
      .await
      .expect_err("missing binary must fail");
    let msg = format!("{err:#}");
    assert!(msg.contains("sectr requires `uv`"), "got: {msg}");
  }

  #[tokio::test]
  async fn node_check_fails_for_missing_binaries() {
    let err = require_node_with("sectr-init-no-node", "npm")
      .await
      .expect_err("missing node must fail");
    let msg = format!("{err:#}");
    assert!(msg.contains("requires Node.js"), "got: {msg}");
    assert!(msg.contains("--frontend none"), "got: {msg}");
  }

  #[test]
  fn node_versions() {
    assert_eq!(node_major("v20.11.1"), Some(20));
    assert_eq!(node_major("v24.0.0"), Some(24));
    assert_eq!(node_major("v18.20.4"), Some(18));
    assert_eq!(node_major("20.1.0"), None);
    assert_eq!(node_major("garbage"), None);
  }

  #[test]
  fn picker_redraw_rewinds_to_the_title_line() {
    let items = Template::picker_items();
    let mut buf = Vec::new();
    draw_picker(&mut buf, "Choose a template", items, 0, true).unwrap();
    let first = String::from_utf8(buf).unwrap();
    assert!(first.starts_with("Choose a template (↑/↓, enter to confirm):\r\n"));
    let mut buf2 = Vec::new();
    draw_picker(&mut buf2, "Choose a template", items, 0, false).unwrap();
    let redraw = String::from_utf8(buf2).unwrap();
    // 5 lines drawn (title + 4 items): the redraw must climb all 5 to
    // re-anchor on the title. A 4-line climb is the off-by-one that
    // garbled the picker on every arrow key.
    assert!(redraw.contains("\u{1b}[5A"), "redraw: {redraw:?}");
    assert!(!redraw.contains("\u{1b}[4A"), "redraw: {redraw:?}");
  }

  #[test]
  fn picker_moves_among_enabled_only() {
    // The real catalog: OpenAI → LangGraph → Pydantic AI, skipping the
    // grayed Claude entry when wrapping.
    let items = Template::picker_items();
    assert_eq!(move_sel(items, 0, 1), 1);
    assert_eq!(move_sel(items, 1, 1), 2);
    assert_eq!(move_sel(items, 2, 1), 0);
    assert_eq!(move_sel(items, 0, -1), 2);
    assert_eq!(move_sel(items, 2, -1), 1);
    // A single enabled item wraps back to itself.
    let items: Vec<PickerItem<Template>> = vec![
      (None, "LangGraph (coming soon)"),
      (Some(Template::OpenAiAgents), "OpenAI Agents SDK"),
      (None, "Claude Agent SDK (coming soon)"),
    ];
    assert_eq!(move_sel(&items, 1, 1), 1);
    assert_eq!(move_sel(&items, 1, -1), 1);
    // Two enabled items: 0 -> 2 skipping 1, wrapping both directions.
    let items: Vec<PickerItem<Template>> = vec![
      (Some(Template::OpenAiAgents), "a"),
      (None, "grayed"),
      (Some(Template::OpenAiAgents), "c"),
    ];
    assert_eq!(move_sel(&items, 0, 1), 2);
    assert_eq!(move_sel(&items, 2, 1), 0);
    assert_eq!(move_sel(&items, 0, -1), 2);
  }

  #[test]
  fn frontend_picker_defaults_to_none() {
    // `pick` starts on the first enabled item.
    let (first, label) = Frontend::picker_items()[0];
    assert_eq!(first, Some(Frontend::None));
    assert_eq!(label, "None (agent only)");
    assert!(
      Frontend::picker_items()
        .iter()
        .all(|(item, _)| item.is_some())
    );
  }

  #[test]
  fn where_it_runs_defaults_to_sectr() {
    let items = RunsOn::picker_items();
    assert_eq!(items.len(), 4);
    assert_eq!(items[0].0, Some(RunsOn::Sectr));
    assert!(items.iter().all(|(item, _)| item.is_some()));
    assert_eq!(
      items[1..].iter().map(|(item, _)| *item).collect::<Vec<_>>(),
      [
        Some(RunsOn::Endpoint(Platform::Lambda)),
        Some(RunsOn::Endpoint(Platform::CloudRun)),
        Some(RunsOn::Endpoint(Platform::Container)),
      ]
    );
  }

  /// A new endpoint project: the scaffold, then the endpoint files in the
  /// agent directory, as `run_init` writes them.
  #[test]
  fn an_endpoint_scaffold_adds_its_files_to_the_agent() {
    let pins = crate::endpoint::Pins {
      base_image: "ghcr.io/sectr-dev/sectr-runner-base:v1.2.3".into(),
      ingest_url: "wss://ingest.sectr.dev/ingest".into(),
    };
    for frontend in [Frontend::None, Frontend::CopilotKit] {
      let dir = tempfile::tempdir().unwrap();
      let target = dir.path().join("my-agent");
      ensure_target_dir(&target, false).unwrap();
      scaffold(&target, Template::OpenAiAgents, frontend, "my-agent").unwrap();
      let agent = agent_dir(&target, frontend);
      assert!(crate::endpoint::write_files(&agent, Platform::CloudRun, &pins, false).unwrap());
      let toml = std::fs::read_to_string(agent.join("sectr.toml")).unwrap();
      assert_eq!(
        crate::deploy::deploy_mode(&toml).unwrap(),
        crate::deploy::Mode::EndpointUnset,
        "{frontend:?}: {toml}"
      );
      assert!(toml.contains("entry = \"app:app\""), "{toml}");
      let dockerfile = std::fs::read_to_string(agent.join("Dockerfile")).unwrap();
      assert!(dockerfile.contains("CMD [\"/usr/local/bin/sectr-worker\", \"--serve\"]"));
      assert!(!dockerfile.contains("aws-lambda-adapter"));
      assert!(agent.join(".dockerignore").is_file());
      // Never at the root of a frontend project.
      if frontend == Frontend::CopilotKit {
        assert!(!target.join("Dockerfile").exists());
      }
    }
  }

  #[test]
  fn a_frontend_projects_root_points_to_its_agent() {
    let dir = tempfile::tempdir().unwrap();
    assert!(!is_agent_dir(dir.path(), Platform::Lambda).unwrap());
    std::fs::create_dir(dir.path().join(AGENT_DIR)).unwrap();
    std::fs::write(dir.path().join(AGENT_DIR).join("sectr.toml"), "").unwrap();
    let err = is_agent_dir(dir.path(), Platform::Lambda)
      .unwrap_err()
      .to_string();
    assert!(
      err.contains("cd agent && sectr init --endpoint lambda"),
      "{err}"
    );
    assert!(is_agent_dir(&dir.path().join(AGENT_DIR), Platform::Lambda).unwrap());
  }

  #[test]
  fn refusing_an_existing_project_points_to_endpoint() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("proj");
    std::fs::create_dir(&target).unwrap();
    std::fs::write(target.join("sectr.toml"), "[app]\nname = \"proj\"\n").unwrap();
    let err = ensure_target_dir(&target, false).unwrap_err().to_string();
    assert!(
      err.contains("sectr init --endpoint lambda|cloud-run|container"),
      "{err}"
    );
    // Not a project: no such line.
    let other = dir.path().join("other");
    std::fs::create_dir(&other).unwrap();
    std::fs::write(other.join("notes.txt"), "x").unwrap();
    let err = ensure_target_dir(&other, false).unwrap_err().to_string();
    assert!(!err.contains("--endpoint"), "{err}");
  }

  #[test]
  fn name_normalization() {
    assert_eq!(normalize_name("My Agent").unwrap(), "my-agent");
    assert_eq!(normalize_name("  my_agent  ").unwrap(), "my-agent");
    assert_eq!(normalize_name("Support//Bot!").unwrap(), "support-bot");
    assert_eq!(normalize_name("--lead--").unwrap(), "lead");
    assert!(normalize_name("").is_err());
    assert!(normalize_name("///").is_err());
    // No path traversal: separators are normalized away.
    assert_eq!(normalize_name("../etc/passwd").unwrap(), "etc-passwd");
  }

  #[test]
  fn args_match_only_known() {
    assert_eq!(
      Template::from_arg("openai-agents").unwrap(),
      Template::OpenAiAgents
    );
    assert_eq!(
      Template::from_arg("langgraph").unwrap(),
      Template::LangGraph
    );
    assert_eq!(
      Template::from_arg("pydantic-ai").unwrap(),
      Template::PydanticAi
    );
    let err = format!("{:#}", Template::from_arg("claude").unwrap_err());
    assert!(
      err.contains("`openai-agents`")
        && err.contains("`langgraph`")
        && err.contains("`pydantic-ai`"),
      "{err}"
    );
    for template in TEMPLATES {
      assert_eq!(Template::from_arg(template.arg()).unwrap(), template);
    }

    assert_eq!(Frontend::from_arg("none").unwrap(), Frontend::None);
    assert_eq!(
      Frontend::from_arg("copilotkit").unwrap(),
      Frontend::CopilotKit
    );
    let err = format!("{:#}", Frontend::from_arg("react").unwrap_err());
    assert!(
      err.contains("`none`") && err.contains("`copilotkit`"),
      "{err}"
    );
  }

  /// The acceptance bar, for both templates: scaffold → `uv sync` →
  /// the inspector imports the template for real and the manifest
  /// extracts. Needs uv AND PyPI (the template depends on `sectr` from the
  /// registry — no editable path dep; before a release, point uv at a
  /// local wheel with UV_FIND_LINKS), so it only runs when SECTR_INIT_IT=1:
  ///   SECTR_INIT_IT=1 cargo test -p sectr-cli -- init::tests::scaffold_then_inspect
  #[tokio::test]
  async fn scaffold_then_inspect() {
    if std::env::var("SECTR_INIT_IT").ok().as_deref() != Some("1") {
      eprintln!("skipped: set SECTR_INIT_IT=1 (needs uv + PyPI network)");
      return;
    }
    for template in TEMPLATES {
      let dir = tempfile::tempdir().unwrap();
      let target = dir.path().join("it-agent");
      ensure_target_dir(&target, false).unwrap();
      scaffold(&target, template, Frontend::None, "it-agent").unwrap();

      let status = tokio::process::Command::new("uv")
        .arg("sync")
        .current_dir(&target)
        .status()
        .await
        .expect("uv must be installed for the integration test");
      assert!(status.success(), "{template:?}: uv sync failed");

      // The smoke check a real `sectr init` runs: real import → manifest.
      let manifest = crate::dev::run_inspect(&target, "app:app")
        .await
        .expect("template must pass sectr inspect");
      assert_eq!(
        manifest["routes"],
        serde_json::json!([{ "path": "/chat", "framework": template.arg() }]),
        "{template:?} manifest: {manifest}"
      );
    }
  }

  /// The frontend builds: scaffold with it → `npm install` → `npm run
  /// lint` → `npm run build`. Needs node + the npm registry, so it only
  /// runs when SECTR_INIT_FRONTEND_IT=1. Before a release (when
  /// `@sectr/copilotkit` at this version isn't on npm yet), set
  /// SECTR_COPILOTKIT_TARBALL to a local `npm pack` of js/copilotkit:
  ///   SECTR_INIT_FRONTEND_IT=1 cargo test -p sectr-cli -- init::tests::scaffold_frontend_then_build
  #[tokio::test]
  async fn scaffold_frontend_then_build() {
    if std::env::var("SECTR_INIT_FRONTEND_IT").ok().as_deref() != Some("1") {
      eprintln!("skipped: set SECTR_INIT_FRONTEND_IT=1 (needs node + npm network)");
      return;
    }
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("it-app");
    ensure_target_dir(&target, false).unwrap();
    scaffold(
      &target,
      Template::OpenAiAgents,
      Frontend::CopilotKit,
      "it-app",
    )
    .unwrap();

    let npm = |args: Vec<String>| {
      let target = target.clone();
      async move {
        let status = tokio::process::Command::new("npm")
          .args(&args)
          .current_dir(&target)
          // `next build` evaluates the Runtime route, which reads it (as in
          // the starter's CI; `.env.development` is dev-only).
          .env("SECTR_AGUI_URL", "https://api.sectr.dev/ag-ui/it-app/chat")
          .status()
          .await
          .expect("npm must be installed for the integration test");
        assert!(status.success(), "npm {args:?} failed");
      }
    };
    let mut install = vec!["install".to_string()];
    if let Ok(tarball) = std::env::var("SECTR_COPILOTKIT_TARBALL") {
      install.push(tarball);
    }
    npm(install).await;
    npm(vec!["run".into(), "lint".into()]).await;
    npm(vec!["run".into(), "build".into()]).await;
  }
}
