use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::Arc,
};

use sctx_domain::{
    Applicability, ConflictParticipant, ContextId, ContextKind, ContextRevisionDraft,
    EvidenceSnapshotDraft, EvidenceType, ExternalSessionLocator, IntentSnapshot, PublicationAction,
    PublicationDraft, SemanticConflictDraft, SpaceId, SubmissionId, TaskId, TaskSessionId,
    TaskSignal, TaskSignalKind, WorkEpisodeId, WorkEpisodeRef, WorkingIntentSnapshot,
};
use sctx_event_schema::{Event, EventPayload};
use sctx_git_store::{AppendRequest, CandidateSubmissionRequest, GitStore};
use sctx_index::ProjectionIndex;
use sctx_local_state::UserConfigStore;
use sctx_mcp::{
    ExpectedRevisionId, TaskBoundary, TaskContextReadInput, TaskContextResponse,
    TaskIntentUpdateInput, task_context_readonly_at_root, task_intent_update_at_root,
};
use sctx_search::{
    ContextPackMode, ContextStatus, SearchEngine, TaskContextRequest, TaskRetrievalPath,
};
use sctx_task_runtime::TaskRuntime;
use serde_json::Value;
use tempfile::TempDir;

struct MilestoneTwoFixture {
    _temporary: TempDir,
    home: PathBuf,
    root: PathBuf,
    workspace: PathBuf,
    store: GitStore,
    index: ProjectionIndex,
    feature_spaces: [SpaceId; 4],
    feature_contexts: [ContextId; 4],
    unsafe_spaces: [SpaceId; 4],
    unassigned_candidate_id: String,
}

#[derive(Clone)]
struct TaskScenario {
    agent_kind: String,
    external_session_id: String,
    intent: WorkingIntentSnapshot,
    task_signals: Vec<TaskSignal>,
    max_spaces: usize,
}

impl MilestoneTwoFixture {
    #[allow(clippy::too_many_lines)]
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("用户 home");
        let root = home.join(".shared-context");
        let workspace = home.join("same workspace");
        fs::create_dir_all(workspace.join("src")).unwrap();
        fs::write(
            workspace.join("src/search_results_page.tsx"),
            "export const SearchResultsPage = () => null;\n",
        )
        .unwrap();
        let status = Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(&workspace)
            .status()
            .unwrap();
        assert!(status.success());
        let workspace = fs::canonicalize(workspace).unwrap();

        let store = GitStore::initialize(&root).unwrap();
        UserConfigStore::initialize(&root)
            .unwrap()
            .add_repository(None, std::slice::from_ref(&workspace))
            .unwrap();
        let page_space = add_space(
            &store,
            "Quartz Page Requirement",
            "quartzpageintent src/search_results_page.tsx",
        );
        let page_context = add_accepted_context(
            &store,
            page_space,
            "quartzpagecontext keeps navigation in src/search_results_page.tsx",
            applicability("quartzpage", "fe", "active"),
        );
        let server_space = add_space(&store, "Cobalt Server Protocol", "cobaltserverintent");
        let server_context = add_accepted_context(
            &store,
            server_space,
            "SearchV2Endpoint returns SearchResponseV2",
            applicability("cobaltserver", "server", "active"),
        );
        let compatibility_space =
            add_space(&store, "Amber Compatibility", "ambercompatibilityintent");
        let compatibility_context = add_accepted_context(
            &store,
            compatibility_space,
            "LegacyCompatibilityTest succeeded for old client behavior",
            applicability("ambercompatibility", "fe", "legacyclientconstraint"),
        );
        let analytics_space = add_space(&store, "Violet Analytics", "violetanalyticsintent");
        let analytics_context = add_accepted_context(
            &store,
            analytics_space,
            "impressionacceptance semantics follow the analytics contract",
            applicability("analyticsdomainconstraint", "server", "production"),
        );

        let candidate_space = add_space(&store, "Hazard Candidate", "hazardpackintent candidate");
        let _candidate_context = add_context(
            &store,
            candidate_space,
            complete_context(
                "hazardpackcontext candidate",
                applicability("unsafe", "fe", "active"),
            ),
        );

        let deprecated_space =
            add_space(&store, "Hazard Deprecated", "hazardpackintent deprecated");
        let (deprecated_context, deprecated_revision) = add_context(
            &store,
            deprecated_space,
            complete_context(
                "hazardpackcontext deprecated",
                applicability("unsafe", "fe", "active"),
            ),
        );
        let deprecated_publication = publish(
            &store,
            deprecated_space,
            deprecated_context,
            deprecated_revision,
            Vec::new(),
            PublicationAction::Publish,
        );
        publish(
            &store,
            deprecated_space,
            deprecated_context,
            deprecated_revision,
            vec![deprecated_publication],
            PublicationAction::Withdraw,
        );

        let conflict_space = add_space(&store, "Hazard Conflict", "hazardpackintent conflict");
        let (conflict_a, revision_a) = add_context(
            &store,
            conflict_space,
            complete_context(
                "hazardpackcontext behavior enabled",
                applicability("unsafe", "fe", "active"),
            ),
        );
        let publication_a = publish(
            &store,
            conflict_space,
            conflict_a,
            revision_a,
            Vec::new(),
            PublicationAction::Publish,
        );
        let (conflict_b, revision_b) = add_context(
            &store,
            conflict_space,
            complete_context(
                "hazardpackcontext behavior disabled",
                applicability("unsafe", "fe", "active"),
            ),
        );
        let publication_b = publish(
            &store,
            conflict_space,
            conflict_b,
            revision_b,
            Vec::new(),
            PublicationAction::Publish,
        );
        append(
            &store,
            Event::semantic_conflict_opened(
                conflict_space,
                SemanticConflictDraft {
                    participants: vec![
                        ConflictParticipant {
                            context_id: conflict_a,
                            revision_id: revision_a,
                            publication_id: publication_a,
                        },
                        ConflictParticipant {
                            context_id: conflict_b,
                            revision_id: revision_b,
                            publication_id: publication_b,
                        },
                    ],
                    reason: "the unsafe fixture has contradictory accepted behavior".to_owned(),
                    applicability: applicability("unsafe", "fe", "active"),
                },
                None,
            )
            .unwrap(),
        );

        let incomplete_space = add_space(
            &store,
            "Hazard Incomplete Evidence",
            "hazardpackintent incomplete",
        );
        let mut incomplete = complete_context(
            "hazardpackcontext incomplete evidence",
            applicability("unsafe", "fe", "active"),
        );
        incomplete.evidence[0].limitations.clear();
        let (incomplete_context, incomplete_revision) =
            add_context(&store, incomplete_space, incomplete);
        publish(
            &store,
            incomplete_space,
            incomplete_context,
            incomplete_revision,
            Vec::new(),
            PublicationAction::Publish,
        );

        let index = ProjectionIndex::for_store(&store);
        let store = store.with_candidate_submission_index(Arc::new(index.clone()));
        let candidate = store
            .submit_candidate(CandidateSubmissionRequest {
                submission_id: SubmissionId::new(),
                source_episode: WorkEpisodeRef {
                    episode_id: WorkEpisodeId::new(),
                    task_session_id: TaskSessionId::new(),
                    task_id: TaskId::new(),
                },
                content: complete_context(
                    "hazardpackintent unassigned Candidate",
                    applicability("unsafe", "fe", "active"),
                ),
            })
            .unwrap();
        let unassigned_candidate_id = candidate.record.candidate_id.to_string();

        index.synchronize().unwrap();
        Self {
            _temporary: temporary,
            home,
            root,
            workspace,
            store,
            index,
            feature_spaces: [
                page_space,
                server_space,
                compatibility_space,
                analytics_space,
            ],
            feature_contexts: [
                page_context,
                server_context,
                compatibility_context,
                analytics_context,
            ],
            unsafe_spaces: [
                candidate_space,
                deprecated_space,
                conflict_space,
                incomplete_space,
            ],
            unassigned_candidate_id,
        }
    }

    fn input(&self, external_session_id: &str, goal: &str) -> TaskScenario {
        TaskScenario {
            agent_kind: "codex".to_owned(),
            external_session_id: external_session_id.to_owned(),
            intent: WorkingIntentSnapshot {
                goal: goal.to_owned(),
                current_direction: None,
                in_scope: Vec::new(),
                out_of_scope: Vec::new(),
                domains: Vec::new(),
                platforms: Vec::new(),
                constraints: Vec::new(),
                acceptance_conditions: Vec::new(),
                artifact_hints: Vec::new(),
                interface_hints: Vec::new(),
                open_questions: Vec::new(),
            },
            task_signals: vec![TaskSignal {
                kind: TaskSignalKind::Workspace,
                content: self.workspace.to_string_lossy().into_owned(),
            }],
            max_spaces: sctx_search::DEFAULT_TASK_MAX_SPACES,
        }
    }

    fn feature_input(&self, external_session_id: &str) -> TaskScenario {
        let mut input = self.input(external_session_id, "quartzpageintent");
        input.intent.current_direction = Some("quartzfeaturechange".to_owned());
        input.intent.domains = vec!["analyticsdomainconstraint".to_owned()];
        input.intent.platforms = vec!["fe".to_owned()];
        input.intent.constraints = vec!["legacyclientconstraint".to_owned()];
        input.intent.acceptance_conditions = vec!["impressionacceptance".to_owned()];
        input.task_signals.extend([
            TaskSignal {
                kind: TaskSignalKind::Diff,
                content: "src/search_results_page.tsx".to_owned(),
            },
            TaskSignal {
                kind: TaskSignalKind::Diff,
                content: "SearchV2Endpoint".to_owned(),
            },
            TaskSignal {
                kind: TaskSignalKind::Diff,
                content: "SearchResponseV2".to_owned(),
            },
            TaskSignal {
                kind: TaskSignalKind::TestOutcome,
                content: "LegacyCompatibilityTest succeeded".to_owned(),
            },
        ]);
        input
    }

    fn hook(&self, input: &Value) -> Value {
        let mut child = Command::new(env!("CARGO_BIN_EXE_sctx"))
            .args(["hook", "--agent", "codex", "--agent-version", "0.147.0"])
            .env("HOME", &self.home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(&serde_json::to_vec(input).unwrap())
            .unwrap();
        drop(child.stdin.take());
        let output = child.wait_with_output().unwrap();
        assert_success(&output);
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

fn applicability(domain: &str, platform: &str, condition: &str) -> Applicability {
    Applicability {
        domains: vec![domain.to_owned()],
        platforms: vec![platform.to_owned()],
        conditions: vec![condition.to_owned()],
    }
}

fn complete_context(statement: &str, applicability: Applicability) -> ContextRevisionDraft {
    ContextRevisionDraft {
        kind: ContextKind::Contract,
        topic_key: Some("milestone-two/acceptance".to_owned()),
        statement: statement.to_owned(),
        rationale: "the retrieval oracle captures durable engineering behavior".to_owned(),
        applicability,
        assumptions: vec!["the synthetic fixture remains stable".to_owned()],
        recheck_when: vec!["the retrieval contract changes".to_owned()],
        relations: Vec::new(),
        evidence: vec![EvidenceSnapshotDraft {
            kind: EvidenceType::ExperimentRecord,
            supports: "the retrieval contract is executable".to_owned(),
            content: serde_json::json!({"gate": "milestone_two", "actual": "passed"}),
            interpretation: "the Context is backed by self-contained Evidence".to_owned(),
            limitations: vec!["synthetic integration fixture".to_owned()],
        }],
    }
}

fn add_space(store: &GitStore, title: &str, intent_text: &str) -> SpaceId {
    let event = Event::space_created(
        IntentSnapshot {
            title: title.to_owned(),
            problem: format!("{intent_text} problem"),
            desired_outcome: format!("{intent_text} outcome"),
            in_scope: vec![intent_text.to_owned()],
            out_of_scope: vec![format!("{title} excluded")],
            acceptance_conditions: vec![format!("{title} accepted")],
            domain_terms: vec![format!("{title} term")],
        },
        None,
    )
    .unwrap();
    let space_id = match event.payload() {
        EventPayload::SpaceCreated { space_id, .. } => *space_id,
        _ => unreachable!(),
    };
    append(store, event);
    space_id
}

fn add_context(
    store: &GitStore,
    space_id: SpaceId,
    context: ContextRevisionDraft,
) -> (ContextId, sctx_domain::RevisionId) {
    let event = Event::context_revision_added(space_id, context, None).unwrap();
    let ids = match event.payload() {
        EventPayload::ContextRevisionAdded {
            context_id,
            revision,
            ..
        } => (*context_id, revision.revision_id),
        _ => unreachable!(),
    };
    append(store, event);
    ids
}

fn add_accepted_context(
    store: &GitStore,
    space_id: SpaceId,
    statement: &str,
    applicability: Applicability,
) -> ContextId {
    let (context_id, revision_id) =
        add_context(store, space_id, complete_context(statement, applicability));
    publish(
        store,
        space_id,
        context_id,
        revision_id,
        Vec::new(),
        PublicationAction::Publish,
    );
    context_id
}

fn publish(
    store: &GitStore,
    space_id: SpaceId,
    context_id: ContextId,
    revision_id: sctx_domain::RevisionId,
    previous_publication_ids: Vec<sctx_domain::PublicationId>,
    action: PublicationAction,
) -> sctx_domain::PublicationId {
    let event = Event::publication_changed(
        space_id,
        context_id,
        PublicationDraft {
            previous_publication_ids,
            action,
            revision_id,
            review_event_ids: Vec::new(),
        },
        None,
    )
    .unwrap();
    let publication_id = match event.payload() {
        EventPayload::ContextPublicationChanged { publication, .. } => publication.publication_id,
        _ => unreachable!(),
    };
    append(store, event);
    publication_id
}

fn append(store: &GitStore, event: Event) {
    store
        .append_event(AppendRequest::event(event))
        .expect("append M2 fixture Event");
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn response_spaces(response: &TaskContextResponse) -> BTreeSet<SpaceId> {
    response
        .candidate_spaces
        .iter()
        .map(|association| association.space_id)
        .collect()
}

fn establish_task(root: &Path, input: &TaskScenario) -> TaskContextResponse {
    let mut update = TaskIntentUpdateInput {
        agent_kind: input.agent_kind.clone(),
        external_session_id: input.external_session_id.clone(),
        task_boundary: TaskBoundary::New,
        expected_revision_id: ExpectedRevisionId::Null(()),
        intent: input.intent.clone(),
    };
    let created = task_intent_update_at_root(root, &update).unwrap().context;
    if input.task_signals.is_empty() {
        return created;
    }
    TaskRuntime::initialize(root)
        .unwrap()
        .merge_signals(created.task_session_id, input.task_signals.clone())
        .unwrap();
    update.task_boundary = TaskBoundary::Continue;
    update.expected_revision_id =
        ExpectedRevisionId::Revision(created.intent_revision_id.to_string());
    task_intent_update_at_root(root, &update).unwrap().context
}

fn assert_typed_m2_path(path: &TaskRetrievalPath) {
    match path {
        TaskRetrievalPath::IntentFts {
            matched_fields,
            matched_tokens,
        } => {
            assert!(!matched_fields.is_empty());
            assert!(!matched_tokens.is_empty());
        }
        TaskRetrievalPath::ContextFts {
            matched_fields,
            matched_tokens,
        } => {
            assert!(!matched_fields.is_empty());
            assert!(!matched_tokens.is_empty());
        }
        TaskRetrievalPath::WorkingIntentHintText { explanation } => {
            assert!(!explanation.matched_tokens.is_empty());
            assert!(explanation.query_token_coverage_basis_points > 0);
            assert!(explanation.fusion_contribution_micros > 0);
        }
        TaskRetrievalPath::ExactScope { dimension, value } => {
            assert!(!dimension.is_empty());
            assert!(!value.is_empty());
        }
        TaskRetrievalPath::EngineeringGraph {
            path,
            relation_hops,
        } => {
            assert!(!path.resolved_focus.locator.canonical_key().is_empty());
            assert!(!path.artifact_generation.is_empty());
            assert!(relation_hops.len() <= 2);
        }
        TaskRetrievalPath::ContextRelation { hops } => {
            assert!(!hops.is_empty());
            assert!(hops.len() <= 2);
        }
        TaskRetrievalPath::GraphDiagnostic { diagnostic } => {
            assert!(!diagnostic.resolved_focus.locator.canonical_key().is_empty());
            assert!(!diagnostic.artifact_generation.is_empty());
        }
    }
}

fn task_intent(goal: &str) -> WorkingIntentSnapshot {
    WorkingIntentSnapshot {
        goal: goal.to_owned(),
        current_direction: None,
        in_scope: Vec::new(),
        out_of_scope: Vec::new(),
        domains: Vec::new(),
        platforms: Vec::new(),
        constraints: Vec::new(),
        acceptance_conditions: Vec::new(),
        artifact_hints: Vec::new(),
        interface_hints: Vec::new(),
        open_questions: Vec::new(),
    }
}

fn git_tree(repository: &Path) -> String {
    let output = Command::new("git")
        .args([
            "-C",
            repository.to_str().unwrap(),
            "rev-parse",
            "HEAD^{tree}",
        ])
        .output()
        .unwrap();
    assert_success(&output);
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[test]
#[allow(clippy::too_many_lines)]
fn task_runtime_retrieval_closes_the_m2_cross_crate_contract() {
    let fixture = MilestoneTwoFixture::new();

    let zero_input = fixture.input("zero-session", "lonelyunrelatedtoken");
    let one_input = fixture.input("page-session", "quartzpageintent");
    let server_input = fixture.input("server-session", "cobaltserverintent");
    let many_input = fixture.feature_input("feature-session");
    let read_input = TaskContextReadInput {
        agent_kind: many_input.agent_kind.clone(),
        external_session_id: many_input.external_session_id.clone(),
        token_budget: 2_000,
        max_spaces: many_input.max_spaces,
    };
    let serialized_input = serde_json::to_value(&read_input).unwrap();
    assert_eq!(serialized_input.as_object().unwrap().len(), 4);
    for route in [
        "space_id",
        "space_ids",
        "workspace",
        "workspace_id",
        "task_id",
        "goal",
        "desired_change",
        "task_signals",
    ] {
        let mut routed = serialized_input.clone();
        routed
            .as_object_mut()
            .unwrap()
            .insert(route.to_owned(), Value::String("forbidden".to_owned()));
        assert!(serde_json::from_value::<TaskContextReadInput>(routed).is_err());
    }

    let zero = establish_task(&fixture.root, &zero_input);
    let page = establish_task(&fixture.root, &one_input);
    let server = establish_task(&fixture.root, &server_input);
    let many = establish_task(&fixture.root, &many_input);
    assert!(zero.candidate_spaces.is_empty());
    assert!(zero.items.is_empty());
    assert_eq!(page.candidate_spaces.len(), 1);
    assert_eq!(page.items.len(), 1);
    assert_eq!(server.candidate_spaces.len(), 1);
    assert_eq!(server.items.len(), 1);
    assert!((2..=4).contains(&many.candidate_spaces.len()));
    assert!((2..=many.candidate_spaces.len()).contains(&many.items.len()));
    assert!(response_spaces(&many).is_subset(&fixture.feature_spaces.into_iter().collect()));
    assert!(
        many.items
            .iter()
            .map(|item| item.context.context_id)
            .collect::<BTreeSet<_>>()
            .is_subset(&fixture.feature_contexts.into_iter().collect())
    );

    assert_ne!(page.task_session_id, server.task_session_id);
    assert_ne!(page.task_id, server.task_id);
    assert_eq!(response_spaces(&page), [fixture.feature_spaces[0]].into());
    assert_eq!(response_spaces(&server), [fixture.feature_spaces[1]].into());
    assert!(page.items.iter().all(|item| {
        item.context.context_id == fixture.feature_contexts[0]
            && item.context.context_id != fixture.feature_contexts[1]
    }));
    assert!(server.items.iter().all(|item| {
        item.context.context_id == fixture.feature_contexts[1]
            && item.context.context_id != fixture.feature_contexts[0]
    }));

    let association_spaces = response_spaces(&many);
    let flattened = many
        .retrieval_paths
        .iter()
        .map(|entry| ((entry.association_space_id, entry.context_id), &entry.paths))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(flattened.len(), many.items.len());
    for item in &many.items {
        assert_eq!(item.association_space_id, item.context.space_id);
        assert!(association_spaces.contains(&item.association_space_id));
        assert_eq!(item.context.status, ContextStatus::Accepted);
        assert!(item.context.auto_injection_eligible);
        assert!(!item.context.evidence.is_empty());
        assert!(item.context.conflicts.is_empty());
        assert!(!item.retrieval_paths.is_empty());
        assert_eq!(
            flattened[&(item.association_space_id, item.context.context_id)],
            &item.retrieval_paths
        );
        item.retrieval_paths.iter().for_each(assert_typed_m2_path);
    }
    assert!(
        many.candidate_spaces
            .iter()
            .all(|association| association.relation_paths.is_empty()),
        "M2 textual paths must not masquerade as an M3 Engineering Graph"
    );

    let metadata = fixture.index.metadata().unwrap();
    assert_eq!(many.tree, metadata.indexed_tree_oid);
    assert_eq!(many.tree, git_tree(fixture.store.repository()));
    assert_eq!(many.generation, metadata.projection_generation);
    assert_eq!(many.task_fingerprint.len(), 64);
    let repeated = task_context_readonly_at_root(&fixture.root, &read_input).unwrap();
    assert_eq!(repeated.task_session_id, many.task_session_id);
    assert_eq!(repeated.task_id, many.task_id);
    assert_eq!(repeated.intent_revision_id, many.intent_revision_id);
    assert_eq!(repeated.task_fingerprint, many.task_fingerprint);
    assert_eq!(repeated.tree, many.tree);
    assert_eq!(repeated.generation, many.generation);
    assert_eq!(repeated.candidate_spaces, many.candidate_spaces);
    assert_eq!(repeated.items, many.items);
    assert_eq!(repeated.retrieval_paths, many.retrieval_paths);

    let unsafe_input = fixture.input("unsafe-session", "hazardpackintent");
    let automatic_unsafe = establish_task(&fixture.root, &unsafe_input);
    assert_eq!(
        response_spaces(&automatic_unsafe),
        fixture.unsafe_spaces.into_iter().collect()
    );
    assert!(automatic_unsafe.items.is_empty());
    assert!(
        !serde_json::to_string(&automatic_unsafe)
            .unwrap()
            .contains(&fixture.unassigned_candidate_id)
    );

    let explicit_unsafe = SearchEngine::new(fixture.index.clone())
        .task_context_pack(&TaskContextRequest {
            task_id: TaskId::new(),
            working_intent: task_intent("hazardpackintent"),
            task_signals: Vec::new(),
            resolved_focus: None,
            token_budget: 100_000,
            max_spaces: sctx_search::DEFAULT_TASK_MAX_SPACES,
            candidate_limit: 100,
            mode: ContextPackMode::Explicit,
        })
        .unwrap();
    assert!(
        explicit_unsafe
            .items
            .iter()
            .any(|item| item.context.status == ContextStatus::Candidate)
    );
    assert!(
        explicit_unsafe
            .items
            .iter()
            .any(|item| item.context.status == ContextStatus::Deprecated)
    );
    assert!(
        explicit_unsafe
            .items
            .iter()
            .any(|item| !item.context.conflicts.is_empty())
    );
    assert!(
        explicit_unsafe
            .items
            .iter()
            .flat_map(|item| item.context.evidence.iter())
            .any(|evidence| evidence.limitations.is_empty())
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn post_tool_file_is_breadcrumb_only_and_test_outcome_refreshes_active_task() {
    let fixture = MilestoneTwoFixture::new();
    let session_id = "hook-signal-session";
    let activation = fixture.hook(&serde_json::json!({
        "session_id": session_id,
        "transcript_path": null,
        "cwd": fixture.workspace,
        "hook_event_name": "SessionStart",
        "model": "gpt-5.6-sol",
        "permission_mode": "default",
        "source": "startup"
    }));
    assert!(
        activation["systemMessage"]
            .as_str()
            .is_some_and(|message| message.contains("<shared-context-active>"))
    );
    let update = |boundary: TaskBoundary, expected: Option<String>| TaskIntentUpdateInput {
        agent_kind: "codex".to_owned(),
        external_session_id: session_id.to_owned(),
        task_boundary: boundary,
        expected_revision_id: expected
            .map_or(ExpectedRevisionId::Null(()), ExpectedRevisionId::Revision),
        intent: WorkingIntentSnapshot {
            goal: "quartzpageintent".to_owned(),
            current_direction: Some("implement quartz page intent".to_owned()),
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

    let initial = task_intent_update_at_root(&fixture.root, &update(TaskBoundary::New, None))
        .unwrap()
        .context;
    assert_eq!(
        initial.candidate_spaces.len(),
        1,
        "initial associations: {:?}",
        initial.candidate_spaces
    );
    assert_eq!(initial.items.len(), 1);
    assert!(initial.items.iter().all(|item| {
        item.retrieval_paths.iter().all(|path| {
            !matches!(
                path,
                TaskRetrievalPath::EngineeringGraph { .. }
                    | TaskRetrievalPath::GraphDiagnostic { .. }
            )
        })
    }));

    let post_tool = fixture.hook(&serde_json::json!({
        "session_id": session_id,
        "transcript_path": null,
        "cwd": fixture.workspace,
        "hook_event_name": "PostToolUse",
        "model": "gpt-5.6-sol",
        "permission_mode": "default",
        "turn_id": "m2-turn",
        "tool_name": "LegacyCompatibilityTest",
        "tool_use_id": "m2-tool",
        "tool_input": {"file_path": fixture.workspace.join("src/search_results_page.tsx")},
        "tool_response": {"output": "passed"}
    }));
    assert_eq!(post_tool, serde_json::json!({}));

    let updated = task_intent_update_at_root(
        &fixture.root,
        &update(
            TaskBoundary::Continue,
            Some(initial.intent_revision_id.to_string()),
        ),
    )
    .unwrap()
    .context;
    assert_eq!(updated.task_id, initial.task_id);
    assert_ne!(updated.task_fingerprint, initial.task_fingerprint);
    assert_eq!(updated.tree, initial.tree);
    assert_eq!(updated.generation, initial.generation);
    assert_eq!(updated.candidate_spaces.len(), 1);
    assert_eq!(updated.items.len(), 1);
    let paths = updated
        .items
        .iter()
        .flat_map(|item| item.retrieval_paths.iter())
        .collect::<Vec<_>>();
    let snapshot = TaskRuntime::initialize(&fixture.root)
        .unwrap()
        .read_snapshot_by_locator(&ExternalSessionLocator::new("codex", session_id).unwrap())
        .unwrap()
        .unwrap();
    assert!(paths.iter().all(|path| {
        !matches!(
            path,
            TaskRetrievalPath::EngineeringGraph { .. } | TaskRetrievalPath::GraphDiagnostic { .. }
        )
    }));

    assert_eq!(snapshot.task_id, updated.task_id);
    assert!(
        !snapshot
            .task_signals
            .iter()
            .any(|signal| signal.content == "src/search_results_page.tsx")
    );
    assert!(snapshot.task_signals.iter().any(|signal| {
        signal.kind == TaskSignalKind::TestOutcome
            && signal.content == "LegacyCompatibilityTest succeeded"
    }));
}
