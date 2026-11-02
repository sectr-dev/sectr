//! What a run prints: a progress bar (or, with `--output plain` or off a
//! terminal, a line per case) while the cases run, then the failures'
//! details, the totals, a scenario × adapter/endpoint matrix of known
//! failures, and the known failures grouped by reason.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::time::Duration;

use clap::ValueEnum;
use console::{Term, style};
use indicatif::{ProgressBar, ProgressStyle};

use crate::adapter::Adapter;
use crate::endpoint::Endpoint;
use crate::warning::{self, Warning};

/// How a run shows progress (`--output`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Output {
  /// A progress bar on a terminal, otherwise `plain`.
  Auto,
  /// A progress bar. What fails the run (an unexpected failure, or a
  /// known failure that passed) prints above it as it happens; known
  /// failures only count.
  Progress,
  /// A line per case as it finishes.
  Plain,
}

/// How one case run ended.
pub enum Outcome {
  Passed,
  Failed(String),
  /// The known reason, and the failure's first line: a known failure should
  /// keep failing for the reason it's tagged with.
  KnownFailure(&'static str, String),
  /// A known failure that passed: the tag must go.
  Fixed(&'static str),
  Skipped(String),
}

/// One finished case run.
pub struct Finished {
  pub adapter: Adapter,
  pub endpoint: Endpoint,
  /// The scenario's catalog position, which orders the matrix rows.
  pub scenario_index: usize,
  pub scenario: &'static str,
  /// The case's position in its scenario, which orders case lists.
  pub case_index: usize,
  pub case: &'static str,
  pub outcome: Outcome,
  /// What the case noticed but didn't stop for.
  pub warnings: Vec<Warning>,
  pub secs: f32,
}

impl Finished {
  pub fn id(&self) -> String {
    format!(
      "{}/{}/{}/{}",
      self.adapter.dir(),
      self.endpoint.id(),
      self.scenario,
      self.case
    )
  }
}

/// Collects the runs as they finish, and prints the summary at the end.
pub struct Report {
  bar: Option<ProgressBar>,
  runs: Vec<Finished>,
  passed: usize,
  known: usize,
  failed: usize,
  skipped: usize,
  warnings: usize,
}

impl Report {
  pub fn new(output: Output, total: usize) -> Self {
    let progress = match output {
      Output::Progress => true,
      Output::Plain => false,
      Output::Auto => Term::stdout().is_term() && Term::stderr().is_term(),
    };
    let bar = progress.then(|| {
      let bar = ProgressBar::new(total as u64);
      bar.set_style(
        ProgressStyle::with_template(
          "[{elapsed_precise}] {wide_bar:.cyan/blue} {pos}/{len}  {msg} · eta {eta}",
        )
        .expect("valid template")
        .progress_chars("██░"),
      );
      bar.enable_steady_tick(Duration::from_millis(200));
      bar
    });
    let report = Report {
      bar,
      runs: Vec::new(),
      passed: 0,
      known: 0,
      failed: 0,
      skipped: 0,
      warnings: 0,
    };
    report.update_message();
    report
  }

  pub fn add(&mut self, run: Finished) {
    let (id, secs) = (run.id(), run.secs);
    // The plain line, and whether it also shows above the bar.
    let (line, notable) = match &run.outcome {
      Outcome::Passed => {
        self.passed += 1;
        (format!("  ok       {id} ({secs:.1}s)"), false)
      }
      Outcome::KnownFailure(reason, why) => {
        self.known += 1;
        (
          format!("  known    {id} ({reason}, {secs:.1}s): {why}"),
          false,
        )
      }
      Outcome::Fixed(_) => {
        self.failed += 1;
        (format!("  FIXED?   {id} ({secs:.1}s)"), true)
      }
      Outcome::Failed(_) => {
        self.failed += 1;
        (format!("  FAILED   {id} ({secs:.1}s)"), true)
      }
      Outcome::Skipped(reason) => {
        self.skipped += 1;
        (format!("  skipped  {id}: {reason}"), false)
      }
    };
    self.warnings += run.warnings.len();
    let line = match run.warnings.len() {
      0 => line,
      1 => format!("{line} (1 warning)"),
      n => format!("{line} ({n} warnings)"),
    };
    match &self.bar {
      None => println!("{line}"),
      Some(bar) => {
        if notable {
          bar.println(style(line.trim_start()).red().to_string());
        }
        bar.inc(1);
      }
    }
    self.runs.push(run);
    self.update_message();
  }

  fn update_message(&self) {
    if let Some(bar) = &self.bar {
      let failed = format!("failed {}", self.failed);
      let failed = if self.failed > 0 {
        style(failed).red().to_string()
      } else {
        failed
      };
      let warnings = if self.warnings > 0 {
        style(format!(" · warnings {}", self.warnings))
          .yellow()
          .to_string()
      } else {
        String::new()
      };
      bar.set_message(format!(
        "ok {} · known {} · {failed}{warnings}",
        self.passed, self.known
      ));
    }
  }

  /// Print the failures' details and the summary. Returns whether
  /// everything passed (known failures count as passing).
  pub fn finish(mut self, elapsed: Duration) -> bool {
    if let Some(bar) = self.bar.take() {
      bar.finish_and_clear();
    }
    let mut failures: Vec<(String, String)> = self
      .runs
      .iter()
      .filter_map(|run| match &run.outcome {
        Outcome::Failed(err) => Some((run.id(), err.clone())),
        Outcome::Fixed(reason) => Some((
          run.id(),
          format!("passed, but is tagged as a known failure ({reason}); remove the tag"),
        )),
        _ => None,
      })
      .collect();
    failures.sort();
    for (id, err) in &failures {
      println!("\n{} {id}\n{}", style("FAILED").red().bold(), indent(err));
    }
    let warned = self.runs.iter().filter(|r| !r.warnings.is_empty()).count();
    let warned = if warned > 0 {
      format!(", {warned} with warnings")
    } else {
      String::new()
    };
    println!(
      "\nrun: {} passed, {} failed, {} known failures, {} skipped{warned} ({:.1}s)",
      self.passed,
      self.failed,
      self.known,
      self.skipped,
      elapsed.as_secs_f32()
    );
    if !self.runs.is_empty() {
      println!("\n{}", matrix(&self.runs));
      let known = known_failures(&self.runs);
      if !known.is_empty() {
        println!("\n{known}");
      }
      let warnings = warnings(&self.runs);
      if !warnings.is_empty() {
        println!("\n{warnings}");
      }
    }
    // A blank last line, so a shell prompt that redraws the line above
    // it can't take the summary's last line.
    println!();
    self.failed == 0
  }
}

fn label(adapter: Adapter) -> &'static str {
  match adapter {
    Adapter::OpenAiAgents => "openai-agents",
    Adapter::LangGraph => "langgraph",
    Adapter::PydanticAi => "pydantic-ai",
  }
}

/// Two letters, for the matrix's column headers.
fn short(endpoint: Endpoint) -> &'static str {
  match endpoint {
    Endpoint::OpenAiChat => "ch",
    Endpoint::OpenRouter => "or",
    Endpoint::OpenAiResponses => "re",
    Endpoint::Anthropic => "an",
  }
}

/// One cell's display width.
const CELL: usize = 3;

/// Rows: scenarios in catalog order. Columns: each adapter's endpoints.
/// A cell aggregates the scenario's cases there: `✓` all passed, a number
/// that many known failures, `✗` any unexpected failure (or a known
/// failure that passed), `·` none ran. The last row sums the known
/// failures per column.
fn matrix(runs: &[Finished]) -> String {
  let adapters: Vec<Adapter> = Adapter::ALL
    .into_iter()
    .filter(|a| runs.iter().any(|r| r.adapter == *a))
    .collect();
  let endpoints: Vec<Endpoint> = Endpoint::ALL
    .into_iter()
    .filter(|e| runs.iter().any(|r| r.endpoint == *e))
    .collect();
  let scenarios: BTreeMap<usize, &str> = runs
    .iter()
    .map(|r| (r.scenario_index, r.scenario))
    .collect();
  let label_width = scenarios
    .values()
    .map(|s| s.len())
    .max()
    .unwrap_or(0)
    .max(5);
  let group_width = |a: Adapter| label(a).len().max(CELL * endpoints.len());

  let mut out = String::new();
  out.push_str(&" ".repeat(label_width));
  for a in &adapters {
    out.push_str(&format!("  {:<w$}", label(*a), w = group_width(*a)));
  }
  out.push('\n');
  out.push_str(&" ".repeat(label_width));
  for a in &adapters {
    let heads: String = endpoints
      .iter()
      .map(|e| format!("{:>CELL$}", short(*e)))
      .collect();
    out.push_str(&format!("  {heads:<w$}", w = group_width(*a)));
  }
  out.push('\n');

  for (index, scenario) in &scenarios {
    out.push_str(&format!("{scenario:<label_width$}"));
    for a in &adapters {
      let mut cells = String::new();
      for e in &endpoints {
        let here: Vec<&Finished> = runs
          .iter()
          .filter(|r| r.scenario_index == *index && r.adapter == *a && r.endpoint == *e)
          .collect();
        cells.push_str(&cell(&here));
      }
      let pad = group_width(*a) - CELL * endpoints.len();
      out.push_str(&format!("  {cells}{}", " ".repeat(pad)));
    }
    out.push('\n');
  }

  let mut sums_row = format!("{:<label_width$}", "known");
  for a in &adapters {
    let sums: String = endpoints
      .iter()
      .map(|e| {
        let n = runs
          .iter()
          .filter(|r| {
            r.adapter == *a && r.endpoint == *e && matches!(r.outcome, Outcome::KnownFailure(..))
          })
          .count();
        format!("{n:>CELL$}")
      })
      .collect();
    sums_row.push_str(&format!("  {sums:<w$}", w = group_width(*a)));
  }
  out.push_str(&style(sums_row.trim_end()).bold().to_string());
  out.push('\n');

  let cells = [
    "✓ = all passed",
    "n = known failures",
    "✗ = failed",
    "· = not run",
  ]
  .map(String::from);
  let names: Vec<String> = endpoints
    .iter()
    .map(|e| format!("{} = {}", short(*e), e.id()))
    .collect();
  out.push_str(&style(legend(&[&cells, &names])).dim().to_string());
  out
}

/// Rows of legend entries, in columns aligned across the rows.
fn legend(rows: &[&[String]]) -> String {
  let columns = rows.iter().map(|r| r.len()).max().unwrap_or(0);
  let widths: Vec<usize> = (0..columns)
    .map(|i| {
      rows
        .iter()
        .filter_map(|r| r.get(i))
        .map(|e| e.chars().count())
        .max()
        .unwrap_or(0)
    })
    .collect();
  rows
    .iter()
    .map(|row| {
      let line: Vec<String> = row
        .iter()
        .zip(&widths)
        .map(|(entry, w)| format!("{entry:<w$}"))
        .collect();
      line.join("   ").trim_end().to_string()
    })
    .collect::<Vec<_>>()
    .join("\n")
}

/// One matrix cell, padded to [`CELL`] before styling.
fn cell(runs: &[&Finished]) -> String {
  let failed = runs
    .iter()
    .any(|r| matches!(r.outcome, Outcome::Failed(_) | Outcome::Fixed(_)));
  let known = runs
    .iter()
    .filter(|r| matches!(r.outcome, Outcome::KnownFailure(..)))
    .count();
  let ran = runs
    .iter()
    .any(|r| !matches!(r.outcome, Outcome::Skipped(_)));
  if failed {
    style(format!("{:>CELL$}", "✗")).red().bold().to_string()
  } else if !ran {
    style(format!("{:>CELL$}", "·")).dim().to_string()
  } else if known > 0 {
    style(format!("{known:>CELL$}")).yellow().to_string()
  } else {
    style(format!("{:>CELL$}", "✓")).green().to_string()
  }
}

/// The known failures (and known failures that passed) grouped by reason,
/// most runs first.
fn known_failures(runs: &[Finished]) -> String {
  let mut by_reason: BTreeMap<&str, Vec<&Finished>> = BTreeMap::new();
  for run in runs {
    if let Outcome::KnownFailure(reason, _) | Outcome::Fixed(reason) = run.outcome {
      by_reason.entry(reason).or_default().push(run);
    }
  }
  if by_reason.is_empty() {
    return String::new();
  }
  let mut reasons: Vec<(&str, Vec<&Finished>)> = by_reason.into_iter().collect();
  reasons.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(b.0)));

  let mut out = String::from("Known failures by reason\n");
  for (reason, tagged) in reasons {
    let runs_label = if tagged.len() == 1 { "run " } else { "runs" };
    let mut head = format!(
      "{}  {:>3} {runs_label}  {}",
      style(reason).yellow().bold(),
      tagged.len(),
      reach(runs, &tagged)
    );
    let fixed = tagged
      .iter()
      .filter(|r| matches!(r.outcome, Outcome::Fixed(_)))
      .count();
    if fixed > 0 {
      head.push_str(
        &style(format!(" · FIXED? {fixed} passed: remove the tag"))
          .red()
          .bold()
          .to_string(),
      );
    }
    out.push_str(&head);
    out.push('\n');
    for line in wrap(&case_list(runs, &tagged), 96) {
      out.push_str(&format!("          {line}\n"));
    }
    out.push('\n');
  }
  out.truncate(out.trim_end().len());
  out
}

/// The adapters `members` ran on, and their endpoints when those aren't
/// every endpoint the same cases ran on.
fn reach(runs: &[Finished], members: &[&Finished]) -> String {
  let adapters: Vec<&str> = Adapter::ALL
    .into_iter()
    .filter(|a| members.iter().any(|r| r.adapter == *a))
    .map(label)
    .collect();
  let cases: HashSet<(Adapter, usize, usize)> = members
    .iter()
    .map(|r| (r.adapter, r.scenario_index, r.case_index))
    .collect();
  let ran_on: BTreeSet<&str> = runs
    .iter()
    .filter(|r| {
      cases.contains(&(r.adapter, r.scenario_index, r.case_index))
        && !matches!(r.outcome, Outcome::Skipped(_))
    })
    .map(|r| r.endpoint.id())
    .collect();
  let members_on: BTreeSet<&str> = members.iter().map(|r| r.endpoint.id()).collect();
  let mut out = adapters.join(", ");
  if members_on != ran_on {
    let named: Vec<&str> = Endpoint::ALL
      .into_iter()
      .map(|e| e.id())
      .filter(|e| members_on.contains(e))
      .collect();
    out.push_str(&format!(" · {}", named.join(", ")));
  }
  out
}

/// The warnings grouped by kind and known reason ([`warning::known`]);
/// new ones first, as untracked.
fn warnings(runs: &[Finished]) -> String {
  // (known last, kind, reason) -> (runs, warning count)
  let mut groups: BTreeMap<(bool, &str, &str), (Vec<&Finished>, usize)> = BTreeMap::new();
  for run in runs {
    let mut here: BTreeMap<(bool, &str, &str), usize> = BTreeMap::new();
    for w in &run.warnings {
      let reason = warning::known(w.kind, run.adapter, run.endpoint);
      *here
        .entry((reason.is_some(), w.kind, reason.unwrap_or("")))
        .or_default() += 1;
    }
    for (key, n) in here {
      let group = groups.entry(key).or_default();
      group.0.push(run);
      group.1 += n;
    }
  }
  if groups.is_empty() {
    return String::new();
  }
  let mut groups: Vec<_> = groups.into_iter().collect();
  groups.sort_by(|a, b| {
    (a.0.0, std::cmp::Reverse(a.1.0.len()), a.0.1).cmp(&(
      b.0.0,
      std::cmp::Reverse(b.1.0.len()),
      b.0.1,
    ))
  });

  let mut out = String::from("Warnings\n");
  for ((tracked, kind, reason), (members, count)) in groups {
    let filed = if tracked {
      style(reason).yellow().bold().to_string()
    } else {
      style(format!("{:<9}", "untracked"))
        .red()
        .bold()
        .to_string()
    };
    let runs_label = if members.len() == 1 { "run" } else { "runs" };
    let warnings_label = if count == 1 { "warning" } else { "warnings" };
    out.push_str(&format!(
      "{kind}  {filed}  {} {runs_label}, {count} {warnings_label}  {}\n",
      members.len(),
      reach(runs, &members)
    ));
    for line in wrap(&case_list(runs, &members), 96) {
      out.push_str(&format!("          {line}\n"));
    }
    let example = members
      .iter()
      .flat_map(|r| &r.warnings)
      .find(|w| w.kind == kind)
      .map(|w| w.detail.as_str())
      .unwrap_or_default();
    out.push_str(&format!(
      "          {}\n",
      style(format!("e.g. {example}")).dim()
    ));
    out.push('\n');
  }
  out.truncate(out.trim_end().len());
  out
}

/// `scenario/*` when every case of the scenario that ran is tagged,
/// otherwise `scenario/case` or `scenario/{a,b}`, in catalog order.
fn case_list(runs: &[Finished], tagged: &[&Finished]) -> Vec<String> {
  let mut by_scenario: BTreeMap<(usize, &str), BTreeMap<usize, &str>> = BTreeMap::new();
  for r in tagged {
    by_scenario
      .entry((r.scenario_index, r.scenario))
      .or_default()
      .insert(r.case_index, r.case);
  }
  by_scenario
    .into_iter()
    .map(|((index, scenario), cases)| {
      let all = runs
        .iter()
        .filter(|r| r.scenario_index == index)
        .map(|r| r.case_index)
        .collect::<BTreeSet<_>>()
        .len();
      let names: Vec<&str> = cases.into_values().collect();
      if names.len() == all && all > 1 {
        format!("{scenario}/*")
      } else if names.len() == 1 {
        format!("{scenario}/{}", names[0])
      } else {
        format!("{scenario}/{{{}}}", names.join(","))
      }
    })
    .collect()
}

/// Join `items` with ", ", breaking lines before `width`.
fn wrap(items: &[String], width: usize) -> Vec<String> {
  let mut lines = vec![String::new()];
  for (i, item) in items.iter().enumerate() {
    let last = lines.last_mut().expect("one line");
    let piece = if i + 1 < items.len() {
      format!("{item},")
    } else {
      item.clone()
    };
    if !last.is_empty() && last.len() + 1 + piece.len() > width {
      lines.push(piece);
    } else {
      if !last.is_empty() {
        last.push(' ');
      }
      last.push_str(&piece);
    }
  }
  lines
}

fn indent(s: &str) -> String {
  s.lines()
    .map(|l| format!("      {l}"))
    .collect::<Vec<_>>()
    .join("\n")
}
