use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use sctx_scenario_contract::{
    ActionExpectation, ActionKind, ActorId, ActorKind, AgentFraming, AgentProfile, AgentVendor,
    AgentVersion, AssertionId, CaptureSource, ConfirmationEventCount, EventClassification,
    EventSupport, InvariantAssertion, InvariantKind, JsonPointer, ProductAction, ResourceId,
    ResourcePath, SandboxBuiltin, SandboxFile, SandboxResource, ScenarioAction, ScenarioActor,
    ScenarioContractVersion, ScenarioDefinition, ScenarioSchema, ScenarioVariable, StepId,
    TemplateValue, VariableKind, VariableName, WireName, to_canonical_json,
};
use sctx_scenario_runner::{RunnerConfig, ScenarioRunner};
use tempfile::tempdir;

fn text(value: &str) -> TemplateValue {
    TemplateValue::String {
        value: value.to_owned(),
    }
}

fn count(value: u64) -> TemplateValue {
    TemplateValue::Unsigned { value }
}

fn array(items: impl IntoIterator<Item = TemplateValue>) -> TemplateValue {
    TemplateValue::Array {
        items: items.into_iter().collect(),
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
fn contract() -> ScenarioDefinition {
    let resource = ResourceId::new("focus-repo").unwrap();
    let relative = ResourcePath::new("src/focus.rs").unwrap();
    let session = || {
        builtin(SandboxBuiltin::ActorSessionKey {
            actor: ActorId::new("session").unwrap(),
        })
    };
    let resource_root = || {
        builtin(SandboxBuiltin::ResourceRoot {
            resource: resource.clone(),
        })
    };
    let resource_file = || {
        builtin(SandboxBuiltin::ResourceFile {
            resource: resource.clone(),
            path: relative.clone(),
        })
    };
    let cli = |id: &str, after: &str, arguments: Vec<TemplateValue>| ScenarioAction {
        id: step(id),
        actor: ActorId::new("task").unwrap(),
        after: if after.is_empty() {
            vec![]
        } else {
            vec![step(after)]
        },
        expectation: ActionExpectation::default(),
        action: ActionKind::CliJson { arguments },
    };
    let mcp = |id: &str, after: &str, method: &str, params: TemplateValue| ScenarioAction {
        id: step(id),
        actor: ActorId::new("task").unwrap(),
        after: vec![step(after)],
        expectation: ActionExpectation::default(),
        action: ActionKind::McpRequest {
            method: WireName::new(method).unwrap(),
            params,
        },
    };
    ScenarioDefinition {
        schema: ScenarioSchema::DynamicScenario,
        version: ScenarioContractVersion::V1,
        name: WireName::new("real_resource_catalog_focus").unwrap(),
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
            cli(
                "repository-add",
                "",
                vec![
                    text("repository"),
                    text("add"),
                    text("--path"),
                    resource_root(),
                ],
            ),
            ScenarioAction {
                id: step("session-start"),
                actor: ActorId::new("session").unwrap(),
                after: vec![step("repository-add")],
                expectation: ActionExpectation::default(),
                action: ActionKind::HookEvent {
                    event: WireName::new("SessionStart").unwrap(),
                    payload: object([("cwd", resource_root()), ("source", text("startup"))]),
                },
            },
            mcp(
                "intent-update",
                "session-start",
                "task_intent_update",
                object([
                    ("agent_kind", text("codex")),
                    ("external_session_id", session()),
                    ("task_boundary", text("new")),
                    ("expected_revision_id", TemplateValue::Null),
                    (
                        "intent",
                        object([(
                            "goal",
                            text("establish a synthetic resource focus validation"),
                        )]),
                    ),
                ]),
            ),
            mcp(
                "checkpoint-close",
                "intent-update",
                "task_checkpoint",
                object([
                    ("agent_kind", text("codex")),
                    ("external_session_id", session()),
                    ("expected_task_id", variable("task-id")),
                    (
                        "expected_intent_revision_id",
                        variable("intent-revision-id"),
                    ),
                    ("expected_episode_version", count(0)),
                    ("boundary", text("close")),
                    (
                        "claims",
                        array([object([
                            ("context_kind_hint", text("validation")),
                            ("topic_key_hint", text("testing/resource-focus")),
                            (
                                "statement",
                                text("The synthetic focus resource resolves through the graph"),
                            ),
                            (
                                "rationale",
                                text("A fixed offline validation exercises the path"),
                            ),
                            (
                                "applicability",
                                object([
                                    ("domains", array([text("testing")])),
                                    ("platforms", array([])),
                                    ("conditions", array([])),
                                ]),
                            ),
                            ("assumptions", array([])),
                            (
                                "recheck_when",
                                array([text("the scenario protocol changes")]),
                            ),
                            (
                                "evidence",
                                array([object([
                                    ("kind", text("inline_validation")),
                                    (
                                        "evidence",
                                        object([
                                            ("kind", text("experiment_record")),
                                            ("supports", text("the synthetic resource exists")),
                                            ("content", object([("actual", text("passed"))])),
                                            (
                                                "interpretation",
                                                text("the local test controls the resource"),
                                            ),
                                            (
                                                "limitations",
                                                array([text("synthetic offline fixture")]),
                                            ),
                                        ]),
                                    ),
                                ])]),
                            ),
                            ("artifact_refs", array([])),
                            ("related_contexts", array([])),
                        ])]),
                    ),
                    ("unknowns", array([])),
                ]),
            ),
            mcp(
                "candidate-get",
                "checkpoint-close",
                "candidate_get",
                object([
                    ("agent_kind", text("codex")),
                    ("external_session_id", session()),
                    ("candidate_id", variable("built-candidate-id")),
                ]),
            ),
            mcp(
                "candidate-confirm",
                "candidate-get",
                "candidate_confirm",
                object([
                    ("agent_kind", text("codex")),
                    ("external_session_id", session()),
                    ("expected_task_id", variable("task-id")),
                    (
                        "expected_intent_revision_id",
                        variable("intent-revision-id"),
                    ),
                    ("candidate_id", variable("candidate-id")),
                    ("expected_review_version", variable("review-version")),
                    (
                        "primary",
                        object([("new_space_recommendation_id", variable("recommendation-id"))]),
                    ),
                    ("related_space_ids", array([])),
                ]),
            ),
            mcp(
                "repository-scan",
                "candidate-confirm",
                "repository_scan",
                object([
                    ("agent_kind", text("codex")),
                    ("external_session_id", session()),
                    ("checkout_path", resource_root()),
                    ("paths", array([text("src/focus.rs")])),
                    ("max_artifacts", count(20)),
                ]),
            ),
            mcp(
                "reference-record",
                "repository-scan",
                "engineering_reference_record",
                object([
                    ("agent_kind", text("codex")),
                    ("external_session_id", session()),
                    ("context_id", variable("context-id")),
                    ("revision_id", variable("context-revision-id")),
                    ("repository_id", variable("repository-id")),
                    ("artifact_kind", text("file")),
                    ("relation", text("implements")),
                    (
                        "locator",
                        object([
                            ("locator_kind", text("file")),
                            ("path", text("src/focus.rs")),
                        ]),
                    ),
                    (
                        "supports",
                        text("the synthetic file implements the validation"),
                    ),
                    ("limitations", array([text("synthetic repository")])),
                ]),
            ),
            mcp(
                "association-rebuild",
                "reference-record",
                "association_rebuild",
                object([
                    ("agent_kind", text("codex")),
                    ("external_session_id", session()),
                    ("diagnose_only", TemplateValue::Boolean { value: false }),
                ]),
            ),
            mcp(
                "artifact-focus",
                "association-rebuild",
                "task_artifact_focus",
                object([
                    ("agent_kind", text("codex")),
                    ("external_session_id", session()),
                    ("expected_revision_id", variable("intent-revision-id")),
                    ("absolute_file_path", resource_file()),
                    ("locator", object([("locator_kind", text("file"))])),
                    ("token_budget", count(4_000)),
                    ("max_spaces", count(8)),
                ]),
            ),
            mcp(
                "ordinary-context",
                "artifact-focus",
                "task_context",
                object([
                    ("agent_kind", text("codex")),
                    ("external_session_id", session()),
                    ("token_budget", count(4_000)),
                    ("max_spaces", count(8)),
                ]),
            ),
        ],
        resources: vec![SandboxResource {
            id: resource.clone(),
            synthetic_git: true,
            files: vec![SandboxFile {
                path: relative.clone(),
                content: "pub fn synthetic_focus_target() -> bool { true }\n".to_owned(),
            }],
        }],
        variables: vec![
            capture(
                "repository-id",
                VariableKind::RepositoryId,
                "repository-add",
                "/catalog/repository/repository_id",
            ),
            capture("task-id", VariableKind::TaskId, "intent-update", "/task_id"),
            capture(
                "intent-revision-id",
                VariableKind::IntentRevisionId,
                "intent-update",
                "/intent_revision_id",
            ),
            capture(
                "built-candidate-id",
                VariableKind::CandidateId,
                "checkpoint-close",
                "/candidate_build/items/0/candidate_id",
            ),
            capture(
                "candidate-id",
                VariableKind::CandidateId,
                "candidate-get",
                "/candidate_id",
            ),
            capture(
                "episode-id",
                VariableKind::WorkEpisodeId,
                "candidate-get",
                "/source_episode/episode_id",
            ),
            capture(
                "review-version",
                VariableKind::Count,
                "candidate-get",
                "/review_version",
            ),
            capture(
                "recommendation-id",
                VariableKind::SpaceRecommendationId,
                "candidate-get",
                "/space_recommendations/0/recommendation_id",
            ),
            capture(
                "confirmation-id",
                VariableKind::ConfirmationId,
                "candidate-confirm",
                "/confirmation_id",
            ),
            capture(
                "context-id",
                VariableKind::ContextId,
                "candidate-confirm",
                "/context_id",
            ),
            capture(
                "context-revision-id",
                VariableKind::RevisionId,
                "candidate-confirm",
                "/revision_id",
            ),
            capture(
                "scan-repository-id",
                VariableKind::RepositoryId,
                "repository-scan",
                "/repository_id",
            ),
            capture(
                "focus-repository-id",
                VariableKind::RepositoryId,
                "artifact-focus",
                "/resolved_focus/repository_id",
            ),
        ],
        faults: vec![],
        assertions: vec![
            InvariantAssertion {
                id: AssertionId::new("candidate-source").unwrap(),
                invariant: InvariantKind::CandidateSourceEpisode {
                    response: step("candidate-get"),
                    candidate: VariableName::new("candidate-id").unwrap(),
                    episode: VariableName::new("episode-id").unwrap(),
                },
            },
            InvariantAssertion {
                id: AssertionId::new("confirmation-atomic").unwrap(),
                invariant: InvariantKind::ConfirmationIsAtomic {
                    confirmation: VariableName::new("confirmation-id").unwrap(),
                    response: step("candidate-confirm"),
                    event_count: ConfirmationEventCount::Five,
                },
            },
            InvariantAssertion {
                id: AssertionId::new("focus-request-scoped").unwrap(),
                invariant: InvariantKind::ArtifactFocusIsRequestScoped {
                    focused_response: step("artifact-focus"),
                    ordinary_response: step("ordinary-context"),
                    resource,
                    path: relative,
                },
            },
        ],
        events: vec![EventClassification {
            event: WireName::new("SessionStart").unwrap(),
            classification: EventSupport::Supported,
            product_action: Some(ProductAction::SessionStart),
        }],
    }
}

#[test]
fn real_resource_flows_through_catalog_scan_reference_graph_and_focus() {
    let scenario = contract();
    let canonical = to_canonical_json(&scenario).unwrap();
    let canonical_text = std::str::from_utf8(&canonical).unwrap();
    assert!(!canonical_text.contains("/Users/"));
    for prefix in ["tsk_", "tir_", "ctx_", "rev_", "rpo_", "cnd_", "cfm_"] {
        assert!(!canonical_text.contains(prefix));
    }

    let sandbox_parent = tempdir().unwrap();
    let runner = ScenarioRunner::new(
        RunnerConfig::new(PathBuf::from(env!("CARGO_BIN_EXE_sctx")), "/usr/bin/git")
            .with_sandbox_parent(sandbox_parent.path())
            .with_step_timeout(Duration::from_secs(20)),
    );
    let run = runner.run(&scenario, 178).unwrap();
    assert_eq!(
        run.outcome.variables["repository-id"].value,
        run.outcome.variables["scan-repository-id"].value
    );
    assert_eq!(
        run.outcome.variables["repository-id"].value,
        run.outcome.variables["focus-repository-id"].value
    );
    assert_eq!(
        run.outcome.variables["built-candidate-id"].value,
        run.outcome.variables["candidate-id"].value
    );
    assert!(
        run.outcome
            .assertions
            .iter()
            .all(|assertion| assertion.passed)
    );
    let absolute_file = run.workspace().join("resources/focus-repo/src/focus.rs");
    assert!(absolute_file.is_absolute() && absolute_file.is_file());
    let outcome = serde_json::to_string(&run.outcome).unwrap();
    assert!(!outcome.contains(sandbox_parent.path().to_string_lossy().as_ref()));
}
