use std::{
    fs,
    path::Path,
    sync::{Arc, Barrier},
    thread,
};

use sctx_domain::{
    Applicability, ContextKind, ContextRevisionDraft, EvidenceSnapshotDraft, EvidenceType,
    IntentSnapshot, PublicationAction, PublicationDraft, ReviewDraft, ReviewVerdict,
    WorkingIntentSnapshot,
};
use sctx_event_schema::{Event, EventPayload};
use sctx_git_store::{AppendRequest, GitStore};
use sctx_index::ProjectionIndex;
use sctx_mcp::{
    ExpectedRevisionId, IntentRevisionStatus, TaskBoundary, TaskIntentUpdateInput,
    task_intent_update_at_root,
};
use sctx_task_runtime::{IntentRevisionWriteStatus, TaskRuntime};
use tempfile::TempDir;

fn append(store: &GitStore, event: Event) {
    store.append_event(AppendRequest::event(event)).unwrap();
}

/// The file the oracle Context is recorded against, and the hint the Working Intent names it with.
const RENDERER_FILE: &str = "src/search/searchresultrenderer.ts";

fn seed_text_context(store: &GitStore) -> usize {
    let space = Event::space_created(
        IntentSnapshot {
            title: "Search renderer".to_owned(),
            problem: "SearchResultRenderer needs search-v2-endpoint history".to_owned(),
            desired_outcome: "Reuse the historical interface decision".to_owned(),
            in_scope: vec!["SearchResultRenderer".to_owned()],
            out_of_scope: Vec::new(),
            acceptance_conditions: vec!["search-v2-endpoint remains compatible".to_owned()],
            domain_terms: Vec::new(),
        },
        None,
    )
    .unwrap();
    let EventPayload::SpaceCreated { space_id, .. } = space.payload() else {
        unreachable!()
    };
    let space_id = *space_id;
    append(store, space);
    let context = Event::context_revision_added(
        space_id,
        ContextRevisionDraft {
            problem_view: None,
            hints: Vec::new(),
            kind: ContextKind::Contract,
            topic_key: Some("search/v2".to_owned()),
            statement: "SearchResultRenderer consumes search-v2-endpoint".to_owned(),
            rationale: "The interface hint should retrieve this text only".to_owned(),
            applicability: Applicability::default(),
            assumptions: Vec::new(),
            recheck_when: Vec::new(),
            relations: Vec::new(),
            evidence: vec![EvidenceSnapshotDraft {
                kind: EvidenceType::ExperimentRecord,
                supports: "The fixed oracle context is searchable".to_owned(),
                content: serde_json::json!({"actual": "searchable"}),
                interpretation: "This Evidence belongs to Context, never Working Intent".to_owned(),
                limitations: vec!["fixed local oracle".to_owned()],
            }],
        },
        None,
    )
    .unwrap();
    let EventPayload::ContextRevisionAdded {
        context_id,
        revision,
        ..
    } = context.payload()
    else {
        unreachable!()
    };
    let (context_id, revision_id) = (*context_id, revision.revision_id);
    append(store, context);
    let review = Event::context_reviewed(
        space_id,
        context_id,
        ReviewDraft {
            revision_id,
            verdict: ReviewVerdict::Approve,
            reason: "fixed oracle approval".to_owned(),
        },
        None,
    )
    .unwrap();
    let review_event_id = review.event_id();
    append(store, review);
    append(
        store,
        Event::engineering_reference_recorded(
            context_id,
            revision_id,
            sctx_domain::EngineeringReferenceDraft {
                repository_id: "Web".parse().unwrap(),
                artifact_kind: sctx_domain::ArtifactKind::File,
                relation: sctx_domain::ReferenceRelation::Implements,
                locator: sctx_domain::ArtifactLocator::File {
                    path: sctx_domain::RepoRelativePath::new(RENDERER_FILE).unwrap(),
                },
                supports: "the oracle Context is about the renderer file".to_owned(),
                limitations: vec!["fixed local oracle".to_owned()],
            },
            None,
        )
        .unwrap(),
    );
    append(
        store,
        Event::publication_changed(
            space_id,
            context_id,
            PublicationDraft {
                previous_publication_ids: Vec::new(),
                action: PublicationAction::Publish,
                revision_id,
                review_event_ids: vec![review_event_id],
            },
            None,
        )
        .unwrap(),
    );
    // Space, revision, review, Reference, publication.
    5
}

fn input(
    session: &str,
    boundary: TaskBoundary,
    expected: ExpectedRevisionId,
    intent: WorkingIntentSnapshot,
) -> TaskIntentUpdateInput {
    TaskIntentUpdateInput {
        agent_kind: "codex".to_owned(),
        external_session_id: session.to_owned(),
        task_boundary: boundary,
        expected_revision_id: expected,
        intent,
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn fixed_working_intent_cross_layer_oracle() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join("working-intent-oracle");
    let store = GitStore::bootstrap_local(&root).unwrap();
    let event_count = seed_text_context(&store);
    let goal_only: WorkingIntentSnapshot =
        serde_json::from_value(serde_json::json!({"goal": "Implement search"})).unwrap();
    assert!(goal_only.open_questions.is_empty());
    let initial = task_intent_update_at_root(
        &root,
        &input(
            "oracle",
            TaskBoundary::New,
            ExpectedRevisionId::Null(()),
            goal_only,
        ),
    )
    .unwrap();
    assert_eq!(initial.revision_status, IntentRevisionStatus::Created);

    // The hint names a file rather than a symbol, because a hint is an anchor now: ADR-0007 reads
    // the spellings that read as a path and joins them against the Engineering Reference rows. A
    // bare `SearchResultRenderer` is still a legal hint and still recorded on the Intent; it simply
    // names no file, so it anchors nothing.
    let hinted = WorkingIntentSnapshot {
        goal: "Implement search".to_owned(),
        artifact_hints: vec![RENDERER_FILE.to_owned()],
        interface_hints: vec!["search-v2-endpoint".to_owned()],
        ..WorkingIntentSnapshot::new("Implement search").unwrap()
    };
    let changed = task_intent_update_at_root(
        &root,
        &input(
            "oracle",
            TaskBoundary::Continue,
            ExpectedRevisionId::Revision(initial.context.intent_revision_id.to_string()),
            hinted.clone(),
        ),
    )
    .unwrap();
    assert_eq!(changed.revision_status, IntentRevisionStatus::Created);
    assert!(!changed.context.items.is_empty());
    assert!(
        changed
            .context
            .items
            .iter()
            .flat_map(|item| &item.retrieval_paths)
            .all(|path| matches!(path, sctx_search::TaskRetrievalPath::FileAnchor { .. })),
        "a hint reaches knowledge by naming a file, and by nothing else: {:#?}",
        changed
            .context
            .items
            .iter()
            .map(|item| &item.retrieval_paths)
            .collect::<Vec<_>>()
    );
    // Each explanation reaches the host once. The full shape used to carry a second, flattened
    // copy of every item's paths at the top level -- byte-for-byte identical, 24.7% of one
    // measured Pack's wire, charged by nothing and read by nobody.
    let body = serde_json::to_string(&changed).unwrap();
    for item in &changed.context.items {
        let encoded = serde_json::to_string(&item.retrieval_paths).unwrap();
        assert_eq!(
            body.matches(encoded.as_str()).count(),
            1,
            "one explanation, one copy on the wire: {encoded}"
        );
    }

    let mut equivalent = hinted.clone();
    equivalent.goal = "  IMPLEMENT   search ".to_owned();
    equivalent.artifact_hints[0] = RENDERER_FILE.to_uppercase();
    let retry = task_intent_update_at_root(
        &root,
        &input(
            "oracle",
            TaskBoundary::Continue,
            ExpectedRevisionId::Revision(changed.context.intent_revision_id.to_string()),
            equivalent,
        ),
    )
    .unwrap();
    assert_eq!(retry.revision_status, IntentRevisionStatus::AlreadyCurrent);
    assert_eq!(
        retry.context.intent_revision_id,
        changed.context.intent_revision_id
    );

    let runtime = Arc::new(TaskRuntime::initialize(&root).unwrap());
    let session = runtime
        .read_snapshot_by_locator(
            &sctx_domain::ExternalSessionLocator::new("codex", "oracle").unwrap(),
        )
        .unwrap()
        .unwrap();
    let parent = session.current_intent_revision().unwrap().revision_id;
    let task_session_id = session.task_session_id;
    let original_task_id = session.task_id;
    let barrier = Arc::new(Barrier::new(20));
    let outcomes = (0..20)
        .map(|_| {
            let runtime = Arc::clone(&runtime);
            let barrier = Arc::clone(&barrier);
            let mut next = hinted.clone();
            next.current_direction = Some("Ship the renderer".to_owned());
            thread::spawn(move || {
                barrier.wait();
                runtime
                    .append_intent_revision(task_session_id, parent, next)
                    .unwrap()
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| outcome.status == IntentRevisionWriteStatus::Created)
            .count(),
        1
    );
    let successor = outcomes[0].revision.revision_id;
    let before_stale = runtime
        .read_snapshot(task_session_id)
        .unwrap()
        .unwrap()
        .intent_revisions
        .len();
    let mut stale_change = hinted.clone();
    stale_change.current_direction = Some("A different stale direction".to_owned());
    assert!(
        runtime
            .append_intent_revision(task_session_id, parent, stale_change)
            .is_err()
    );
    assert_eq!(
        runtime
            .read_snapshot(task_session_id)
            .unwrap()
            .unwrap()
            .intent_revisions
            .len(),
        before_stale
    );

    let explicit_new = task_intent_update_at_root(
        &root,
        &input(
            "oracle",
            TaskBoundary::New,
            ExpectedRevisionId::Revision(successor.to_string()),
            hinted.clone(),
        ),
    )
    .unwrap();
    assert_eq!(explicit_new.revision_status, IntentRevisionStatus::Created);
    assert_ne!(explicit_new.context.task_id, original_task_id);
    let locator = sctx_domain::ExternalSessionLocator::new("codex", "oracle").unwrap();
    let switched = runtime
        .switch_active_task(&locator, explicit_new.context.task_id, original_task_id)
        .unwrap();
    assert!(switched.switched);
    assert_eq!(switched.snapshot.task_id, original_task_id);

    let projection = ProjectionIndex::for_store(&store)
        .domain_snapshot()
        .unwrap()
        .projection;
    assert!(projection.candidates.is_empty());
    // The corpus seeded exactly one Reference, on the Context. Working Intent activity adds none:
    // a hint that names a file is read as a coordinate at retrieval time and never written down as
    // one, which is what keeps the Intent a disposable local note rather than a source of
    // engineering facts.
    assert_eq!(projection.engineering_references.len(), 1);
    assert_eq!(
        outcomes
            .iter()
            .map(|outcome| outcome.revision.revision_id)
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        1
    );
    assert_eq!(store.list_pending().unwrap().len(), 0);
    assert_eq!(
        std::process::Command::new("git")
            .arg("-C")
            .arg(store.repository())
            .args(["ls-tree", "-r", "--name-only", "HEAD", "--", "events"])
            .output()
            .unwrap()
            .stdout
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .count(),
        event_count
    );
    let database = runtime.database_path().to_path_buf();
    drop(runtime);
    fs::remove_file(database).unwrap();
    assert!(
        TaskRuntime::initialize(&root)
            .unwrap()
            .read_snapshot_by_locator(&locator)
            .unwrap()
            .is_none()
    );
}

#[test]
fn production_working_intent_residue_is_zero() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mcp = fs::read_to_string(root.join("crates/mcp/src/lib.rs")).unwrap();
    let cli = fs::read_to_string(root.join("crates/cli/src/main.rs")).unwrap();
    let git_store = fs::read_to_string(root.join("crates/git-store/src/store.rs")).unwrap();
    let domain = fs::read_to_string(root.join("crates/domain/src/task.rs")).unwrap();
    let runtime = fs::read_to_string(root.join("crates/task-runtime/src/lib.rs")).unwrap();
    for residue in [
        "IntentMaturity",
        "pub struct TaskIntent {",
        "pub struct TaskIntentDraft {",
    ] {
        assert!(
            !mcp.contains(residue) && !domain.contains(residue) && !runtime.contains(residue),
            "legacy residue: {residue}"
        );
    }
    let schema = mcp
        .split("fn task_intent_update_schema()")
        .nth(1)
        .unwrap()
        .split("fn task_signal_supersede_schema()")
        .next()
        .unwrap();
    for residue in [
        "maturity",
        "evidence_refs",
        "desired_change",
        "artifacts",
        "interfaces",
        "unknowns",
    ] {
        assert!(
            !schema.contains(residue),
            "public Intent schema residue: {residue}"
        );
    }
    let table = runtime
        .split("CREATE TABLE IF NOT EXISTS task_intent_revision")
        .nth(1)
        .unwrap()
        .split(") STRICT;")
        .next()
        .unwrap();
    for residue in ["maturity", "evidence", "intent_json", "task_id"] {
        assert!(
            !table.contains(residue),
            "runtime Intent schema residue: {residue}"
        );
    }
    let removed_manual_entry = ["candidate", "_create"].concat();
    let removed_manual_command = ["candidate", " create"].concat();
    assert!(!mcp.contains(&removed_manual_entry));
    assert!(!cli.contains(&removed_manual_command));
    assert!(git_store.contains("submit_candidate"));
}

#[test]
fn working_intent_document_language_is_current_and_bounded() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let context = fs::read_to_string(root.join("CONTEXT.md")).unwrap();
    let development = fs::read_to_string(root.join("DEVELOPMENT.md")).unwrap();
    let technical = fs::read_to_string(root.join("technical-design.md")).unwrap();
    let acceptance = fs::read_to_string(root.join("docs/acceptance-report.md")).unwrap();

    for required in [
        "**WorkingIntentSnapshot**:",
        "**TaskIntentRevision**:",
        "**TaskSignal**:",
        "**DirectEvidenceDraft**:",
        "**CheckpointOperation**:",
        "**CandidateBuildOutbox**:",
        "**Evidence**:",
        "never engineering Evidence",
    ] {
        assert!(
            context.contains(required),
            "missing glossary language: {required}"
        );
    }
    for required in [
        "`task_intent_update`",
        "`WorkingIntentSnapshot`",
        "`TaskIntentRevision`",
    ] {
        assert!(
            development.contains(required)
                && technical.contains(required)
                && acceptance.contains(required),
            "current-state docs are missing {required}"
        );
    }
    assert!(
        technical.contains(
            "Evidence 绑定 WorkObservation、CheckpointClaim、Candidate、ContextRevision 或 EngineeringReference"
        )
    );
    assert!(development.contains("M4：Direct Checkpoint / Candidate Review"));
    assert!(acceptance.contains("Mew #226–#229"));
    assert!(
        development.contains("#164 固定 oracle")
            && technical.contains("#164 固定 oracle")
            && acceptance.contains("direct_evidence_workflow")
    );

    let current_state = [context, development, technical, acceptance]
        .join("\n")
        .to_lowercase();
    for residue in [
        "a signal is evidence about the current task",
        "superseded signals remain historical evidence",
        "`taskintent` 不包含 space",
        "各自的 taskintent",
        "创建或修订权威 taskintent",
        "以 `taskintent` 作为默认检索入口",
        "| `taskintent` |",
        "m3 defines the verifiable evidence source boundary for deferred #136",
        "tasksignal, contextevidence and engineeringresolution sources",
        "grounded working intent",
        "grounded workingintent",
        "working intent maturity",
        "tasksignal 是 evidence",
        "tasksignal 作为 evidence",
    ] {
        assert!(
            !current_state.contains(residue),
            "obsolete document language: {residue}"
        );
    }
}
