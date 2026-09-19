//! Sanitized, non-blocking phase-one replay reporting.

use std::{collections::BTreeMap, time::Instant};

use sctx_scenario_contract::{
    ActionExpectation, ActionKind, ActorKind, ContractErrorKind, CrashTiming, FaultKind,
    InvariantKind, ScenarioDefinition, parse_scenario,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::{
    FailureClassification, RunOutcome, RunnerFailure, ScenarioRunner, StepRecord, StepStatus,
    expected_failure_kind,
    schedule::{PlannedDisposition, PlannedStep, compile_schedule},
};

const REPORT_SCHEMA: &str = "shared-context.replay-report";
const REPORT_VERSION: u8 = 1;
const INVALID_SCENARIO_NAME: &str = "invalid-scenario";
const REDACTED_SCENARIO_NAME: &str = "redacted-scenario";
const PHASE_ONE_SCENARIO_NAMES: [&str; 6] = [
    "codex_normal_candidate_confirm",
    "cursor_normal_candidate_confirm",
    "precompact_resume_new_episode",
    "missing_hook_empty_close_fallback",
    "turnstop_repeat_and_mcp_restart_recovery",
    "same_workspace_dual_session_isolation",
];

/// Closed phase-one classifications. A report is evidence only and never creates an Issue or
/// changes product state.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayClassification {
    InvalidScenario,
    UnsupportedVersion,
    CorruptData,
    ExpectedFailOpen,
    InfrastructureFlake,
    ProductInvariantViolation,
}

/// Whether one replay passed normally or produced a non-blocking classified observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayDisposition {
    Passed,
    Classified,
}

/// Closed, payload-free diagnostics retained by phase-one reports.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayDiagnosticCode {
    AllAssertionsPassed,
    ScenarioContractInvalid,
    ScenarioVersionUnsupported,
    ScenarioJsonCorrupt,
    ExpectedFailureObserved,
    RunnerInfrastructureFailure,
    RunnerOutcomeIncomplete,
    ClosedInvariantFailed,
}

/// One sanitized replay result. It contains no captured values, dynamic identities, paths,
/// assertion names, step names, runner messages, stdout, or stderr.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayReportItem {
    pub scenario: String,
    pub seed: u64,
    pub duration_ms: u64,
    pub step_count: usize,
    pub assertion_count: usize,
    pub expected_failure_step_count: usize,
    pub failed_assertion_count: usize,
    pub disposition: ReplayDisposition,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub classification: Option<ReplayClassification>,
    pub diagnostic_code: ReplayDiagnosticCode,
    pub semantic_digest: String,
}

/// A deterministic semantic summary plus per-run timing observations.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayReport {
    pub schema: String,
    pub version: u8,
    pub items: Vec<ReplayReportItem>,
    pub semantic_digest: String,
}

/// One serialized scenario and its explicit replay seeds.
#[derive(Clone, Copy, Debug)]
pub struct ReplayScenario<'a> {
    pub bytes: &'a [u8],
    pub seeds: &'a [u64],
}

impl<'a> ReplayScenario<'a> {
    #[must_use]
    pub const fn new(bytes: &'a [u8], seeds: &'a [u64]) -> Self {
        Self { bytes, seeds }
    }
}

/// Minimal execution seam used by the real runner and deterministic reporter tests.
pub trait ReplayExecutor {
    /// Execute one already validated scenario and return only the raw-free outcome boundary.
    ///
    /// # Errors
    ///
    /// Returns the runner's sanitized failure. The reporter deliberately discards its message and
    /// step fields.
    fn execute_replay(
        &self,
        scenario: &ScenarioDefinition,
        seed: u64,
    ) -> Result<RunOutcome, RunnerFailure>;
}

impl ReplayExecutor for ScenarioRunner {
    fn execute_replay(
        &self,
        scenario: &ScenarioDefinition,
        seed: u64,
    ) -> Result<RunOutcome, RunnerFailure> {
        self.run(scenario, seed).map(|run| run.outcome)
    }
}

/// Non-blocking reporter over a configured runner or a deterministic test executor.
pub struct ReplayHarness<'a, E> {
    executor: &'a E,
}

impl<'a, E: ReplayExecutor> ReplayHarness<'a, E> {
    #[must_use]
    pub const fn new(executor: &'a E) -> Self {
        Self { executor }
    }

    /// Execute a scenario matrix and classify every input/seed independently. Parse errors,
    /// runner failures, and invariant failures become report items and do not stop later runs.
    #[must_use]
    pub fn run(&self, scenarios: &[ReplayScenario<'_>]) -> ReplayReport {
        let mut items = Vec::new();
        for input in scenarios {
            for &seed in input.seeds {
                items.push(self.run_one(input.bytes, seed));
            }
        }
        let semantic_digest = report_digest(&items);
        ReplayReport {
            schema: REPORT_SCHEMA.to_owned(),
            version: REPORT_VERSION,
            items,
            semantic_digest,
        }
    }

    fn run_one(&self, bytes: &[u8], seed: u64) -> ReplayReportItem {
        let started = Instant::now();
        let result = match parse_scenario(bytes) {
            Ok(scenario) => {
                let scenario_name = safe_scenario_name(scenario.name.as_str());
                let declared_steps = scenario.actions.len();
                let declared_assertions = scenario.assertions.len();
                let contract_digest = scenario_semantic_digest(&scenario);
                match self.executor.execute_replay(&scenario, seed) {
                    Ok(outcome) => item_from_outcome(
                        &scenario,
                        scenario_name,
                        seed,
                        &outcome,
                        &contract_digest,
                    ),
                    Err(failure) => item_from_runner_failure(
                        scenario_name,
                        seed,
                        declared_steps,
                        declared_assertions,
                        failure.classification,
                        &contract_digest,
                    ),
                }
            }
            Err(error) => item_from_contract_error(seed, error.kind()),
        };
        with_duration(result, started.elapsed().as_millis())
    }
}

fn safe_scenario_name(name: &str) -> String {
    if PHASE_ONE_SCENARIO_NAMES.contains(&name) {
        name.to_owned()
    } else {
        REDACTED_SCENARIO_NAME.to_owned()
    }
}

fn scenario_semantic_digest(scenario: &ScenarioDefinition) -> String {
    let actor_indices = scenario
        .actors
        .iter()
        .enumerate()
        .map(|(index, actor)| (actor.id.as_str(), index))
        .collect::<BTreeMap<_, _>>();
    let step_indices = scenario
        .actions
        .iter()
        .enumerate()
        .map(|(index, action)| (action.id.as_str(), index))
        .collect::<BTreeMap<_, _>>();
    let actors = scenario_actor_shapes(scenario, &actor_indices);
    let actions = scenario_action_shapes(scenario, &actor_indices, &step_indices);
    let variables = scenario
        .variables
        .iter()
        .map(|variable| {
            json!({
                "kind": variable.value_type,
                "source": step_indices[variable.capture.step.as_str()],
            })
        })
        .collect::<Vec<_>>();
    let faults = scenario_fault_shapes(scenario, &actor_indices, &step_indices);
    let assertions = scenario
        .assertions
        .iter()
        .map(|assertion| invariant_tag(&assertion.invariant))
        .collect::<Vec<_>>();
    let resources = scenario
        .resources
        .iter()
        .map(|resource| {
            json!({
                "synthetic_git": resource.synthetic_git,
                "file_count": resource.files.len(),
            })
        })
        .collect::<Vec<_>>();
    let events = scenario
        .events
        .iter()
        .map(|event| {
            json!({
                "classification": event.classification,
                "product_action": event.product_action,
            })
        })
        .collect::<Vec<_>>();
    hash_serialized(&json!({
        "schema": scenario.schema,
        "version": scenario.version,
        "agent": {
            "vendor": scenario.agent.vendor,
            "version": scenario.agent.version.to_string(),
            "framing": scenario.agent.framing,
        },
        "actors": actors,
        "actions": actions,
        "variables": variables,
        "faults": faults,
        "assertions": assertions,
        "resources": resources,
        "events": events,
    }))
}

fn scenario_actor_shapes(
    scenario: &ScenarioDefinition,
    actor_indices: &BTreeMap<&str, usize>,
) -> Vec<Value> {
    scenario
        .actors
        .iter()
        .map(|actor| match &actor.kind {
            ActorKind::Session => json!({"kind": "session"}),
            ActorKind::Task { session } => json!({
                "kind": "task",
                "session": actor_indices[session.as_str()],
            }),
        })
        .collect()
}

fn scenario_action_shapes(
    scenario: &ScenarioDefinition,
    actor_indices: &BTreeMap<&str, usize>,
    step_indices: &BTreeMap<&str, usize>,
) -> Vec<Value> {
    scenario
        .actions
        .iter()
        .map(|action| {
            let expectation = match &action.expectation {
                ActionExpectation::Success => json!({"kind": "success"}),
                ActionExpectation::TypedFailure { code, kind } => json!({
                    "kind": "typed_failure",
                    "code": code.as_str(),
                    "failure_kind": kind,
                }),
            };
            let action_shape = match &action.action {
                ActionKind::McpRequest { method, .. } => {
                    json!({"kind": "mcp_request", "operation": method.as_str()})
                }
                ActionKind::CliJson { arguments } => {
                    json!({"kind": "cli_json", "argument_count": arguments.len()})
                }
                ActionKind::HookEvent { event, .. } => {
                    json!({"kind": "hook_event", "event": event.as_str()})
                }
                ActionKind::Observe { source, .. } => {
                    json!({"kind": "observe", "source": source})
                }
                ActionKind::Restart => json!({"kind": "restart"}),
                ActionKind::Barrier { .. } => json!({"kind": "barrier"}),
            };
            json!({
                "actor": actor_indices[action.actor.as_str()],
                "after": action.after.iter().map(|step| step_indices[step.as_str()]).collect::<Vec<_>>(),
                "expectation": expectation,
                "action": action_shape,
            })
        })
        .collect()
}

fn scenario_fault_shapes(
    scenario: &ScenarioDefinition,
    actor_indices: &BTreeMap<&str, usize>,
    step_indices: &BTreeMap<&str, usize>,
) -> Vec<Value> {
    scenario
        .faults
        .iter()
        .map(|plan| match &plan.fault {
            FaultKind::Drop { target } => {
                json!({"kind": "drop", "target": step_indices[target.as_str()]})
            }
            FaultKind::Repeat { target, times } => json!({
                "kind": "repeat",
                "target": step_indices[target.as_str()],
                "times": times,
            }),
            FaultKind::Reorder { first, second } => json!({
                "kind": "reorder",
                "first": step_indices[first.as_str()],
                "second": step_indices[second.as_str()],
            }),
            FaultKind::Crash {
                target,
                actor,
                timing,
            } => json!({
                "kind": "crash",
                "target": step_indices[target.as_str()],
                "actor": actor_indices[actor.as_str()],
                "timing": timing,
            }),
            FaultKind::Concurrent { targets } => json!({
                "kind": "concurrent",
                "targets": targets.iter().map(|target| step_indices[target.as_str()]).collect::<Vec<_>>(),
            }),
        })
        .collect()
}

const fn invariant_tag(invariant: &InvariantKind) -> &'static str {
    match invariant {
        InvariantKind::ActiveTaskPerSession { .. } => "active_task_per_session",
        InvariantKind::CanonicalContinueKeepsRevision { .. } => "canonical_continue_keeps_revision",
        InvariantKind::StaleCasZeroWrites { .. } => "stale_cas_zero_writes",
        InvariantKind::OpenEpisodeHasNoCandidate { .. } => "open_episode_has_no_candidate",
        InvariantKind::TurnStopIsIdempotent { .. } => "turn_stop_is_idempotent",
        InvariantKind::CandidateSourceEpisode { .. } => "candidate_source_episode",
        InvariantKind::CandidateIsNotAutoInjected { .. } => "candidate_is_not_auto_injected",
        InvariantKind::ConfirmationIsAtomic { .. } => "confirmation_is_atomic",
        InvariantKind::SessionEndDoesNotCloseEpisode { .. } => "session_end_does_not_close_episode",
        InvariantKind::WorkingIntentHintHasNoGraphPath { .. } => {
            "working_intent_hint_has_no_graph_path"
        }
        InvariantKind::ArtifactFocusIsRequestScoped { .. } => "artifact_focus_is_request_scoped",
        InvariantKind::RawContentIsAbsent { .. } => "raw_content_is_absent",
    }
}

fn item_from_outcome(
    definition: &ScenarioDefinition,
    scenario: String,
    seed: u64,
    outcome: &RunOutcome,
    contract_digest: &str,
) -> ReplayReportItem {
    if !outcome_matches_contract(definition, seed, outcome) {
        return finalize_item(
            ReplayReportItem {
                scenario,
                seed,
                duration_ms: 0,
                step_count: outcome.steps.len(),
                assertion_count: outcome.assertions.len(),
                expected_failure_step_count: 0,
                failed_assertion_count: 0,
                disposition: ReplayDisposition::Classified,
                classification: Some(ReplayClassification::InfrastructureFlake),
                diagnostic_code: ReplayDiagnosticCode::RunnerOutcomeIncomplete,
                semantic_digest: String::new(),
            },
            &contract_digest,
        );
    }
    let expected_failure_step_count = outcome
        .steps
        .iter()
        .filter(|step| step.status == StepStatus::ExpectedFailure)
        .count();
    let failed_assertion_count = outcome
        .assertions
        .iter()
        .filter(|assertion| !assertion.passed)
        .count();
    let (disposition, classification, diagnostic_code) = if failed_assertion_count > 0 {
        (
            ReplayDisposition::Classified,
            Some(ReplayClassification::ProductInvariantViolation),
            ReplayDiagnosticCode::ClosedInvariantFailed,
        )
    } else if expected_failure_step_count > 0 {
        (
            ReplayDisposition::Classified,
            Some(ReplayClassification::ExpectedFailOpen),
            ReplayDiagnosticCode::ExpectedFailureObserved,
        )
    } else {
        (
            ReplayDisposition::Passed,
            None,
            ReplayDiagnosticCode::AllAssertionsPassed,
        )
    };
    let trace = OutcomeTrace {
        contract_digest,
        steps: outcome
            .steps
            .iter()
            .map(|step| StepTrace {
                occurrence: step.occurrence,
                status: step.status,
                error_code: step.error_code.as_deref(),
                error_kind: step.error_kind.as_deref(),
            })
            .collect(),
        assertions: outcome
            .assertions
            .iter()
            .map(|assertion| AssertionTrace {
                passed: assertion.passed,
                diagnostic_code: &assertion.diagnostic_code,
            })
            .collect(),
    };
    finalize_item(
        ReplayReportItem {
            scenario,
            seed,
            duration_ms: 0,
            step_count: outcome.steps.len(),
            assertion_count: outcome.assertions.len(),
            expected_failure_step_count,
            failed_assertion_count,
            disposition,
            classification,
            diagnostic_code,
            semantic_digest: String::new(),
        },
        &trace,
    )
}

fn outcome_matches_contract(
    definition: &ScenarioDefinition,
    seed: u64,
    outcome: &RunOutcome,
) -> bool {
    if outcome.scenario != definition.name.as_str() || outcome.seed != seed {
        return false;
    }
    let action_indices = definition
        .actions
        .iter()
        .enumerate()
        .map(|(index, action)| (action.id.as_str(), index))
        .collect::<BTreeMap<_, _>>();
    let Ok(schedule) = compile_schedule(definition, seed) else {
        return false;
    };
    let mut expected = BTreeMap::new();
    for planned in schedule.into_iter().flat_map(|batch| batch.steps) {
        if expected
            .insert((planned.index, planned.occurrence), planned)
            .is_some()
        {
            return false;
        }
    }
    let mut observed = BTreeMap::new();
    for step in &outcome.steps {
        let Some(index) = action_indices.get(step.step.as_str()).copied() else {
            return false;
        };
        if observed.insert((index, step.occurrence), step).is_some() {
            return false;
        }
    }
    if observed.len() != expected.len() {
        return false;
    }
    for (key, planned) in expected {
        let Some(step) = observed.get(&key) else {
            return false;
        };
        if !step_matches_plan(&definition.actions[planned.index], &planned, step) {
            return false;
        }
    }
    definition
        .assertions
        .iter()
        .map(|assertion| assertion.id.as_str())
        .eq(outcome
            .assertions
            .iter()
            .map(|assertion| assertion.id.as_str()))
}

fn step_matches_plan(
    action: &sctx_scenario_contract::ScenarioAction,
    planned: &PlannedStep,
    step: &StepRecord,
) -> bool {
    match planned.disposition {
        PlannedDisposition::Drop => step.status == StepStatus::Dropped && has_no_error_fields(step),
        PlannedDisposition::Barrier => {
            step.status == StepStatus::Barrier && has_no_error_fields(step)
        }
        PlannedDisposition::Execute => match planned.crash {
            Some(CrashTiming::Before) => {
                step.status == StepStatus::CrashedBefore && has_no_error_fields(step)
            }
            Some(CrashTiming::After) => {
                step.status == StepStatus::CompletedThenCrashed && has_no_error_fields(step)
            }
            None if matches!(action.action, ActionKind::Restart) => {
                step.status == StepStatus::Restarted && has_no_error_fields(step)
            }
            None => step_matches_expectation(&action.expectation, step),
        },
    }
}

fn has_no_error_fields(step: &StepRecord) -> bool {
    step.error_code.is_none() && step.error_kind.is_none()
}

fn step_matches_expectation(expectation: &ActionExpectation, step: &StepRecord) -> bool {
    match expectation {
        ActionExpectation::Success => {
            step.status == StepStatus::Completed && has_no_error_fields(step)
        }
        ActionExpectation::TypedFailure { code, kind } => {
            step.status == StepStatus::ExpectedFailure
                && step.error_code.as_deref() == Some(code.as_str())
                && step.error_kind.as_deref() == Some(expected_failure_kind(*kind))
        }
    }
}

fn item_from_runner_failure(
    scenario: String,
    seed: u64,
    step_count: usize,
    assertion_count: usize,
    failure: FailureClassification,
    contract_digest: &str,
) -> ReplayReportItem {
    let (classification, diagnostic_code) = match failure {
        FailureClassification::InvalidScenario
        | FailureClassification::PolicyViolation
        | FailureClassification::CaptureFailure => (
            ReplayClassification::InvalidScenario,
            ReplayDiagnosticCode::ScenarioContractInvalid,
        ),
        FailureClassification::ProcessFailure
        | FailureClassification::Timeout
        | FailureClassification::ObserverFailure => (
            ReplayClassification::InfrastructureFlake,
            ReplayDiagnosticCode::RunnerInfrastructureFailure,
        ),
    };
    finalize_item(
        ReplayReportItem {
            scenario,
            seed,
            duration_ms: 0,
            step_count,
            assertion_count,
            expected_failure_step_count: 0,
            failed_assertion_count: 0,
            disposition: ReplayDisposition::Classified,
            classification: Some(classification),
            diagnostic_code,
            semantic_digest: String::new(),
        },
        &(contract_digest, failure),
    )
}

fn item_from_contract_error(seed: u64, error: ContractErrorKind) -> ReplayReportItem {
    let (classification, diagnostic_code) = match error {
        ContractErrorKind::InvalidJson => (
            ReplayClassification::CorruptData,
            ReplayDiagnosticCode::ScenarioJsonCorrupt,
        ),
        ContractErrorKind::UnsupportedSchema | ContractErrorKind::UnsupportedVersion => (
            ReplayClassification::UnsupportedVersion,
            ReplayDiagnosticCode::ScenarioVersionUnsupported,
        ),
        _ => (
            ReplayClassification::InvalidScenario,
            ReplayDiagnosticCode::ScenarioContractInvalid,
        ),
    };
    finalize_item(
        ReplayReportItem {
            scenario: INVALID_SCENARIO_NAME.to_owned(),
            seed,
            duration_ms: 0,
            step_count: 0,
            assertion_count: 0,
            expected_failure_step_count: 0,
            failed_assertion_count: 0,
            disposition: ReplayDisposition::Classified,
            classification: Some(classification),
            diagnostic_code,
            semantic_digest: String::new(),
        },
        &diagnostic_code,
    )
}

fn with_duration(mut item: ReplayReportItem, elapsed_ms: u128) -> ReplayReportItem {
    item.duration_ms = u64::try_from(elapsed_ms).unwrap_or(u64::MAX);
    item
}

fn finalize_item(mut item: ReplayReportItem, trace: &impl Serialize) -> ReplayReportItem {
    item.semantic_digest = item_digest(&item, trace);
    item
}

#[derive(Serialize)]
struct SemanticItem<'a> {
    scenario: &'a str,
    seed: u64,
    step_count: usize,
    assertion_count: usize,
    expected_failure_step_count: usize,
    failed_assertion_count: usize,
    disposition: ReplayDisposition,
    classification: Option<ReplayClassification>,
    diagnostic_code: ReplayDiagnosticCode,
}

#[derive(Serialize)]
struct StepTrace<'a> {
    occurrence: u8,
    status: StepStatus,
    error_code: Option<&'a str>,
    error_kind: Option<&'a str>,
}

#[derive(Serialize)]
struct AssertionTrace<'a> {
    passed: bool,
    diagnostic_code: &'a str,
}

#[derive(Serialize)]
struct OutcomeTrace<'a> {
    contract_digest: &'a str,
    steps: Vec<StepTrace<'a>>,
    assertions: Vec<AssertionTrace<'a>>,
}

#[derive(Serialize)]
struct SemanticEnvelope<'a, T> {
    item: SemanticItem<'a>,
    trace: &'a T,
}

fn item_digest<T: Serialize>(item: &ReplayReportItem, trace: &T) -> String {
    let semantic = SemanticItem {
        scenario: &item.scenario,
        seed: item.seed,
        step_count: item.step_count,
        assertion_count: item.assertion_count,
        expected_failure_step_count: item.expected_failure_step_count,
        failed_assertion_count: item.failed_assertion_count,
        disposition: item.disposition,
        classification: item.classification,
        diagnostic_code: item.diagnostic_code,
    };
    hash_serialized(&SemanticEnvelope {
        item: semantic,
        trace,
    })
}

fn report_digest(items: &[ReplayReportItem]) -> String {
    let digests = items
        .iter()
        .map(|item| item.semantic_digest.as_str())
        .collect::<Vec<_>>();
    hash_serialized(&digests)
}

fn hash_serialized(value: &impl Serialize) -> String {
    let encoded = serde_json::to_vec(value).expect("fixed replay semantic value serializes");
    let digest = Sha256::digest(encoded);
    format!("sha256:{digest:x}")
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, collections::BTreeMap};

    use sctx_scenario_contract::VariableKind;
    use serde_json::{Value, json};

    use super::*;
    use crate::{AssertionRecord, CapturedValue, StepRecord};

    const VALID: &[u8] =
        include_bytes!("../../scenario-contract/tests/fixtures/valid/codex-v1.json");

    #[derive(Clone)]
    enum MockResult {
        Outcome(RunOutcome),
        Failure(RunnerFailure),
    }

    struct MockExecutor {
        calls: Cell<usize>,
        result: MockResult,
    }

    impl MockExecutor {
        fn outcome(outcome: RunOutcome) -> Self {
            Self {
                calls: Cell::new(0),
                result: MockResult::Outcome(outcome),
            }
        }

        fn failure(failure: RunnerFailure) -> Self {
            Self {
                calls: Cell::new(0),
                result: MockResult::Failure(failure),
            }
        }
    }

    impl ReplayExecutor for MockExecutor {
        fn execute_replay(
            &self,
            scenario: &ScenarioDefinition,
            seed: u64,
        ) -> Result<RunOutcome, RunnerFailure> {
            self.calls.set(self.calls.get() + 1);
            match &self.result {
                MockResult::Outcome(template) => {
                    let mut outcome = scheduled_outcome(scenario, seed);
                    let status = template.steps[0].status;
                    let error_code = template.steps[0].error_code.clone();
                    let error_kind = template.steps[0].error_kind.clone();
                    let assertion_passed = template.assertions[0].passed;
                    let assertion_diagnostic = template.assertions[0].diagnostic_code.clone();
                    let target_index = scenario
                        .actions
                        .iter()
                        .position(|action| {
                            matches!(action.expectation, ActionExpectation::TypedFailure { .. })
                        })
                        .unwrap_or(0);
                    let target = scenario.actions[target_index].id.as_str();
                    for step in outcome.steps.iter_mut().filter(|step| step.step == target) {
                        step.status = status;
                        step.error_code.clone_from(&error_code);
                        step.error_kind.clone_from(&error_kind);
                    }
                    outcome.variables.clone_from(&template.variables);
                    for assertion in &mut outcome.assertions {
                        assertion.passed = assertion_passed;
                        assertion.diagnostic_code.clone_from(&assertion_diagnostic);
                    }
                    Ok(outcome)
                }
                MockResult::Failure(failure) => Err(failure.clone()),
            }
        }
    }

    fn outcome(step: StepStatus, assertion_passed: bool, raw_variable: &str) -> RunOutcome {
        RunOutcome {
            scenario: "ignored-dynamic-runner-name".to_owned(),
            seed: 999,
            steps: vec![StepRecord {
                step: "safe-step".to_owned(),
                occurrence: 0,
                status: step,
                error_code: (step == StepStatus::ExpectedFailure)
                    .then(|| "intent_stale".to_owned()),
                error_kind: (step == StepStatus::ExpectedFailure).then(|| "stale_state".to_owned()),
            }],
            variables: BTreeMap::from([(
                "ignored-variable".to_owned(),
                CapturedValue {
                    value_type: VariableKind::Text,
                    value: Value::String(raw_variable.to_owned()),
                },
            )]),
            assertions: vec![AssertionRecord {
                id: "safe-assertion".to_owned(),
                passed: assertion_passed,
                diagnostic_code: "safe_runner_diagnostic".to_owned(),
            }],
        }
    }

    fn one(executor: &impl ReplayExecutor, bytes: &[u8]) -> ReplayReportItem {
        ReplayHarness::new(executor)
            .run(&[ReplayScenario::new(bytes, &[7])])
            .items
            .into_iter()
            .next()
            .unwrap()
    }

    fn typed_failure_scenario() -> Vec<u8> {
        let mut scenario: Value = serde_json::from_slice(VALID).unwrap();
        let actions = scenario["actions"].as_array_mut().unwrap();
        actions.push(json!({
            "id": "expected-failure-probe",
            "actor": "task",
            "after": ["confirmation-observation"],
            "expectation": {
                "type": "typed_failure",
                "code": "intent_stale",
                "kind": "stale_state"
            },
            "action": {
                "type": "cli_json",
                "arguments": [
                    {"type": "string", "value": "space"},
                    {"type": "string", "value": "list"}
                ]
            }
        }));
        actions.push(json!({
            "id": "post-failure-observation",
            "actor": "task",
            "after": ["expected-failure-probe"],
            "action": {
                "type": "cli_json",
                "arguments": [
                    {"type": "string", "value": "space"},
                    {"type": "string", "value": "list"}
                ]
            }
        }));
        let bytes = serde_json::to_vec(&scenario).unwrap();
        parse_scenario(&bytes).unwrap();
        bytes
    }

    fn fault_schedule_scenario() -> ScenarioDefinition {
        let mut scenario: Value = serde_json::from_slice(VALID).unwrap();
        let actions = scenario["actions"].as_array_mut().unwrap();
        actions.extend([
            json!({
                "id": "fault-drop", "actor": "task", "after": ["confirmation-observation"],
                "action": {"type": "cli_json", "arguments": [
                    {"type": "string", "value": "space"},
                    {"type": "string", "value": "list"}
                ]}
            }),
            json!({
                "id": "fault-crash-before", "actor": "task", "after": ["fault-drop"],
                "action": {"type": "cli_json", "arguments": [
                    {"type": "string", "value": "space"},
                    {"type": "string", "value": "list"}
                ]}
            }),
            json!({
                "id": "restart-before", "actor": "session", "after": ["fault-crash-before"],
                "action": {"type": "restart"}
            }),
            json!({
                "id": "fault-crash-after", "actor": "task", "after": ["restart-before"],
                "action": {"type": "cli_json", "arguments": [
                    {"type": "string", "value": "space"},
                    {"type": "string", "value": "list"}
                ]}
            }),
            json!({
                "id": "restart-after", "actor": "session", "after": ["fault-crash-after"],
                "action": {"type": "restart"}
            }),
            json!({
                "id": "scheduled-barrier", "actor": "task", "after": ["restart-after"],
                "action": {"type": "barrier", "barrier": "report-barrier"}
            }),
            json!({
                "id": "fault-repeat", "actor": "task", "after": ["scheduled-barrier"],
                "action": {"type": "cli_json", "arguments": [
                    {"type": "string", "value": "space"},
                    {"type": "string", "value": "list"}
                ]}
            }),
        ]);
        scenario["faults"] = json!([
            {"id": "drop-plan", "fault": {"type": "drop", "target": "fault-drop"}},
            {"id": "before-plan", "fault": {
                "type": "crash", "target": "fault-crash-before", "actor": "task", "timing": "before"
            }},
            {"id": "after-plan", "fault": {
                "type": "crash", "target": "fault-crash-after", "actor": "task", "timing": "after"
            }},
            {"id": "repeat-plan", "fault": {"type": "repeat", "target": "fault-repeat", "times": 2}}
        ]);
        parse_scenario(&serde_json::to_vec(&scenario).unwrap()).unwrap()
    }

    fn scheduled_outcome(scenario: &ScenarioDefinition, seed: u64) -> RunOutcome {
        let steps = compile_schedule(scenario, seed)
            .unwrap()
            .into_iter()
            .flat_map(|batch| batch.steps)
            .map(|planned| {
                let action = &scenario.actions[planned.index];
                let status = match (planned.disposition, planned.crash) {
                    (PlannedDisposition::Drop, _) => StepStatus::Dropped,
                    (PlannedDisposition::Barrier, _) => StepStatus::Barrier,
                    (PlannedDisposition::Execute, Some(CrashTiming::Before)) => {
                        StepStatus::CrashedBefore
                    }
                    (PlannedDisposition::Execute, Some(CrashTiming::After)) => {
                        StepStatus::CompletedThenCrashed
                    }
                    (PlannedDisposition::Execute, None)
                        if matches!(action.action, ActionKind::Restart) =>
                    {
                        StepStatus::Restarted
                    }
                    (PlannedDisposition::Execute, None) => StepStatus::Completed,
                };
                StepRecord {
                    step: action.id.to_string(),
                    occurrence: planned.occurrence,
                    status,
                    error_code: None,
                    error_kind: None,
                }
            })
            .collect();
        RunOutcome {
            scenario: scenario.name.to_string(),
            seed,
            steps,
            variables: BTreeMap::new(),
            assertions: scenario
                .assertions
                .iter()
                .map(|assertion| AssertionRecord {
                    id: assertion.id.to_string(),
                    passed: true,
                    diagnostic_code: "safe_runner_diagnostic".to_owned(),
                })
                .collect(),
        }
    }

    fn item_for_outcome(scenario: &ScenarioDefinition, outcome: &RunOutcome) -> ReplayReportItem {
        item_from_outcome(
            scenario,
            safe_scenario_name(scenario.name.as_str()),
            outcome.seed,
            outcome,
            &scenario_semantic_digest(scenario),
        )
    }

    fn assert_incomplete(item: &ReplayReportItem) {
        assert_eq!(item.disposition, ReplayDisposition::Classified);
        assert_eq!(
            item.classification,
            Some(ReplayClassification::InfrastructureFlake)
        );
        assert_eq!(
            item.diagnostic_code,
            ReplayDiagnosticCode::RunnerOutcomeIncomplete
        );
    }

    #[test]
    fn classifies_corrupt_unsupported_and_invalid_contracts_without_execution() {
        let executor = MockExecutor::outcome(outcome(StepStatus::Completed, true, "unused"));
        let corrupt = one(&executor, br#"{"schema":"#);
        assert_eq!(
            corrupt.classification,
            Some(ReplayClassification::CorruptData)
        );

        let mut unsupported: Value = serde_json::from_slice(VALID).unwrap();
        unsupported["version"] = json!("999");
        let unsupported = one(&executor, &serde_json::to_vec(&unsupported).unwrap());
        assert_eq!(
            unsupported.classification,
            Some(ReplayClassification::UnsupportedVersion)
        );

        let mut unsupported_schema: Value = serde_json::from_slice(VALID).unwrap();
        unsupported_schema["schema"] = json!("shared-context.future-scenario");
        let unsupported_schema = one(&executor, &serde_json::to_vec(&unsupported_schema).unwrap());
        assert_eq!(
            unsupported_schema.classification,
            Some(ReplayClassification::UnsupportedVersion)
        );

        let mut invalid: Value = serde_json::from_slice(VALID).unwrap();
        invalid["actions"][1]["id"] = invalid["actions"][0]["id"].clone();
        let invalid = one(&executor, &serde_json::to_vec(&invalid).unwrap());
        assert_eq!(
            invalid.classification,
            Some(ReplayClassification::InvalidScenario)
        );
        assert_eq!(executor.calls.get(), 0);
    }

    #[test]
    fn distinguishes_pass_expected_fail_open_and_invariant_violation() {
        let passed = one(
            &MockExecutor::outcome(outcome(StepStatus::Completed, true, "ignored")),
            VALID,
        );
        assert_eq!(passed.disposition, ReplayDisposition::Passed);
        assert_eq!(passed.classification, None);

        let typed = typed_failure_scenario();
        let expected = one(
            &MockExecutor::outcome(outcome(StepStatus::ExpectedFailure, true, "ignored")),
            &typed,
        );
        assert_eq!(
            expected.classification,
            Some(ReplayClassification::ExpectedFailOpen)
        );

        let violated = one(
            &MockExecutor::outcome(outcome(StepStatus::ExpectedFailure, false, "ignored")),
            &typed,
        );
        assert_eq!(
            violated.classification,
            Some(ReplayClassification::ProductInvariantViolation)
        );
    }

    #[test]
    fn expected_failure_must_match_declared_action_code_and_kind() {
        let typed = typed_failure_scenario();

        let forged_on_success = one(
            &MockExecutor::outcome(outcome(StepStatus::ExpectedFailure, true, "ignored")),
            VALID,
        );
        assert_incomplete(&forged_on_success);

        let mut error_on_success = outcome(StepStatus::Completed, true, "ignored");
        error_on_success.steps[0].error_code = Some("intent_stale".to_owned());
        error_on_success.steps[0].error_kind = Some("stale_state".to_owned());
        assert_incomplete(&one(&MockExecutor::outcome(error_on_success), VALID));

        let completed_typed = one(
            &MockExecutor::outcome(outcome(StepStatus::Completed, true, "ignored")),
            &typed,
        );
        assert_incomplete(&completed_typed);

        let mut wrong_code = outcome(StepStatus::ExpectedFailure, true, "ignored");
        wrong_code.steps[0].error_code = Some("different_safe_code".to_owned());
        assert_incomplete(&one(&MockExecutor::outcome(wrong_code), &typed));

        let mut wrong_kind = outcome(StepStatus::ExpectedFailure, true, "ignored");
        wrong_kind.steps[0].error_kind = Some("conflict".to_owned());
        assert_incomplete(&one(&MockExecutor::outcome(wrong_kind), &typed));

        let mut missing_error = outcome(StepStatus::ExpectedFailure, true, "ignored");
        missing_error.steps[0].error_code = None;
        missing_error.steps[0].error_kind = None;
        assert_incomplete(&one(&MockExecutor::outcome(missing_error), &typed));
    }

    #[test]
    fn schedule_ledger_rejects_illegal_status_duplicate_and_missing_occurrence() {
        let scenario = fault_schedule_scenario();
        let base = scheduled_outcome(&scenario, 176);
        let mut reordered = base.clone();
        reordered.steps.reverse();
        assert_eq!(
            item_for_outcome(&scenario, &reordered).disposition,
            ReplayDisposition::Passed
        );

        for step_name in [
            "fault-drop",
            "fault-crash-before",
            "fault-crash-after",
            "scheduled-barrier",
            "restart-before",
        ] {
            let mut invalid = base.clone();
            invalid
                .steps
                .iter_mut()
                .find(|step| step.step == step_name)
                .unwrap()
                .status = StepStatus::Completed;
            assert_incomplete(&item_for_outcome(&scenario, &invalid));
        }

        let repeated = base
            .steps
            .iter()
            .find(|step| step.step == "fault-repeat" && step.occurrence == 0)
            .unwrap()
            .clone();
        let mut duplicate = base.clone();
        duplicate.steps.push(repeated.clone());
        assert_incomplete(&item_for_outcome(&scenario, &duplicate));

        let mut missing = base.clone();
        missing
            .steps
            .retain(|step| !(step.step == "fault-repeat" && step.occurrence == 1));
        assert_incomplete(&item_for_outcome(&scenario, &missing));

        let mut extra = base;
        extra.steps.push(StepRecord {
            occurrence: 2,
            ..repeated
        });
        assert_incomplete(&item_for_outcome(&scenario, &extra));
    }

    #[test]
    fn runner_failures_are_non_blocking_and_messages_are_never_reported() {
        let executor = MockExecutor::failure(RunnerFailure {
            scenario: "ignored".to_owned(),
            seed: 7,
            step: Some("ignored-step".to_owned()),
            classification: FailureClassification::Timeout,
            message: "raw prompt /private/machine/path SECRET_CANARY".to_owned(),
        });
        let report = ReplayHarness::new(&executor).run(&[
            ReplayScenario::new(VALID, &[1]),
            ReplayScenario::new(VALID, &[2]),
        ]);
        assert_eq!(report.items.len(), 2);
        assert!(report.items.iter().all(|item| {
            item.classification == Some(ReplayClassification::InfrastructureFlake)
        }));
        let encoded = serde_json::to_string(&report).unwrap();
        for forbidden in ["raw prompt", "/private/", "SECRET_CANARY", "ignored-step"] {
            assert!(!encoded.contains(forbidden));
        }
    }

    #[test]
    fn only_closed_phase_one_scenario_names_are_reported() {
        let executor = MockExecutor::outcome(outcome(StepStatus::Completed, true, "ignored"));
        let mut private: Value = serde_json::from_slice(VALID).unwrap();
        private["name"] = json!("client_alpha_private_workflow");
        let private_bytes = serde_json::to_vec(&private).unwrap();
        let item = one(&executor, &private_bytes);
        assert_eq!(item.scenario, REDACTED_SCENARIO_NAME);
        assert!(
            !serde_json::to_string(&item)
                .unwrap()
                .contains("client_alpha_private_workflow")
        );

        let mut supported: Value = serde_json::from_slice(VALID).unwrap();
        supported["name"] = json!("codex_normal_candidate_confirm");
        let supported_bytes = serde_json::to_vec(&supported).unwrap();
        let item = one(&executor, &supported_bytes);
        assert_eq!(item.scenario, "codex_normal_candidate_confirm");
    }

    #[test]
    fn semantic_digest_ignores_timing_dynamic_values_and_runner_diagnostics() {
        let first = ReplayHarness::new(&MockExecutor::outcome(outcome(
            StepStatus::Completed,
            true,
            "tsk_dynamic-one /private/one",
        )))
        .run(&[ReplayScenario::new(VALID, &[11])]);
        let second = ReplayHarness::new(&MockExecutor::outcome(outcome(
            StepStatus::Completed,
            true,
            "tsk_dynamic-two /private/two",
        )))
        .run(&[ReplayScenario::new(VALID, &[11])]);
        assert_eq!(first.semantic_digest, second.semantic_digest);
        assert_eq!(
            first.items[0].semantic_digest,
            second.items[0].semantic_digest
        );
        let encoded = serde_json::to_string(&first).unwrap();
        assert!(!encoded.contains("tsk_dynamic"));
        assert!(!encoded.contains("/private/"));

        let mut private_contract: Value = serde_json::from_slice(VALID).unwrap();
        private_contract["actions"][0]["action"]["payload"]["fields"]["source"]["value"] =
            json!("SECRET_CONTRACT_VALUE");
        let private_report = ReplayHarness::new(&MockExecutor::outcome(outcome(
            StepStatus::Completed,
            true,
            "ignored",
        )))
        .run(&[ReplayScenario::new(
            &serde_json::to_vec(&private_contract).unwrap(),
            &[11],
        )]);
        assert_eq!(first.semantic_digest, private_report.semantic_digest);
        assert!(
            !serde_json::to_string(&private_report)
                .unwrap()
                .contains("SECRET_CONTRACT_VALUE")
        );

        let mut changed_diagnostic = outcome(StepStatus::Completed, true, "ignored");
        changed_diagnostic.assertions[0].diagnostic_code = "changed_safe_diagnostic".to_owned();
        let changed = ReplayHarness::new(&MockExecutor::outcome(changed_diagnostic))
            .run(&[ReplayScenario::new(VALID, &[11])]);
        assert_ne!(first.semantic_digest, changed.semantic_digest);
    }

    #[test]
    fn incomplete_runner_outcome_is_never_reported_as_passed() {
        struct IncompleteExecutor;

        impl ReplayExecutor for IncompleteExecutor {
            fn execute_replay(
                &self,
                scenario: &ScenarioDefinition,
                seed: u64,
            ) -> Result<RunOutcome, RunnerFailure> {
                Ok(RunOutcome {
                    scenario: scenario.name.to_string(),
                    seed,
                    steps: Vec::new(),
                    variables: BTreeMap::new(),
                    assertions: Vec::new(),
                })
            }
        }

        let item = one(&IncompleteExecutor, VALID);
        assert_incomplete(&item);
    }
}
