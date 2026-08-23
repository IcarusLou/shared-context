use std::collections::{BTreeMap, HashMap, HashSet};

use sctx_scenario_contract::{ActionKind, CrashTiming, FaultKind, ScenarioDefinition};

use crate::{FailureClassification, RunnerFailure};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PlannedDisposition {
    Execute,
    Drop,
    Barrier,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PlannedStep {
    pub index: usize,
    pub occurrence: u8,
    pub disposition: PlannedDisposition,
    pub crash: Option<CrashTiming>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ScheduledBatch {
    pub parallel: bool,
    pub steps: Vec<PlannedStep>,
}

/// Compile every fault combination and causal decision before a sandbox or child process exists.
#[allow(clippy::too_many_lines)]
pub(crate) fn compile_schedule(
    scenario: &ScenarioDefinition,
    seed: u64,
) -> Result<Vec<ScheduledBatch>, RunnerFailure> {
    let scenario_name = scenario.name.as_str();
    let indices = scenario
        .actions
        .iter()
        .enumerate()
        .map(|(index, step)| (step.id.as_str(), index))
        .collect::<HashMap<_, _>>();
    let mut dependencies = scenario
        .actions
        .iter()
        .map(|step| {
            step.after
                .iter()
                .map(|dependency| indices[dependency.as_str()])
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let mut occurrences = vec![1_u8; scenario.actions.len()];
    let mut dropped = vec![false; scenario.actions.len()];
    let mut crash = vec![None; scenario.actions.len()];
    let mut grouped = vec![None; scenario.actions.len()];
    let mut groups = Vec::<Vec<usize>>::new();

    for fault in &scenario.faults {
        match &fault.fault {
            FaultKind::Drop { target } => {
                let target = indices[target.as_str()];
                if dropped[target] || occurrences[target] != 1 || crash[target].is_some() {
                    return Err(invalid_fault(scenario_name, seed));
                }
                dropped[target] = true;
            }
            FaultKind::Repeat { target, times } => {
                let target = indices[target.as_str()];
                if dropped[target] || occurrences[target] != 1 {
                    return Err(invalid_fault(scenario_name, seed));
                }
                occurrences[target] = *times;
            }
            FaultKind::Reorder { first, second } => {
                let first = indices[first.as_str()];
                let second = indices[second.as_str()];
                if dependencies[first].contains(&second) {
                    return Err(invalid_fault(scenario_name, seed));
                }
                dependencies[first].push(second);
            }
            FaultKind::Crash { target, timing, .. } => {
                let target = indices[target.as_str()];
                if dropped[target] || crash[target].replace(*timing).is_some() {
                    return Err(invalid_fault(scenario_name, seed));
                }
            }
            FaultKind::Concurrent { targets } => {
                let members = targets
                    .iter()
                    .map(|target| indices[target.as_str()])
                    .collect::<Vec<_>>();
                register_group(&members, &mut grouped, &mut groups, scenario_name, seed)?;
            }
        }
    }

    let mut barriers = BTreeMap::<&str, Vec<usize>>::new();
    for (index, step) in scenario.actions.iter().enumerate() {
        if let ActionKind::Barrier { barrier } = &step.action {
            barriers.entry(barrier.as_str()).or_default().push(index);
        }
    }
    for members in barriers.into_values().filter(|members| members.len() > 1) {
        register_group(&members, &mut grouped, &mut groups, scenario_name, seed)?;
    }
    for members in &groups {
        for (offset, first) in members.iter().enumerate() {
            for second in &members[offset + 1..] {
                if dependency_reaches(&dependencies, *first, *second)
                    || dependency_reaches(&dependencies, *second, *first)
                {
                    return Err(invalid_fault(scenario_name, seed));
                }
            }
        }
    }

    for variable in &scenario.variables {
        let source = indices[variable.capture.step.as_str()];
        if dropped[source] || crash[source] == Some(CrashTiming::Before) {
            return Err(RunnerFailure::new(
                scenario_name,
                seed,
                Some(variable.capture.step.as_str()),
                FailureClassification::InvalidScenario,
                "a capture source is removed by its fault plan",
            ));
        }
    }

    let mut completed = vec![false; scenario.actions.len()];
    let mut schedule = Vec::new();
    let mut round = 0_u64;
    while completed.iter().any(|complete| !complete) {
        let mut units = Vec::<Vec<usize>>::new();
        let mut seen_groups = HashSet::new();
        for index in 0..scenario.actions.len() {
            if completed[index] {
                continue;
            }
            if let Some(group_id) = grouped[index] {
                if !seen_groups.insert(group_id) {
                    continue;
                }
                let members = &groups[group_id];
                if members.iter().all(|member| {
                    !completed[*member]
                        && dependencies[*member].iter().all(|dependency| {
                            completed[*dependency] || members.contains(dependency)
                        })
                }) {
                    units.push(members.clone());
                }
            } else if dependencies[index]
                .iter()
                .all(|dependency| completed[*dependency])
            {
                units.push(vec![index]);
            }
        }
        let Some(selected) = units.into_iter().min_by_key(|unit| {
            let representative = unit.iter().copied().min().unwrap_or(0) as u64;
            seeded_key(seed, round, representative)
        }) else {
            return Err(invalid_fault(scenario_name, seed));
        };

        let parallel = selected.len() > 1;
        let mut planned = Vec::new();
        for index in &selected {
            let disposition = if dropped[*index] {
                PlannedDisposition::Drop
            } else if matches!(scenario.actions[*index].action, ActionKind::Barrier { .. }) {
                PlannedDisposition::Barrier
            } else {
                PlannedDisposition::Execute
            };
            let count = if disposition == PlannedDisposition::Execute {
                occurrences[*index]
            } else {
                1
            };
            for occurrence in 0..count {
                planned.push(PlannedStep {
                    index: *index,
                    occurrence,
                    disposition,
                    crash: crash[*index],
                });
            }
            completed[*index] = true;
        }
        planned.sort_by_key(|step| (step.index, step.occurrence));
        schedule.push(ScheduledBatch {
            parallel,
            steps: planned,
        });
        round = round.saturating_add(1);
    }
    Ok(schedule)
}

fn register_group(
    members: &[usize],
    grouped: &mut [Option<usize>],
    groups: &mut Vec<Vec<usize>>,
    scenario: &str,
    seed: u64,
) -> Result<(), RunnerFailure> {
    if members.iter().any(|member| grouped[*member].is_some()) {
        return Err(invalid_fault(scenario, seed));
    }
    let group_id = groups.len();
    let mut members = members.to_vec();
    members.sort_unstable();
    for member in &members {
        grouped[*member] = Some(group_id);
    }
    groups.push(members);
    Ok(())
}

fn dependency_reaches(dependencies: &[Vec<usize>], step: usize, target: usize) -> bool {
    let mut pending = dependencies[step].clone();
    let mut visited = HashSet::new();
    while let Some(current) = pending.pop() {
        if current == target {
            return true;
        }
        if visited.insert(current) {
            pending.extend(dependencies[current].iter().copied());
        }
    }
    false
}

fn invalid_fault(scenario: &str, seed: u64) -> RunnerFailure {
    RunnerFailure::new(
        scenario,
        seed,
        None,
        FailureClassification::InvalidScenario,
        "fault plans cannot form a deterministic legal schedule",
    )
}

fn seeded_key(seed: u64, round: u64, index: u64) -> u64 {
    let mut value = seed
        ^ round.wrapping_mul(0x9e37_79b9_7f4a_7c15)
        ^ index.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

#[cfg(test)]
mod tests {
    use sctx_scenario_contract::parse_scenario;

    use super::compile_schedule;

    const CODEX: &[u8] =
        include_bytes!("../../scenario-contract/tests/fixtures/valid/codex-v1.json");

    #[test]
    fn valid_contract_has_a_deterministic_schedule() {
        let scenario = parse_scenario(CODEX).unwrap();
        assert_eq!(
            compile_schedule(&scenario, 7).unwrap(),
            compile_schedule(&scenario, 7).unwrap()
        );
    }
}
