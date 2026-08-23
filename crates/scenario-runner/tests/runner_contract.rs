use std::{
    fs,
    path::PathBuf,
    process::Command,
    time::{Duration, Instant},
};

use sctx_scenario_contract::{
    ActionKind, ActorId, ActorKind, AgentFraming, AgentProfile, AgentVendor, AgentVersion,
    AssertionId, BarrierId, CaptureSource, CrashTiming, EventClassification, EventSupport, FaultId,
    FaultKind, FaultPlan, InvariantAssertion, InvariantKind, JsonPointer, ObservationSource,
    ProductAction, ScenarioAction, ScenarioActor, ScenarioContractVersion, ScenarioDefinition,
    ScenarioSchema, ScenarioVariable, StepId, TemplateValue, VariableKind, VariableName, WireName,
};
use sctx_scenario_runner::{FailureClassification, RunnerConfig, ScenarioRunner, StepStatus};
use tempfile::tempdir;

fn fake_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_sctx-scenario-fake"))
}

fn runner(parent: &std::path::Path, timeout: Duration) -> ScenarioRunner {
    ScenarioRunner::new(
        RunnerConfig::new(fake_binary(), "/usr/bin/git")
            .with_sandbox_parent(parent)
            .with_step_timeout(timeout),
    )
}

fn identifier(value: &str) -> StepId {
    StepId::new(value).unwrap()
}

fn string(value: &str) -> TemplateValue {
    TemplateValue::String {
        value: value.to_owned(),
    }
}

fn unsigned(value: u64) -> TemplateValue {
    TemplateValue::Unsigned { value }
}

fn variable(value: &str) -> TemplateValue {
    TemplateValue::Variable {
        name: VariableName::new(value).unwrap(),
    }
}

fn object(fields: impl IntoIterator<Item = (&'static str, TemplateValue)>) -> TemplateValue {
    TemplateValue::Object {
        fields: fields
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    }
}

fn cli(id: &str, after: &[&str], arguments: &[TemplateValue]) -> ScenarioAction {
    ScenarioAction {
        id: identifier(id),
        actor: ActorId::new("task").unwrap(),
        after: after.iter().map(|step| identifier(step)).collect(),
        action: ActionKind::CliJson {
            arguments: arguments.to_vec(),
        },
    }
}

fn mcp(id: &str, after: &[&str], method: &str, params: TemplateValue) -> ScenarioAction {
    ScenarioAction {
        id: identifier(id),
        actor: ActorId::new("task").unwrap(),
        after: after.iter().map(|step| identifier(step)).collect(),
        action: ActionKind::McpRequest {
            method: WireName::new(method).unwrap(),
            params,
        },
    }
}

fn restart(id: &str, after: &[&str]) -> ScenarioAction {
    ScenarioAction {
        id: identifier(id),
        actor: ActorId::new("session").unwrap(),
        after: after.iter().map(|step| identifier(step)).collect(),
        action: ActionKind::Restart,
    }
}

fn hook(id: &str, after: &[&str], value: &str) -> ScenarioAction {
    ScenarioAction {
        id: identifier(id),
        actor: ActorId::new("session").unwrap(),
        after: after.iter().map(|step| identifier(step)).collect(),
        action: ActionKind::HookEvent {
            event: WireName::new("SessionStart").unwrap(),
            payload: object([("value", string(value))]),
        },
    }
}

fn observation(after: &[&str]) -> ScenarioAction {
    ScenarioAction {
        id: identifier("observe"),
        actor: ActorId::new("task").unwrap(),
        after: after.iter().map(|step| identifier(step)).collect(),
        action: ActionKind::Observe {
            source: ObservationSource::Runtime,
            selector: object([("entity", string("work_episode"))]),
        },
    }
}

fn capture(name: &str, kind: VariableKind, step: &str, pointer: &str) -> ScenarioVariable {
    ScenarioVariable {
        name: VariableName::new(name).unwrap(),
        value_type: kind,
        capture: CaptureSource {
            step: identifier(step),
            pointer: JsonPointer::new(pointer).unwrap(),
        },
    }
}

fn scenario(
    name: &str,
    vendor: AgentVendor,
    mut actions: Vec<ScenarioAction>,
    variables: Vec<ScenarioVariable>,
    faults: Vec<FaultPlan>,
) -> ScenarioDefinition {
    let prior = actions
        .iter()
        .map(|action| action.id.as_str().to_owned())
        .collect::<Vec<_>>();
    actions.push(observation(
        &prior.iter().map(String::as_str).collect::<Vec<_>>(),
    ));
    ScenarioDefinition {
        schema: ScenarioSchema::DynamicScenario,
        version: ScenarioContractVersion::V1,
        name: WireName::new(name).unwrap(),
        agent: AgentProfile {
            vendor,
            version: AgentVersion::new(match vendor {
                AgentVendor::Cursor => "3.13.2",
                AgentVendor::Codex => "0.147.0",
            })
            .unwrap(),
            framing: match vendor {
                AgentVendor::Cursor => AgentFraming::NewlineDelimitedJson,
                AgentVendor::Codex => AgentFraming::ContentLength,
            },
        },
        actors: vec![
            ScenarioActor {
                id: ActorId::new("session").unwrap(),
                kind: ActorKind::Session,
            },
            ScenarioActor {
                id: ActorId::new("task").unwrap(),
                kind: ActorKind::Task {
                    session: ActorId::new("session").unwrap(),
                },
            },
        ],
        actions,
        variables,
        faults,
        assertions: vec![InvariantAssertion {
            id: AssertionId::new("observer-is-typed").unwrap(),
            invariant: InvariantKind::WorkingIntentHintHasNoGraphPath {
                observation: identifier("observe"),
            },
        }],
        events: vec![EventClassification {
            event: WireName::new("SessionStart").unwrap(),
            classification: EventSupport::Supported,
            product_action: Some(ProductAction::SessionStart),
        }],
    }
}

fn fault(id: &str, fault: FaultKind) -> FaultPlan {
    FaultPlan {
        id: FaultId::new(id).unwrap(),
        fault,
    }
}

fn execution_log(run: &sctx_scenario_runner::ScenarioRun) -> Vec<String> {
    fs::read_to_string(run.home().join("fake-executions.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn persistent_mcp_supports_both_framings_and_real_variable_chaining() {
    for vendor in [AgentVendor::Cursor, AgentVendor::Codex] {
        let temporary = tempdir().unwrap();
        let contract = scenario(
            "mcp_variable_chain",
            vendor,
            vec![
                mcp(
                    "create",
                    &[],
                    "fake-echo",
                    object([("value", string("safe-value"))]),
                ),
                mcp(
                    "consume",
                    &["create"],
                    "fake-echo",
                    object([("task_id", variable("task-id"))]),
                ),
            ],
            vec![
                capture("task-id", VariableKind::TaskId, "create", "/task_id"),
                capture(
                    "echoed-task-id",
                    VariableKind::TaskId,
                    "consume",
                    "/task_id",
                ),
            ],
            vec![],
        );
        let run = runner(temporary.path(), Duration::from_secs(2))
            .run(&contract, 11)
            .unwrap();
        assert_eq!(
            run.outcome.variables["task-id"].value,
            run.outcome.variables["echoed-task-id"].value
        );
        assert_eq!(
            execution_log(&run)
                .iter()
                .filter(|line| line.contains("mcp,serve"))
                .count(),
            1,
            "MCP must remain persistent across tool actions"
        );
    }
}

#[test]
fn restart_reinitializes_the_persistent_mcp_child() {
    let temporary = tempdir().unwrap();
    let contract = scenario(
        "mcp_restart",
        AgentVendor::Codex,
        vec![
            mcp("first", &[], "fake-echo", object([])),
            restart("restart", &["first"]),
            mcp("second", &["restart"], "fake-echo", object([])),
        ],
        vec![
            capture("first-pid", VariableKind::Count, "first", "/process_id"),
            capture("second-pid", VariableKind::Count, "second", "/process_id"),
        ],
        vec![],
    );
    let run = runner(temporary.path(), Duration::from_secs(2))
        .run(&contract, 12)
        .unwrap();
    assert_ne!(
        run.outcome.variables["first-pid"].value,
        run.outcome.variables["second-pid"].value
    );
    assert_eq!(
        run.outcome
            .steps
            .iter()
            .find(|step| step.step == "restart")
            .unwrap()
            .status,
        StepStatus::Restarted
    );
}

#[test]
fn mcp_restart_is_scoped_to_its_session_actor() {
    let temporary = tempdir().unwrap();
    let mut contract = scenario(
        "session_scoped_restart",
        AgentVendor::Codex,
        vec![
            mcp("alpha-first", &[], "fake-echo", object([])),
            mcp("beta-first", &[], "fake-echo", object([])),
            restart("restart-alpha", &["alpha-first", "beta-first"]),
            mcp("alpha-second", &["restart-alpha"], "fake-echo", object([])),
            mcp("beta-second", &["restart-alpha"], "fake-echo", object([])),
        ],
        vec![
            capture(
                "alpha-first-pid",
                VariableKind::Count,
                "alpha-first",
                "/process_id",
            ),
            capture(
                "alpha-second-pid",
                VariableKind::Count,
                "alpha-second",
                "/process_id",
            ),
            capture(
                "beta-first-pid",
                VariableKind::Count,
                "beta-first",
                "/process_id",
            ),
            capture(
                "beta-second-pid",
                VariableKind::Count,
                "beta-second",
                "/process_id",
            ),
        ],
        vec![],
    );
    contract.actors.extend([
        ScenarioActor {
            id: ActorId::new("session-beta").unwrap(),
            kind: ActorKind::Session,
        },
        ScenarioActor {
            id: ActorId::new("task-beta").unwrap(),
            kind: ActorKind::Task {
                session: ActorId::new("session-beta").unwrap(),
            },
        },
    ]);
    for step in ["beta-first", "beta-second"] {
        contract
            .actions
            .iter_mut()
            .find(|action| action.id.as_str() == step)
            .unwrap()
            .actor = ActorId::new("task-beta").unwrap();
    }

    let run = runner(temporary.path(), Duration::from_secs(2))
        .run(&contract, 121)
        .unwrap();
    assert_ne!(
        run.outcome.variables["alpha-first-pid"].value,
        run.outcome.variables["alpha-second-pid"].value
    );
    assert_eq!(
        run.outcome.variables["beta-first-pid"].value,
        run.outcome.variables["beta-second-pid"].value
    );
}

#[test]
fn cli_and_hook_json_are_isolated_and_capture_only_selected_scalars() {
    let temporary = tempdir().unwrap();
    let contract = scenario(
        "cli_hook_capture",
        AgentVendor::Codex,
        vec![
            hook("hook", &[], "hook-value"),
            cli(
                "cli",
                &["hook"],
                &[string("fake-echo"), string("cli-value")],
            ),
        ],
        vec![
            capture("hook-value", VariableKind::Status, "hook", "/value"),
            capture("cli-value", VariableKind::Status, "cli", "/value"),
            capture(
                "network-disabled",
                VariableKind::Boolean,
                "cli",
                "/network_disabled",
            ),
            capture(
                "model-home-isolated",
                VariableKind::Boolean,
                "cli",
                "/model_home_isolated",
            ),
        ],
        vec![],
    );
    let run = runner(temporary.path(), Duration::from_secs(2))
        .run(&contract, 13)
        .unwrap();
    assert_eq!(run.outcome.variables["hook-value"].value, "hook-value");
    assert_eq!(run.outcome.variables["cli-value"].value, "cli-value");
    assert_eq!(run.outcome.variables["network-disabled"].value, true);
    assert_eq!(run.outcome.variables["model-home-isolated"].value, true);
    assert!(run.home().starts_with(temporary.path()));
    assert!(run.workspace().starts_with(temporary.path()));
}

#[test]
fn text_capture_is_allowed_only_for_a_typed_observer_summary() {
    let temporary = tempdir().unwrap();
    let contract = scenario(
        "observer_text_capture",
        AgentVendor::Cursor,
        vec![cli("safe", &[], &[string("fake-echo")])],
        vec![capture(
            "observer-entity",
            VariableKind::Text,
            "observe",
            "/entity",
        )],
        vec![],
    );
    let run = runner(temporary.path(), Duration::from_secs(2))
        .run(&contract, 131)
        .unwrap();
    assert_eq!(
        run.outcome.variables["observer-entity"].value,
        "work_episode"
    );
}

#[test]
fn aliased_secret_text_capture_is_rejected_before_the_fake_can_run() {
    let fixture_home = tempdir().unwrap();
    let fixture_output = Command::new(fake_binary())
        .args(["--json", "fake-secret-data"])
        .env("HOME", fixture_home.path())
        .output()
        .unwrap();
    assert!(fixture_output.status.success());
    let fixture_json: serde_json::Value = serde_json::from_slice(&fixture_output.stdout).unwrap();
    assert_eq!(
        fixture_json["data"]["data"],
        "SECRET_ALIASED_RAW_DATA_MUST_NOT_ESCAPE"
    );

    let parent = tempdir().unwrap();
    let raw_capture = scenario(
        "invalid_raw_capture",
        AgentVendor::Cursor,
        vec![cli("source", &[], &[string("fake-secret-data")])],
        vec![capture("captured", VariableKind::Text, "source", "/data")],
        vec![],
    );
    let failure = runner(parent.path(), Duration::from_secs(1))
        .run(&raw_capture, 132)
        .err()
        .expect("aliased raw content capture must fail preflight");
    assert_eq!(
        failure.classification,
        FailureClassification::PolicyViolation
    );
    assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 0);
    let report = serde_json::to_string(&failure).unwrap();
    assert!(!report.contains("SECRET_ALIASED_RAW_DATA_MUST_NOT_ESCAPE"));
    assert!(!report.contains("fake-secret-data"));
}

#[test]
fn repeat_drop_and_reorder_faults_execute_the_compiled_plan() {
    let temporary = tempdir().unwrap();
    let actions = vec![
        cli("alpha", &[], &[string("fake-echo"), string("alpha")]),
        cli("beta", &[], &[string("fake-echo"), string("beta")]),
    ];
    let contract = scenario(
        "fault_delivery",
        AgentVendor::Cursor,
        actions,
        vec![],
        vec![
            fault(
                "repeat-alpha",
                FaultKind::Repeat {
                    target: identifier("alpha"),
                    times: 3,
                },
            ),
            fault(
                "drop-beta",
                FaultKind::Drop {
                    target: identifier("beta"),
                },
            ),
        ],
    );
    let run = runner(temporary.path(), Duration::from_secs(2))
        .run(&contract, 14)
        .unwrap();
    assert_eq!(
        run.outcome
            .steps
            .iter()
            .filter(|step| step.step == "alpha")
            .count(),
        3
    );
    assert_eq!(
        run.outcome
            .steps
            .iter()
            .find(|step| step.step == "beta")
            .unwrap()
            .status,
        StepStatus::Dropped
    );

    let reorder = scenario(
        "fault_reorder",
        AgentVendor::Cursor,
        vec![
            cli("alpha", &[], &[string("fake-echo"), string("alpha")]),
            cli("beta", &[], &[string("fake-echo"), string("beta")]),
        ],
        vec![],
        vec![fault(
            "reorder-alpha-beta",
            FaultKind::Reorder {
                first: identifier("alpha"),
                second: identifier("beta"),
            },
        )],
    );
    let reordered = runner(temporary.path(), Duration::from_secs(2))
        .run(&reorder, 14)
        .unwrap();
    let names = reordered
        .outcome
        .steps
        .iter()
        .map(|step| step.step.as_str())
        .collect::<Vec<_>>();
    assert!(
        names.iter().position(|name| *name == "beta").unwrap()
            < names.iter().position(|name| *name == "alpha").unwrap()
    );
}

#[test]
fn concurrent_fault_runs_independent_one_shot_actions_in_parallel() {
    let temporary = tempdir().unwrap();
    let contract = scenario(
        "fault_concurrent",
        AgentVendor::Cursor,
        vec![
            cli(
                "alpha",
                &[],
                &[string("fake-sleep"), unsigned(300), string("alpha")],
            ),
            cli(
                "beta",
                &[],
                &[string("fake-sleep"), unsigned(300), string("beta")],
            ),
        ],
        vec![],
        vec![fault(
            "concurrent-alpha-beta",
            FaultKind::Concurrent {
                targets: vec![identifier("alpha"), identifier("beta")],
            },
        )],
    );
    let started = Instant::now();
    let run = runner(temporary.path(), Duration::from_secs(2))
        .run(&contract, 15)
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(2));
    let sleep_events = execution_log(&run)
        .into_iter()
        .filter(|line| line.starts_with("sleep-"))
        .collect::<Vec<_>>();
    assert_eq!(
        sleep_events,
        ["sleep-start", "sleep-start", "sleep-end", "sleep-end"],
        "both children must start before either concurrent action completes"
    );
    assert_eq!(
        run.outcome
            .steps
            .iter()
            .filter(|step| matches!(step.step.as_str(), "alpha" | "beta"))
            .count(),
        2
    );
}

#[test]
fn concurrent_mcp_actions_on_one_session_fail_before_sandbox_creation() {
    let parent = tempdir().unwrap();
    let contract = scenario(
        "invalid_same_session_mcp_concurrency",
        AgentVendor::Codex,
        vec![
            mcp(
                "alpha",
                &[],
                "fake-sleep",
                object([("sleep_ms", unsigned(100))]),
            ),
            mcp(
                "beta",
                &[],
                "fake-sleep",
                object([("sleep_ms", unsigned(100))]),
            ),
        ],
        vec![],
        vec![fault(
            "concurrent-alpha-beta",
            FaultKind::Concurrent {
                targets: vec![identifier("alpha"), identifier("beta")],
            },
        )],
    );
    let failure = runner(parent.path(), Duration::from_secs(2))
        .run(&contract, 151)
        .err()
        .expect("same-Session MCP concurrency must fail preflight");
    assert_eq!(
        failure.classification,
        FailureClassification::InvalidScenario
    );
    assert!(failure.message.contains("independent Session actors"));
    assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 0);
}

#[test]
fn concurrent_mcp_actions_on_independent_sessions_overlap() {
    let temporary = tempdir().unwrap();
    let mut contract = scenario(
        "independent_session_mcp_concurrency",
        AgentVendor::Codex,
        vec![
            mcp(
                "alpha",
                &[],
                "fake-sleep",
                object([("sleep_ms", unsigned(300))]),
            ),
            mcp(
                "beta",
                &[],
                "fake-sleep",
                object([("sleep_ms", unsigned(300))]),
            ),
        ],
        vec![],
        vec![fault(
            "concurrent-alpha-beta",
            FaultKind::Concurrent {
                targets: vec![identifier("alpha"), identifier("beta")],
            },
        )],
    );
    contract.actors.extend([
        ScenarioActor {
            id: ActorId::new("session-beta").unwrap(),
            kind: ActorKind::Session,
        },
        ScenarioActor {
            id: ActorId::new("task-beta").unwrap(),
            kind: ActorKind::Task {
                session: ActorId::new("session-beta").unwrap(),
            },
        },
    ]);
    contract
        .actions
        .iter_mut()
        .find(|action| action.id.as_str() == "beta")
        .unwrap()
        .actor = ActorId::new("task-beta").unwrap();

    let run = runner(temporary.path(), Duration::from_secs(2))
        .run(&contract, 152)
        .unwrap();
    let events = execution_log(&run)
        .into_iter()
        .filter(|line| line.starts_with("mcp-sleep-"))
        .collect::<Vec<_>>();
    assert_eq!(
        events,
        [
            "mcp-sleep-start",
            "mcp-sleep-start",
            "mcp-sleep-end",
            "mcp-sleep-end"
        ],
        "independent Session transports must overlap"
    );
}

#[test]
fn repeated_mcp_without_a_parallel_group_remains_valid() {
    let temporary = tempdir().unwrap();
    let contract = scenario(
        "sequential_mcp_repeat",
        AgentVendor::Codex,
        vec![mcp("repeated", &[], "fake-echo", object([]))],
        vec![],
        vec![fault(
            "repeat-mcp",
            FaultKind::Repeat {
                target: identifier("repeated"),
                times: 2,
            },
        )],
    );
    let run = runner(temporary.path(), Duration::from_secs(2))
        .run(&contract, 153)
        .unwrap();
    assert_eq!(
        run.outcome
            .steps
            .iter()
            .filter(|record| record.step == "repeated")
            .count(),
        2
    );
}

#[test]
fn shared_barrier_waits_for_all_participants() {
    let temporary = tempdir().unwrap();
    let mut contract = scenario(
        "shared_barrier",
        AgentVendor::Cursor,
        vec![
            cli("alpha", &[], &[string("fake-echo"), string("alpha")]),
            cli("beta", &[], &[string("fake-echo"), string("beta")]),
            ScenarioAction {
                id: identifier("alpha-barrier"),
                actor: ActorId::new("task").unwrap(),
                after: vec![identifier("alpha")],
                action: ActionKind::Barrier {
                    barrier: BarrierId::new("join").unwrap(),
                },
            },
            ScenarioAction {
                id: identifier("beta-barrier"),
                actor: ActorId::new("session").unwrap(),
                after: vec![identifier("beta")],
                action: ActionKind::Barrier {
                    barrier: BarrierId::new("join").unwrap(),
                },
            },
        ],
        vec![],
        vec![],
    );
    contract.actions.last_mut().unwrap().after =
        vec![identifier("alpha-barrier"), identifier("beta-barrier")];
    let run = runner(temporary.path(), Duration::from_secs(2))
        .run(&contract, 16)
        .unwrap();
    let barrier_records = run
        .outcome
        .steps
        .iter()
        .filter(|step| step.status == StepStatus::Barrier)
        .collect::<Vec<_>>();
    assert_eq!(barrier_records.len(), 2);
    assert_eq!(barrier_records[0].step, "alpha-barrier");
    assert_eq!(barrier_records[1].step, "beta-barrier");
}

#[test]
fn crash_before_and_after_require_explicit_restart() {
    for timing in [CrashTiming::Before, CrashTiming::After] {
        let temporary = tempdir().unwrap();
        let contract = scenario(
            "crash_restart",
            AgentVendor::Codex,
            vec![
                mcp("faulted", &[], "fake-echo", object([])),
                restart("restart", &["faulted"]),
                mcp("recovered", &["restart"], "fake-echo", object([])),
            ],
            vec![],
            vec![fault(
                "crash-faulted",
                FaultKind::Crash {
                    target: identifier("faulted"),
                    actor: ActorId::new("task").unwrap(),
                    timing,
                },
            )],
        );
        let run = runner(temporary.path(), Duration::from_secs(2))
            .run(&contract, 17)
            .unwrap();
        assert_eq!(
            run.outcome.steps[0].status,
            match timing {
                CrashTiming::Before => StepStatus::CrashedBefore,
                CrashTiming::After => StepStatus::CompletedThenCrashed,
            }
        );
        assert!(
            run.outcome
                .steps
                .iter()
                .any(|step| { step.step == "restart" && step.status == StepStatus::Restarted })
        );
        assert!(
            run.outcome
                .steps
                .iter()
                .any(|step| step.step == "recovered")
        );
    }
}

#[test]
fn seeded_scheduler_is_stable_and_only_reorders_causally_ready_steps() {
    let contract = scenario(
        "seeded_schedule",
        AgentVendor::Cursor,
        vec![
            cli("alpha", &[], &[string("fake-echo"), string("alpha")]),
            cli("beta", &[], &[string("fake-echo"), string("beta")]),
            cli("gamma", &[], &[string("fake-echo"), string("gamma")]),
        ],
        vec![],
        vec![],
    );
    let first_parent = tempdir().unwrap();
    let second_parent = tempdir().unwrap();
    let first = runner(first_parent.path(), Duration::from_secs(2))
        .run(&contract, 21)
        .unwrap();
    let second = runner(second_parent.path(), Duration::from_secs(2))
        .run(&contract, 21)
        .unwrap();
    assert_eq!(
        serde_json::to_vec(&first.outcome).unwrap(),
        serde_json::to_vec(&second.outcome).unwrap()
    );

    let different = (22..100)
        .find_map(|seed| {
            let parent = tempdir().unwrap();
            let run = runner(parent.path(), Duration::from_secs(2))
                .run(&contract, seed)
                .unwrap();
            (run.outcome.steps != first.outcome.steps).then_some(run.outcome.steps)
        })
        .expect("some seed must choose another legal ready-step order");
    assert_eq!(different.last().unwrap().step, "observe");
}

#[test]
fn timeout_exit_and_invalid_json_failures_are_sanitized() {
    let timeout_parent = tempdir().unwrap();
    let timeout = scenario(
        "safe_timeout",
        AgentVendor::Cursor,
        vec![cli("failure", &[], &[string("fake-sleep"), unsigned(500)])],
        vec![],
        vec![],
    );
    let timeout_failure = runner(timeout_parent.path(), Duration::from_millis(50))
        .run(&timeout, 31)
        .err()
        .expect("fake timeout must be reported");
    assert_eq!(
        timeout_failure.classification,
        FailureClassification::Timeout
    );

    for arguments in [vec![string("fake-exit")], vec![string("fake-invalid")]] {
        let temporary = tempdir().unwrap();
        let contract = scenario(
            "safe_failure",
            AgentVendor::Cursor,
            vec![cli("failure", &[], &arguments)],
            vec![],
            vec![],
        );
        let failure = runner(temporary.path(), Duration::from_secs(2))
            .run(&contract, 31)
            .err()
            .expect("fake failure must be reported");
        assert_eq!(
            failure.classification,
            FailureClassification::ProcessFailure
        );
        assert_eq!(failure.scenario, "safe_failure");
        assert_eq!(failure.seed, 31);
        assert_eq!(failure.step.as_deref(), Some("failure"));
        let encoded = serde_json::to_string(&failure).unwrap();
        for secret in ["SECRET", "fake-sleep", "fake-exit", "fake-invalid"] {
            assert!(!encoded.contains(secret));
        }
    }
}

#[test]
fn observer_failures_use_the_safe_observer_classification() {
    let temporary = tempdir().unwrap();
    let mut contract = scenario(
        "observer_failure",
        AgentVendor::Cursor,
        vec![cli("safe", &[], &[string("fake-echo")])],
        vec![],
        vec![],
    );
    contract.actions.last_mut().unwrap().action = ActionKind::Observe {
        source: ObservationSource::Runtime,
        selector: object([("entity", string("git_event"))]),
    };
    let failure = runner(temporary.path(), Duration::from_secs(2))
        .run(&contract, 32)
        .err()
        .expect("wrong-source observer selector must fail");
    assert_eq!(
        failure.classification,
        FailureClassification::ObserverFailure
    );
    assert_eq!(failure.step.as_deref(), Some("observe"));
}

#[test]
fn invalid_contract_and_fault_capture_fail_before_any_sandbox_or_spawn() {
    let parent = tempdir().unwrap();
    let invalid = runner(parent.path(), Duration::from_secs(1))
        .run_bytes(b"{}", 41)
        .err()
        .expect("invalid contract must fail");
    assert_eq!(
        invalid.classification,
        FailureClassification::InvalidScenario
    );
    assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 0);

    let conflicting_faults = scenario(
        "invalid_fault_combination",
        AgentVendor::Cursor,
        vec![
            cli("alpha", &[], &[string("fake-echo")]),
            cli("beta", &[], &[string("fake-echo")]),
        ],
        vec![],
        vec![
            fault(
                "reorder-alpha-beta",
                FaultKind::Reorder {
                    first: identifier("alpha"),
                    second: identifier("beta"),
                },
            ),
            fault(
                "concurrent-alpha-beta",
                FaultKind::Concurrent {
                    targets: vec![identifier("alpha"), identifier("beta")],
                },
            ),
        ],
    );
    let failure = runner(parent.path(), Duration::from_secs(1))
        .run(&conflicting_faults, 44)
        .err()
        .expect("conflicting fault plans must fail preflight");
    assert_eq!(
        failure.classification,
        FailureClassification::InvalidScenario
    );
    assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 0);

    let contract = scenario(
        "invalid_drop_capture",
        AgentVendor::Cursor,
        vec![cli("source", &[], &[string("fake-echo"), string("safe")])],
        vec![capture("captured", VariableKind::Text, "source", "/value")],
        vec![fault(
            "drop-source",
            FaultKind::Drop {
                target: identifier("source"),
            },
        )],
    );
    let failure = runner(parent.path(), Duration::from_secs(1))
        .run(&contract, 42)
        .err()
        .expect("faulted capture source must fail preflight");
    assert_eq!(
        failure.classification,
        FailureClassification::InvalidScenario
    );
    assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 0);
}

#[test]
fn model_executable_names_are_rejected_without_starting_them() {
    let parent = tempdir().unwrap();
    let contract = scenario(
        "model_sentinel",
        AgentVendor::Codex,
        vec![cli("safe", &[], &[string("fake-echo")])],
        vec![],
        vec![],
    );
    let failure = ScenarioRunner::new(
        RunnerConfig::new(parent.path().join("codex"), "/usr/bin/git")
            .with_sandbox_parent(parent.path()),
    )
    .run(&contract, 43)
    .err()
    .expect("model executable must be rejected by policy");
    assert_eq!(
        failure.classification,
        FailureClassification::PolicyViolation
    );
    assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 0);
}
