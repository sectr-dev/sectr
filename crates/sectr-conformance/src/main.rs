use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};
use sectr_conformance::compile;
use sectr_conformance::harness::Harness;
use sectr_conformance::report::Output;

/// Adapter conformance harness.
#[derive(Parser)]
struct Cli {
  #[command(subcommand)]
  command: Command,
}

#[derive(Subcommand)]
enum Command {
  /// Generate the conformance projects (default: sdk-python/tests/conformance/gen).
  Gen {
    /// Fail if the projects on disk differ from what would be generated.
    #[arg(long)]
    check: bool,
    #[arg(long)]
    out: Option<PathBuf>,
  },
  /// Run the cases against the generated projects.
  Run {
    /// Only cases whose `adapter/endpoint/scenario/case` id contains this.
    filter: Option<String>,
    #[arg(long)]
    gen_dir: Option<PathBuf>,
    /// How many cases run at once (default: the number of CPUs).
    #[arg(long, short)]
    jobs: Option<NonZeroUsize>,
    /// How to show progress: a progress bar on a terminal (`auto`), or a
    /// line per case (`plain`). The summary follows either way.
    #[arg(long, value_enum, default_value_t = Output::Auto)]
    output: Output,
    /// Also write every warning to this file, one JSON object per line
    /// (`run`, `adapter`, `endpoint`, `kind`, `reason`, `detail`). The
    /// summary shows one example per kind.
    #[arg(long)]
    warnings: Option<PathBuf>,
  },
}

#[tokio::main]
async fn main() -> Result<ExitCode> {
  match Cli::parse().command {
    Command::Gen { check, out } => {
      let out = out.unwrap_or_else(compile::default_out);
      let tree = compile::emit_all();
      if check {
        compile::check(&out, &tree)?;
      } else {
        compile::write(&out, &tree)?;
      }
      Ok(ExitCode::SUCCESS)
    }
    Command::Run {
      filter,
      gen_dir,
      jobs,
      output,
      warnings,
    } => {
      let gen_dir = gen_dir.unwrap_or_else(compile::default_out);
      compile::check(&gen_dir, &compile::emit_all())?;
      let jobs = jobs
        .or_else(|| std::thread::available_parallelism().ok())
        .map_or(1, NonZeroUsize::get);
      let harness = Harness::new(&gen_dir).await?;
      let ok = harness
        .run_all(filter.as_deref(), jobs, output, warnings.as_deref())
        .await?;
      Ok(if ok {
        ExitCode::SUCCESS
      } else {
        ExitCode::FAILURE
      })
    }
  }
}
