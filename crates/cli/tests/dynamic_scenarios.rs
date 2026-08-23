use std::{collections::BTreeSet, path::PathBuf, time::Duration};

use sctx_scenario_contract::{parse_scenario, to_canonical_json};
use sctx_scenario_runner::{RunOutcome, RunnerConfig, ScenarioRunner};
use tempfile::tempdir;

const FIXTURES: [(&str, &[u8]); 6] = [
    (
        "codex-normal",
        include_bytes!("fixtures/dynamic/codex-normal.json"),
    ),
    (
        "cursor-normal",
        include_bytes!("fixtures/dynamic/cursor-normal.json"),
    ),
    (
        "precompact-resume",
        include_bytes!("fixtures/dynamic/precompact-resume.json"),
    ),
    (
        "missing-hook-fallback",
        include_bytes!("fixtures/dynamic/missing-hook-fallback.json"),
    ),
    (
        "turnstop-recovery",
        include_bytes!("fixtures/dynamic/turnstop-recovery.json"),
    ),
    (
        "dual-session-isolation",
        include_bytes!("fixtures/dynamic/dual-session-isolation.json"),
    ),
];

fn runner(parent: &std::path::Path) -> ScenarioRunner {
    ScenarioRunner::new(
        RunnerConfig::new(PathBuf::from(env!("CARGO_BIN_EXE_sctx")), "/usr/bin/git")
            .with_sandbox_parent(parent)
            .with_step_timeout(Duration::from_secs(30)),
    )
}

fn safe_shape(outcome: &RunOutcome) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "scenario": outcome.scenario,
        "seed": outcome.seed,
        "steps": outcome.steps,
        "variable_names": outcome.variables.keys().collect::<Vec<_>>(),
        "assertions": outcome.assertions,
    }))
    .unwrap()
}

fn captured_string<'a>(outcome: &'a RunOutcome, name: &str) -> &'a str {
    outcome
        .variables
        .get(name)
        .and_then(|captured| captured.value.as_str())
        .unwrap_or_else(|| panic!("missing typed capture {name}"))
}

fn assert_scenario_facts(name: &str, outcome: &RunOutcome) {
    let same = |left: &str, right: &str| {
        assert!(
            captured_string(outcome, left) == captured_string(outcome, right),
            "{name}: identity relation failed"
        );
    };
    match name {
        "codex-normal" | "missing-hook-fallback" => {
            same("episode-id", "candidate-episode-id");
        }
        "cursor-normal" | "turnstop-recovery" => {
            same("checkpoint-episode-id", "episode-id");
        }
        "precompact-resume" => {
            same("task-id", "task-id-after");
            same("task-session-id", "task-session-after");
            same("checkpoint-one-episode-id", "episode-one-id");
            same("episode-two-id", "candidate-two-episode-id");
            let episodes = [
                captured_string(outcome, "episode-one-id"),
                captured_string(outcome, "episode-two-id"),
                captured_string(outcome, "episode-three-id"),
            ]
            .into_iter()
            .collect::<BTreeSet<_>>();
            assert_eq!(episodes.len(), 3, "{name}: Episode isolation failed");
        }
        "dual-session-isolation" => {
            assert!(
                captured_string(outcome, "task-a-id") != captured_string(outcome, "task-b-id"),
                "{name}: Task isolation failed"
            );
            assert!(
                captured_string(outcome, "task-session-a-id")
                    != captured_string(outcome, "task-session-b-id"),
                "{name}: TaskSession isolation failed"
            );
            assert!(
                captured_string(outcome, "episode-a-id")
                    != captured_string(outcome, "episode-b-id"),
                "{name}: Episode isolation failed"
            );
            same("episode-b-id", "candidate-b-episode-id");
        }
        _ => panic!("unregistered dynamic fixture {name}"),
    }
}

#[test]
fn dynamic_fixtures_are_canonical_synthetic_contracts() {
    let mut scenario_names = BTreeSet::new();
    for (name, fixture) in FIXTURES {
        let scenario = parse_scenario(fixture).unwrap_or_else(|error| panic!("{name}: {error}"));
        assert!(scenario_names.insert(scenario.name.to_string()));
        let canonical = to_canonical_json(&scenario).unwrap();
        assert_eq!(
            canonical,
            to_canonical_json(&parse_scenario(&canonical).unwrap()).unwrap()
        );
        let text = std::str::from_utf8(fixture).unwrap();
        for forbidden in [
            "/Users/",
            "transcript_path",
            "tool_response",
            "last_assistant_message",
            "expected_output",
            "model_text",
        ] {
            assert!(!text.contains(forbidden), "{name}: {forbidden}");
        }
    }
}

#[test]
fn dynamic_fixtures_run_with_all_closed_assertions() {
    for (name, fixture) in FIXTURES {
        let scenario = parse_scenario(fixture).unwrap();
        let parent = tempdir().unwrap();
        let run = runner(parent.path())
            .run(&scenario, 175)
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        assert!(
            run.outcome.assertions.iter().all(|record| record.passed),
            "{name}: {:?}",
            run.outcome.assertions
        );
        assert_scenario_facts(name, &run.outcome);
        let encoded = serde_json::to_string(&run.outcome).unwrap();
        assert!(!encoded.contains("sctx-canary-"));
        assert!(!encoded.contains(parent.path().to_string_lossy().as_ref()));
    }
}

#[test]
#[ignore = "representative multi-seed replay runs through the phase-one report harness"]
fn dynamic_suite_is_reproducible_across_representative_seeds() {
    for (name, fixture) in FIXTURES {
        let scenario = parse_scenario(fixture).unwrap();
        for seed in [7_u64, 29] {
            let first_parent = tempdir().unwrap();
            let second_parent = tempdir().unwrap();
            let first = runner(first_parent.path()).run(&scenario, seed).unwrap();
            let second = runner(second_parent.path()).run(&scenario, seed).unwrap();
            assert_eq!(
                safe_shape(&first.outcome),
                safe_shape(&second.outcome),
                "{name}"
            );
            assert!(first.outcome.assertions.iter().all(|record| record.passed));
            assert_scenario_facts(name, &first.outcome);
            assert_scenario_facts(name, &second.outcome);
        }
    }
}
