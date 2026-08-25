use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use sctx_domain::WorkingIntentSnapshot;
use sctx_mcp::{
    ExpectedRevisionId, TaskBoundary, TaskIntentUpdateInput, task_intent_update_at_root,
};
use sctx_scenario_contract::{
    ActionExpectation, ActionKind, ActorId, ActorKind, AgentFraming, AgentProfile, AgentVendor,
    AgentVersion, AssertionId, CaptureSource, EventClassification, EventSupport,
    ExpectedFailureCode, ExpectedFailureKind, InvariantAssertion, InvariantKind, JsonPointer,
    ObservationSource, ProductAction, ResourceId, SandboxBuiltin, SandboxFile, SandboxResource,
    ScenarioAction, ScenarioActor, ScenarioContractVersion, ScenarioDefinition, ScenarioSchema,
    ScenarioVariable, StepId, TemplateValue, VariableKind, VariableName, WireName,
};
use sctx_scenario_runner::state_fingerprint;
use sctx_scenario_runner::{ReadOnlyObserver, RunnerConfig, ScenarioRunner, StepStatus};
use tempfile::tempdir;

fn string(value: &str) -> TemplateValue {
    TemplateValue::String {
        value: value.to_owned(),
    }
}

fn object(fields: impl IntoIterator<Item = (&'static str, TemplateValue)>) -> TemplateValue {
    TemplateValue::Object {
        fields: fields
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect::<BTreeMap<_, _>>(),
    }
}

fn variable(name: &str) -> TemplateValue {
    TemplateValue::Variable {
        name: VariableName::new(name).unwrap(),
    }
}

fn builtin(value: SandboxBuiltin) -> TemplateValue {
    TemplateValue::Builtin { builtin: value }
}

fn step(value: &str) -> StepId {
    StepId::new(value).unwrap()
}

fn capture(name: &str, kind: VariableKind, source: &str, pointer: &str) -> ScenarioVariable {
    ScenarioVariable {
        name: VariableName::new(name).unwrap(),
        value_type: kind,
        capture: CaptureSource {
            step: step(source),
            pointer: JsonPointer::new(pointer).unwrap(),
        },
    }
}

#[allow(clippy::too_many_lines)]
fn real_protocol_contract() -> ScenarioDefinition {
    let resource = ResourceId::new("activation-repo").unwrap();
    let resource_root = || {
        builtin(SandboxBuiltin::ResourceRoot {
            resource: resource.clone(),
        })
    };
    ScenarioDefinition {
        schema: ScenarioSchema::DynamicScenario,
        version: ScenarioContractVersion::V1,
        name: WireName::new("real_sctx_minimal_protocol").unwrap(),
        agent: AgentProfile {
            vendor: AgentVendor::Codex,
            version: AgentVersion::new("0.147.0").unwrap(),
            framing: AgentFraming::ContentLength,
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
        actions: vec![
            ScenarioAction {
                id: step("repository-add"),
                actor: ActorId::new("task").unwrap(),
                after: vec![],
                expectation: ActionExpectation::default(),
                action: ActionKind::CliJson {
                    arguments: vec![
                        string("repository"),
                        string("add"),
                        string("--repository-id"),
                        string("FE"),
                        string("--path"),
                        resource_root(),
                    ],
                },
            },
            ScenarioAction {
                id: step("session-start"),
                actor: ActorId::new("session").unwrap(),
                after: vec![step("repository-add")],
                expectation: ActionExpectation::default(),
                action: ActionKind::HookEvent {
                    event: WireName::new("SessionStart").unwrap(),
                    payload: object([("cwd", resource_root()), ("source", string("startup"))]),
                },
            },
            ScenarioAction {
                id: step("intent-update"),
                actor: ActorId::new("task").unwrap(),
                after: vec![step("session-start")],
                expectation: ActionExpectation::default(),
                action: ActionKind::McpRequest {
                    method: WireName::new("task_intent_update").unwrap(),
                    params: object([
                        ("agent_kind", string("codex")),
                        (
                            "external_session_id",
                            TemplateValue::Builtin {
                                builtin: sctx_scenario_contract::SandboxBuiltin::ActorSessionKey {
                                    actor: ActorId::new("session").unwrap(),
                                },
                            },
                        ),
                        ("task_boundary", string("new")),
                        ("expected_revision_id", TemplateValue::Null),
                        (
                            "intent",
                            object([("goal", string("exercise the isolated runner protocol"))]),
                        ),
                    ]),
                },
            },
            ScenarioAction {
                id: step("cli-list"),
                actor: ActorId::new("task").unwrap(),
                after: vec![step("intent-update")],
                expectation: ActionExpectation::default(),
                action: ActionKind::CliJson {
                    arguments: vec![string("space"), string("list")],
                },
            },
            ScenarioAction {
                id: step("runtime-observe"),
                actor: ActorId::new("task").unwrap(),
                after: vec![step("cli-list")],
                expectation: ActionExpectation::default(),
                action: ActionKind::Observe {
                    source: ObservationSource::Runtime,
                    selector: object([
                        ("entity", string("active_task")),
                        (
                            "session_key",
                            TemplateValue::Builtin {
                                builtin: sctx_scenario_contract::SandboxBuiltin::ActorSessionKey {
                                    actor: ActorId::new("session").unwrap(),
                                },
                            },
                        ),
                    ]),
                },
            },
        ],
        resources: vec![SandboxResource {
            id: resource,
            synthetic_git: true,
            files: vec![SandboxFile {
                path: sctx_scenario_contract::ResourcePath::new("README.md").unwrap(),
                content: "activation fixture\n".to_owned(),
            }],
        }],
        variables: vec![
            capture("task-id", VariableKind::TaskId, "intent-update", "/task_id"),
            capture(
                "intent-revision-id",
                VariableKind::IntentRevisionId,
                "intent-update",
                "/intent_revision_id",
            ),
            capture(
                "task-session-count",
                VariableKind::Count,
                "runtime-observe",
                "/count",
            ),
        ],
        faults: vec![],
        assertions: vec![InvariantAssertion {
            id: AssertionId::new("active-task-is-dynamic").unwrap(),
            invariant: InvariantKind::ActiveTaskPerSession {
                observation: step("runtime-observe"),
                session: ActorId::new("session").unwrap(),
                task: VariableName::new("task-id").unwrap(),
            },
        }],
        events: vec![EventClassification {
            event: WireName::new("SessionStart").unwrap(),
            classification: EventSupport::Supported,
            product_action: Some(ProductAction::SessionStart),
        }],
    }
}

#[test]
fn real_sctx_runs_hook_content_length_mcp_cli_and_readonly_observer() {
    let sandbox_parent = tempdir().unwrap();
    let runner = ScenarioRunner::new(
        RunnerConfig::new(PathBuf::from(env!("CARGO_BIN_EXE_sctx")), "/usr/bin/git")
            .with_sandbox_parent(sandbox_parent.path())
            .with_step_timeout(Duration::from_secs(10)),
    );
    let run = runner.run(&real_protocol_contract(), 174).unwrap();

    assert!(
        run.outcome.variables["task-id"]
            .value
            .as_str()
            .is_some_and(|value| value.starts_with("tsk_"))
    );
    assert!(
        run.outcome.variables["intent-revision-id"]
            .value
            .as_str()
            .is_some_and(|value| value.starts_with("tir_"))
    );
    assert_eq!(run.outcome.variables["task-session-count"].value, 1);
    assert!(run.root().join("state/runtime.sqlite").is_file());
    assert!(run.root().join("repository/.git").is_dir());
    assert!(
        run.outcome
            .steps
            .iter()
            .all(|record| record.status == StepStatus::Completed)
    );
    assert!(
        run.outcome
            .assertions
            .iter()
            .all(|assertion| assertion.passed)
    );
}

#[allow(clippy::too_many_lines)]
fn stale_contract() -> ScenarioDefinition {
    let resource = ResourceId::new("activation-repo").unwrap();
    let session_builtin = || {
        builtin(SandboxBuiltin::ActorSessionKey {
            actor: ActorId::new("session").unwrap(),
        })
    };
    let intent = |goal: &str| object([("goal", string(goal))]);
    let update = |id: &str,
                  after: Vec<StepId>,
                  boundary: &str,
                  expected: TemplateValue,
                  goal: &str,
                  expectation: ActionExpectation|
     -> ScenarioAction {
        ScenarioAction {
            id: step(id),
            actor: ActorId::new("task").unwrap(),
            after,
            expectation,
            action: ActionKind::McpRequest {
                method: WireName::new("task_intent_update").unwrap(),
                params: object([
                    ("agent_kind", string("codex")),
                    ("external_session_id", session_builtin()),
                    ("task_boundary", string(boundary)),
                    ("expected_revision_id", expected),
                    ("intent", intent(goal)),
                ]),
            },
        }
    };
    let observe = |id: &str, after: &str| ScenarioAction {
        id: step(id),
        actor: ActorId::new("task").unwrap(),
        after: vec![step(after)],
        expectation: ActionExpectation::default(),
        action: ActionKind::Observe {
            source: ObservationSource::Runtime,
            selector: object([
                ("entity", string("task_semantic_state")),
                ("session_key", session_builtin()),
            ]),
        },
    };
    ScenarioDefinition {
        schema: ScenarioSchema::DynamicScenario,
        version: ScenarioContractVersion::V1,
        name: WireName::new("real_stale_zero_write").unwrap(),
        agent: AgentProfile {
            vendor: AgentVendor::Codex,
            version: AgentVersion::new("0.147.0").unwrap(),
            framing: AgentFraming::ContentLength,
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
        actions: vec![
            ScenarioAction {
                id: step("repository-add"),
                actor: ActorId::new("task").unwrap(),
                after: vec![],
                expectation: ActionExpectation::default(),
                action: ActionKind::CliJson {
                    arguments: vec![
                        string("repository"),
                        string("add"),
                        string("--repository-id"),
                        string("FE"),
                        string("--path"),
                        builtin(SandboxBuiltin::ResourceRoot {
                            resource: resource.clone(),
                        }),
                    ],
                },
            },
            ScenarioAction {
                id: step("session-start"),
                actor: ActorId::new("session").unwrap(),
                after: vec![step("repository-add")],
                expectation: ActionExpectation::default(),
                action: ActionKind::HookEvent {
                    event: WireName::new("SessionStart").unwrap(),
                    payload: object([
                        (
                            "cwd",
                            builtin(SandboxBuiltin::ResourceRoot {
                                resource: resource.clone(),
                            }),
                        ),
                        ("source", string("startup")),
                    ]),
                },
            },
            update(
                "intent-one",
                vec![step("session-start")],
                "new",
                TemplateValue::Null,
                "establish the first synthetic direction",
                ActionExpectation::default(),
            ),
            update(
                "intent-two",
                vec![step("intent-one")],
                "continue",
                variable("revision-one"),
                "move to the second synthetic direction",
                ActionExpectation::default(),
            ),
            observe("before-stale", "intent-two"),
            update(
                "stale-attempt",
                vec![step("before-stale")],
                "continue",
                variable("revision-one"),
                "attempt a stale third synthetic direction",
                ActionExpectation::TypedFailure {
                    code: ExpectedFailureCode::new("intent_stale").unwrap(),
                    kind: ExpectedFailureKind::StaleState,
                },
            ),
            observe("after-stale", "stale-attempt"),
        ],
        resources: vec![SandboxResource {
            id: resource,
            synthetic_git: true,
            files: vec![SandboxFile {
                path: sctx_scenario_contract::ResourcePath::new("README.md").unwrap(),
                content: "activation fixture\n".to_owned(),
            }],
        }],
        variables: vec![
            capture("task-id", VariableKind::TaskId, "intent-one", "/task_id"),
            capture(
                "revision-one",
                VariableKind::IntentRevisionId,
                "intent-one",
                "/intent_revision_id",
            ),
            capture(
                "revision-two",
                VariableKind::IntentRevisionId,
                "intent-two",
                "/intent_revision_id",
            ),
        ],
        faults: vec![],
        assertions: vec![InvariantAssertion {
            id: AssertionId::new("stale-zero-write").unwrap(),
            invariant: InvariantKind::StaleCasZeroWrites {
                attempt: step("stale-attempt"),
                before_observation: step("before-stale"),
                after_observation: step("after-stale"),
            },
        }],
        events: vec![EventClassification {
            event: WireName::new("SessionStart").unwrap(),
            classification: EventSupport::Supported,
            product_action: Some(ProductAction::SessionStart),
        }],
    }
}

#[test]
fn real_stale_failure_continues_to_readonly_observer_with_zero_state_change() {
    let sandbox_parent = tempdir().unwrap();
    let runner = ScenarioRunner::new(
        RunnerConfig::new(PathBuf::from(env!("CARGO_BIN_EXE_sctx")), "/usr/bin/git")
            .with_sandbox_parent(sandbox_parent.path())
            .with_step_timeout(Duration::from_secs(10)),
    );
    let run = runner.run(&stale_contract(), 177).unwrap();
    assert_ne!(
        run.outcome.variables["revision-one"].value,
        run.outcome.variables["revision-two"].value
    );
    assert!(run.outcome.steps.iter().any(|record| {
        record.step == "stale-attempt" && record.status == StepStatus::ExpectedFailure
    }));
    assert_eq!(
        run.outcome.assertions,
        [sctx_scenario_runner::AssertionRecord {
            id: "stale-zero-write".to_owned(),
            passed: true,
            diagnostic_code: "infrastructure_bytes_changed".to_owned(),
        }]
    );
}

#[test]
fn direct_product_stale_cas_keeps_semantics_despite_infrastructure_bytes() {
    let temporary = tempdir().unwrap();
    let root = temporary.path().join("root");
    let input = |boundary, expected_revision_id, goal: &str| TaskIntentUpdateInput {
        agent_kind: "codex".to_owned(),
        external_session_id: "direct-stale-session".to_owned(),
        task_boundary: boundary,
        expected_revision_id,
        intent: WorkingIntentSnapshot {
            goal: goal.to_owned(),
            current_direction: None,
            in_scope: vec![],
            out_of_scope: vec![],
            domains: vec![],
            platforms: vec![],
            constraints: vec![],
            acceptance_conditions: vec![],
            artifact_hints: vec![],
            interface_hints: vec![],
            open_questions: vec![],
        },
    };
    let first = task_intent_update_at_root(
        &root,
        &input(
            TaskBoundary::New,
            ExpectedRevisionId::Null(()),
            "first direct direction",
        ),
    )
    .unwrap();
    let second = task_intent_update_at_root(
        &root,
        &input(
            TaskBoundary::Continue,
            ExpectedRevisionId::Revision(first.context.intent_revision_id.to_string()),
            "second direct direction",
        ),
    )
    .unwrap();
    let observer = ReadOnlyObserver::new("/usr/bin/git");
    let before_observation = observer
        .observe(
            &root,
            ObservationSource::Runtime,
            &serde_json::json!({
                "entity": "task_semantic_state",
                "session_key": "direct-stale-session"
            }),
        )
        .unwrap();
    let before = state_fingerprint(&root).unwrap();
    task_intent_update_at_root(
        &root,
        &input(
            TaskBoundary::Continue,
            ExpectedRevisionId::Revision(first.context.intent_revision_id.to_string()),
            "stale direct direction",
        ),
    )
    .unwrap_err();
    let after = state_fingerprint(&root).unwrap();
    let after_observation = observer
        .observe(
            &root,
            ObservationSource::Runtime,
            &serde_json::json!({
                "entity": "task_semantic_state",
                "session_key": "direct-stale-session"
            }),
        )
        .unwrap();
    assert_ne!(
        first.context.intent_revision_id,
        second.context.intent_revision_id
    );
    assert_eq!(before_observation.summary, after_observation.summary);
    assert_ne!(
        before, after,
        "the accepted boundary records infrastructure bytes"
    );
}
