use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use sctx_scenario_runner::{
    ReplayClassification, ReplayDisposition, ReplayHarness, ReplayScenario, RunnerConfig,
    ScenarioRunner,
};
use serde::Serialize;
use tempfile::tempdir;

const FIXTURES: [(&str, &[u8]); 6] = [
    (
        "codex_normal_candidate_confirm",
        include_bytes!("fixtures/dynamic/codex-normal.json"),
    ),
    (
        "cursor_normal_candidate_confirm",
        include_bytes!("fixtures/dynamic/cursor-normal.json"),
    ),
    (
        "precompact_resume_new_episode",
        include_bytes!("fixtures/dynamic/precompact-resume.json"),
    ),
    (
        "missing_hook_empty_close_fallback",
        include_bytes!("fixtures/dynamic/missing-hook-fallback.json"),
    ),
    (
        "turnstop_repeat_and_mcp_restart_recovery",
        include_bytes!("fixtures/dynamic/turnstop-recovery.json"),
    ),
    (
        "same_workspace_dual_session_isolation",
        include_bytes!("fixtures/dynamic/dual-session-isolation.json"),
    ),
];
const PHASE_ONE_SEEDS: [u64; 20] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19,
];

fn runner(parent: &std::path::Path) -> ScenarioRunner {
    ScenarioRunner::new(
        RunnerConfig::new(PathBuf::from(env!("CARGO_BIN_EXE_sctx")), "/usr/bin/git")
            .with_sandbox_parent(parent)
            .with_step_timeout(Duration::from_secs(30)),
    )
}

fn inputs(seeds: &[u64]) -> Vec<ReplayScenario<'_>> {
    FIXTURES
        .iter()
        .map(|(_, fixture)| ReplayScenario::new(fixture, seeds))
        .collect()
}

#[derive(Serialize)]
struct SanitizedStageSummary<'a> {
    scenarios: Vec<&'a str>,
    seeds: &'a [u64],
    runs: usize,
    passed: usize,
    classifications: BTreeMap<&'static str, usize>,
    total_duration_ms: u64,
    min_duration_ms: u64,
    max_duration_ms: u64,
    semantic_digest: &'a str,
}

fn summarize(report: &sctx_scenario_runner::ReplayReport) -> SanitizedStageSummary<'_> {
    let mut classifications = BTreeMap::from([
        ("corrupt_data", 0),
        ("expected_fail_open", 0),
        ("infrastructure_flake", 0),
        ("invalid_scenario", 0),
        ("product_invariant_violation", 0),
        ("unsupported_version", 0),
    ]);
    for item in &report.items {
        let key = match item.classification {
            Some(ReplayClassification::InvalidScenario) => "invalid_scenario",
            Some(ReplayClassification::UnsupportedVersion) => "unsupported_version",
            Some(ReplayClassification::CorruptData) => "corrupt_data",
            Some(ReplayClassification::ExpectedFailOpen) => "expected_fail_open",
            Some(ReplayClassification::InfrastructureFlake) => "infrastructure_flake",
            Some(ReplayClassification::ProductInvariantViolation) => "product_invariant_violation",
            None => continue,
        };
        *classifications.get_mut(key).unwrap() += 1;
    }
    let durations = report
        .items
        .iter()
        .map(|item| item.duration_ms)
        .collect::<Vec<_>>();
    SanitizedStageSummary {
        scenarios: FIXTURES.iter().map(|(name, _)| *name).collect(),
        seeds: &PHASE_ONE_SEEDS,
        runs: report.items.len(),
        passed: report
            .items
            .iter()
            .filter(|item| item.disposition == ReplayDisposition::Passed)
            .count(),
        classifications,
        total_duration_ms: durations.iter().sum(),
        min_duration_ms: durations.iter().copied().min().unwrap_or(0),
        max_duration_ms: durations.iter().copied().max().unwrap_or(0),
        semantic_digest: &report.semantic_digest,
    }
}

#[test]
fn real_runner_report_smoke_is_sanitized_and_passes_normally() {
    let parent = tempdir().unwrap();
    let runner = runner(parent.path());
    let report = ReplayHarness::new(&runner).run(&[
        ReplayScenario::new(FIXTURES[0].1, &[176]),
        ReplayScenario::new(FIXTURES[1].1, &[176]),
    ]);
    assert_eq!(report.items.len(), 2);
    assert_eq!(report.items[0].disposition, ReplayDisposition::Classified);
    assert_eq!(
        report.items[0].classification,
        Some(ReplayClassification::ExpectedFailOpen)
    );
    assert_eq!(report.items[0].expected_failure_step_count, 1);
    assert_eq!(report.items[1].disposition, ReplayDisposition::Passed);
    assert_eq!(report.items[1].classification, None);
    let encoded = serde_json::to_string(&report).unwrap();
    for forbidden in [
        "variables",
        "task_id",
        "candidate_id",
        "confirmation_id",
        "prompt",
        "transcript",
        "tool_output",
        parent.path().to_string_lossy().as_ref(),
    ] {
        assert!(
            !encoded.contains(forbidden),
            "unsafe report field: {forbidden}"
        );
    }
}

#[test]
#[ignore = "explicit 6x20 phase-one soak; run with --ignored --nocapture --test-threads=1"]
fn phase_one_replay_runs_all_six_scenarios_twenty_times() {
    let parent = tempdir().unwrap();
    let runner = runner(parent.path());
    let report = ReplayHarness::new(&runner).run(&inputs(&PHASE_ONE_SEEDS));
    let summary = summarize(&report);
    assert_eq!(summary.runs, 120);
    assert_eq!(summary.passed, 100);
    assert_eq!(summary.classifications["expected_fail_open"], 20);
    for classification in [
        "corrupt_data",
        "infrastructure_flake",
        "invalid_scenario",
        "product_invariant_violation",
        "unsupported_version",
    ] {
        assert_eq!(summary.classifications[classification], 0);
    }
    assert!(
        report
            .items
            .iter()
            .all(|item| item.failed_assertion_count == 0)
    );
    println!("{}", serde_json::to_string(&summary).unwrap());
}
