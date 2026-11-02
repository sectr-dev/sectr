//! The scenario catalog. Order is stable: it numbers the generated projects.

use crate::scenario::{Gate, Scenario};

mod abandon;
mod adversarial;
mod chat;
mod crash_in_tool;
mod faults;
mod gated;
mod gated_with_sibling;
mod handoff;
mod lookup;
mod mcp;
mod parallel;
mod policy;
mod prefixes;
mod reasoning;
mod replay;
mod results;
mod structured_output;
mod subagent;
mod tool_raises;
mod tools;
mod two_gated;
mod two_gated_with_sibling;

pub fn all() -> Vec<Scenario> {
  let mut lookup = lookup::scenarios();
  let lookup_gated = lookup.pop().expect("both modes");
  let mut all = vec![
    chat::scenario(),
    lookup.remove(0),
    gated::scenario(),
    parallel::scenario(),
    two_gated::scenario(),
    gated_with_sibling::scenario(),
    policy::scenario(),
  ];
  all.extend(results::scenarios());
  all.extend(tool_raises::scenarios());
  all.extend(reasoning::scenarios());
  // Appended, not next to `lookup`: catalog position numbers the generated
  // projects, so this keeps the others' numbers.
  all.push(lookup_gated);
  all.extend([
    faults::scenario(),
    crash_in_tool::scenario(),
    abandon::scenario(),
    prefixes::scenario(),
    structured_output::scenario(),
    two_gated_with_sibling::scenario(),
    mcp::scenario(),
    mcp::structured_scenario(),
  ]);
  all.extend(subagent::scenarios());
  all.push(adversarial::scenario());
  all.push(adversarial::mcp_scenario());
  all.push(replay::scenario());
  all.extend(langgraph_gates(&all));
  // After the twins, so their projects keep their numbers.
  all.push(handoff::scenario());
  all
}

/// The scenarios that approve or deny tool calls, again with each other way
/// langgraph can gate a tool in code ([`Gate`]), on langgraph only.
const GATE_TWINS: &[&str] = &[
  "gated",
  "two_gated",
  "gated_with_sibling",
  "two_gated_with_sibling",
  "policy",
  "lookup_gated",
  "results_gated",
  "tool_raises_gated",
  "reasoning_gated",
];

fn langgraph_gates(all: &[Scenario]) -> Vec<Scenario> {
  [Gate::Middleware, Gate::Wrapped]
    .into_iter()
    .flat_map(|gate| GATE_TWINS.iter().map(move |id| (gate, *id)))
    .map(|(gate, id)| {
      let scenario = all.iter().find(|s| s.id == id).expect("a twinned scenario");
      scenario.gated_by(gate)
    })
    .collect()
}

#[cfg(test)]
mod tests {
  use std::collections::HashSet;

  use super::*;

  #[test]
  fn catalog_is_well_formed() {
    let all = all();
    let ids: HashSet<_> = all.iter().map(|s| s.id).collect();
    assert_eq!(ids.len(), all.len(), "duplicate scenario id");
    for s in &all {
      assert!(!s.cases.is_empty(), "{} has no cases", s.id);
      let names: HashSet<_> = s.cases.iter().map(|c| c.name).collect();
      assert_eq!(names.len(), s.cases.len(), "{}: duplicate case name", s.id);
      for c in &s.cases {
        let targets: HashSet<_> = c.exceptions.iter().map(|(t, _)| t).collect();
        assert_eq!(
          targets.len(),
          c.exceptions.len(),
          "{}/{}: two exceptions with one target",
          s.id,
          c.name
        );
      }
    }
  }
}
