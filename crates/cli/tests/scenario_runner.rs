use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use sctx_scenario_contract::{
    ActionKind, ActorId, ActorKind, AgentFraming, AgentProfile, AgentVendor, AgentVersion,
    AssertionId, CaptureSource, EventClassification, EventSupport, InvariantAssertion,
    InvariantKind, JsonPointer, ObservationSource, ProductAction, ScenarioAction, ScenarioActor,
    ScenarioContractVersion, ScenarioDefinition, ScenarioSchema, ScenarioVariable, StepId,
    TemplateValue, VariableKind, VariableName, WireName,
};
use sctx_scenario_runner::{RunnerConfig, ScenarioRunner, StepStatus};
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

fn real_protocol_contract() -> ScenarioDefinition {
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
                id: step("session-start"),
                actor: ActorId::new("session").unwrap(),
                after: vec![],
                action: ActionKind::HookEvent {
                    event: WireName::new("SessionStart").unwrap(),
                    payload: object([("source", string("startup"))]),
                },
            },
            ScenarioAction {
                id: step("intent-update"),
                actor: ActorId::new("task").unwrap(),
                after: vec![step("session-start")],
                action: ActionKind::McpRequest {
                    method: WireName::new("task_intent_update").unwrap(),
                    params: object([
                        ("agent_kind", string("codex")),
                        ("external_session_id", string("synthetic-runner-session")),
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
                action: ActionKind::CliJson {
                    arguments: vec![string("space"), string("list")],
                },
            },
            ScenarioAction {
                id: step("runtime-observe"),
                actor: ActorId::new("task").unwrap(),
                after: vec![step("cli-list")],
                action: ActionKind::Observe {
                    source: ObservationSource::Runtime,
                    selector: object([("entity", string("task_session"))]),
                },
            },
        ],
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
}
