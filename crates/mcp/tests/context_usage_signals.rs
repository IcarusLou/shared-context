//! Server-recorded injection results and the usage prior they feed back into retrieval.
//!
//! Nothing here is Agent-authored: the Checkpoint contract stays the four public fields, and every
//! usage outcome is derived from text the Agent wrote for its own purpose.

use std::{fs, path::Path, process::Command};

use sctx_domain::{
    Applicability, ArtifactKind, ArtifactLocator, CandidateId, ContextId, ContextKind,
    ContextRelation, ContextRelationKind, ContextRevisionDraft, EvidenceSnapshotDraft,
    EvidenceType, ExternalSessionLocator, IntentSnapshot, OptionalCandidateEdits,
    PublicationAction, PublicationDraft, ReferenceRelation, RepoRelativePath, ReviewDraft,
    ReviewVerdict, RevisionId, SpaceId, WorkingIntentSnapshot,
};
use sctx_event_schema::{Event, EventPayload};
use sctx_git_store::{AppendRequest, GitStore};
use sctx_local_state::{AuthorizedSessionScopeStore, UserConfigStore};
use sctx_mcp::{
    CandidateConfirmInput, CandidateConfirmPrimaryInput, CandidateListInput,
    EngineeringReferenceRecordInput, ExistingCandidatePrimaryInput, ExpectedRevisionId,
    TaskBoundary, TaskCheckpointClaimInput, TaskCheckpointEvidenceInput, TaskCheckpointInput,
    TaskContextReadInput, TaskIntentUpdateInput, build_closed_episode_at_root,
    candidate_confirm_at_root, candidate_list_at_root, engineering_reference_record_at_root,
    task_checkpoint_at_root, task_context_readonly_with_detail_at_root, task_intent_update_at_root,
};
use sctx_search::ContextPackDetailLevel;
use sctx_task_runtime::{ContextInjectionSource, ContextUsageTotals, TaskRuntime};

const REUSED_STATEMENT: &str =
    "quorum ledger replay preserves deterministic ordering across restarts";
const IGNORED_STATEMENT: &str = "quorum ledger replay tolerates truncated segments during recovery";
const GOAL: &str = "quorum ledger replay";

fn accepted_context(root: &Path, statement: &str) -> (SpaceId, ContextId, RevisionId) {
    let store = GitStore::bootstrap_local(root).unwrap();
    let space = Event::space_created(
        IntentSnapshot {
            title: statement.to_owned(),
            problem: "Agents keep rediscovering the same replay behavior".to_owned(),
            desired_outcome: "The behavior is inherited instead of re-derived".to_owned(),
            in_scope: vec![statement.to_owned()],
            out_of_scope: Vec::new(),
            acceptance_conditions: vec!["The Context is retrievable".to_owned()],
            domain_terms: vec![statement.to_owned()],
        },
        None,
    )
    .unwrap();
    let space_id = match space.payload() {
        EventPayload::SpaceCreated { space_id, .. } => *space_id,
        _ => unreachable!(),
    };
    store.append_event(AppendRequest::event(space)).unwrap();
    let draft = ContextRevisionDraft {
        problem_view: None,
        hints: Vec::new(),
        kind: ContextKind::Discovery,
        topic_key: Some(format!("usage/{statement}")),
        statement: statement.to_owned(),
        rationale: "The fixture verified the behavior directly".to_owned(),
        applicability: Applicability::default(),
        assumptions: Vec::new(),
        recheck_when: Vec::new(),
        relations: Vec::new(),
        evidence: vec![EvidenceSnapshotDraft {
            kind: EvidenceType::ExperimentRecord,
            supports: "The usage fixture verified the Context".to_owned(),
            content: serde_json::json!({"test": "context_usage_signals", "result": "passed"}),
            interpretation: "The Context is safe for automatic retrieval".to_owned(),
            limitations: vec!["Synthetic fixture".to_owned()],
        }],
    };
    let revision = Event::context_revision_added(space_id, draft, None).unwrap();
    let (context_id, revision_id) = match revision.payload() {
        EventPayload::ContextRevisionAdded {
            context_id,
            revision,
            ..
        } => (*context_id, revision.revision_id),
        _ => unreachable!(),
    };
    store.append_event(AppendRequest::event(revision)).unwrap();
    let review = Event::context_reviewed(
        space_id,
        context_id,
        ReviewDraft {
            revision_id,
            verdict: ReviewVerdict::Approve,
            reason: "Verified fixture evidence".to_owned(),
        },
        None,
    )
    .unwrap();
    let review_event_id = review.event_id();
    store.append_event(AppendRequest::event(review)).unwrap();
    store
        .append_event(AppendRequest::event(
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
        ))
        .unwrap();
    (space_id, context_id, revision_id)
}

fn intent_update(root: &Path, session: &str) -> sctx_mcp::TaskIntentUpdateResponse {
    task_intent_update_at_root(
        root,
        &TaskIntentUpdateInput {
            agent_kind: "codex".to_owned(),
            external_session_id: session.to_owned(),
            task_boundary: TaskBoundary::New,
            expected_revision_id: ExpectedRevisionId::Null(()),
            intent: WorkingIntentSnapshot {
                goal: GOAL.to_owned(),
                current_direction: Some("Extend the replay path".to_owned()),
                in_scope: vec![GOAL.to_owned()],
                out_of_scope: Vec::new(),
                domains: vec!["ledger".to_owned()],
                platforms: Vec::new(),
                constraints: Vec::new(),
                acceptance_conditions: vec!["The Pack is returned".to_owned()],
                artifact_hints: Vec::new(),
                interface_hints: Vec::new(),
                open_questions: Vec::new(),
            },
        },
    )
    .unwrap()
}

/// Submits one Checkpoint and drains its Candidate Build outbox.
///
/// The durable ACK is receipt plus outbox (ADR-0003): it never reads the retrieval index, so the
/// injection comparison happens where the Build already reconstructs the Claims. Every caller here
/// wants the state an Agent reaches after `candidate_list`, so the drain belongs in the helper.
///
/// `cites` are Contexts the Claim names by identity in its own rationale, which is how an Agent
/// says "I built this on what you gave me" without the server asking it a question.
fn checkpoint(root: &Path, session: &str, statement: &str, cites: &[ContextId]) -> bool {
    let accepted = submit_checkpoint(root, session, statement, cites);
    build_closed_episode_at_root(root, accepted.episode_id).unwrap();
    accepted.replayed
}

fn submit_checkpoint(
    root: &Path,
    session: &str,
    statement: &str,
    cites: &[ContextId],
) -> sctx_mcp::TaskCheckpointAcceptedResponse {
    let mut rationale = "The Task confirmed the inherited behavior while extending it".to_owned();
    for context_id in cites {
        rationale.push_str(&format!(" (building on {context_id})"));
    }
    task_checkpoint_at_root(
        root,
        &TaskCheckpointInput {
            agent_kind: "codex".to_owned(),
            external_session_id: session.to_owned(),
            claims: vec![TaskCheckpointClaimInput {
                context_kind: ContextKind::Validation,
                statement: statement.to_owned(),
                rationale,
                conditions: Vec::new(),
                evidence: vec![TaskCheckpointEvidenceInput {
                    evidence_type: EvidenceType::ExperimentRecord,
                    summary: "The replay suite passed on the reviewed branch".to_owned(),
                    limitations: vec!["local fixture".to_owned()],
                }],
            }],
            unknowns: Vec::new(),
        },
    )
    .unwrap()
    .into_accepted()
    .expect("nonempty Checkpoint must be accepted")
}

#[test]
fn injections_are_recorded_and_one_checkpoint_separates_reuse_from_omission() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("context usage root");
    let (_, reused, _) = accepted_context(&root, REUSED_STATEMENT);
    let (_, ignored, _) = accepted_context(&root, IGNORED_STATEMENT);
    let session = "usage-first-task";

    let task = intent_update(&root, session);
    let task_id = task.context.task_id;
    let returned = task
        .context
        .items
        .iter()
        .map(|item| item.context.context_id)
        .collect::<Vec<_>>();
    assert!(
        returned.contains(&reused),
        "fixture Context must be injected"
    );
    assert!(
        returned.contains(&ignored),
        "fixture Context must be injected"
    );

    let runtime = TaskRuntime::initialize(&root).unwrap();
    let injections = runtime.read_task_injections(task_id).unwrap();
    assert_eq!(injections.len(), returned.len());
    assert!(injections.iter().all(|injection| injection.source
        == ContextInjectionSource::IntentUpdate
        && injection.intent_revision_id == task.context.intent_revision_id));
    assert!(
        runtime
            .context_usage_totals(&[reused, ignored])
            .unwrap()
            .is_empty(),
        "no Checkpoint has compared anything yet"
    );

    assert!(!checkpoint(&root, session, REUSED_STATEMENT, &[reused]));
    let totals = runtime.context_usage_totals(&[reused, ignored]).unwrap();
    assert_eq!(
        totals[&reused],
        ContextUsageTotals {
            reused: 1,
            ignored: 0,
            refuted: 0,
        }
    );
    assert_eq!(
        totals[&ignored],
        ContextUsageTotals {
            reused: 0,
            ignored: 1,
            refuted: 0,
        }
    );

    // A same-content replay is idempotent: it neither doubles a count nor changes a verdict.
    assert!(checkpoint(&root, session, REUSED_STATEMENT, &[reused]));
    assert_eq!(
        runtime.context_usage_totals(&[reused, ignored]).unwrap(),
        totals
    );

    // Reading the Pack again re-injects the same Contexts without adding rows.
    let repeated = task_context_readonly_with_detail_at_root(
        &root,
        &TaskContextReadInput {
            agent_kind: "codex".to_owned(),
            external_session_id: session.to_owned(),
            token_budget: 8_000,
            max_spaces: 8,
        },
        ContextPackDetailLevel::Full,
    )
    .unwrap();
    assert!(!repeated.items.is_empty());
    assert_eq!(
        runtime.read_task_injections(task_id).unwrap().len(),
        injections.len()
    );
}

#[test]
fn a_reused_context_outranks_its_sibling_and_says_so_in_the_compact_pack() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("usage prior root");
    let (_, reused, _) = accepted_context(&root, REUSED_STATEMENT);
    let (_, ignored, _) = accepted_context(&root, IGNORED_STATEMENT);

    let first = intent_update(&root, "usage-prior-first");
    assert!(
        first
            .context
            .items
            .iter()
            .all(|item| item.context.usage.is_empty()),
        "a Context nobody has used yet reports no usage"
    );
    assert!(!checkpoint(
        &root,
        "usage-prior-first",
        REUSED_STATEMENT,
        &[reused]
    ));

    let second = intent_update(&root, "usage-prior-second");
    let full = second
        .context
        .items
        .iter()
        .find(|item| item.context.context_id == reused)
        .expect("the reused Context is still retrievable");
    assert_eq!(full.context.usage.reused, 1);
    assert_eq!(full.context.usage.ignored, 0);

    let compact = task_context_readonly_with_detail_at_root(
        &root,
        &TaskContextReadInput {
            agent_kind: "codex".to_owned(),
            external_session_id: "usage-prior-second".to_owned(),
            token_budget: 8_000,
            max_spaces: 8,
        },
        ContextPackDetailLevel::Compact,
    )
    .unwrap();
    let order = compact
        .compact_items
        .iter()
        .map(|item| item.context_id)
        .collect::<Vec<_>>();
    let reused_rank = order.iter().position(|id| *id == reused).unwrap();
    let ignored_rank = order.iter().position(|id| *id == ignored).unwrap();
    assert!(
        reused_rank < ignored_rank,
        "the reused Context must sort ahead of its equally scored sibling: {order:?}"
    );
    assert!(
        compact.compact_items[reused_rank]
            .why
            .iter()
            .any(|reason| reason == "Reused in 1 prior task(s)."),
        "compact why explains the prior: {:?}",
        compact.compact_items[reused_rank].why
    );
    assert!(
        compact.compact_items[ignored_rank]
            .why
            .iter()
            .all(|reason| !reason.contains("Reused in") && !reason.contains("Ignored in")),
        "one omission is not enough to report an ignore: {:?}",
        compact.compact_items[ignored_rank].why
    );
}

/// A Task that explicitly contradicts a Context it was given records the refutation, and no later
/// Checkpoint verdict downgrades it.
#[test]
fn confirming_a_contradiction_refutes_the_injected_context() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("refutation root");
    let (space_id, reused, _) = accepted_context(&root, REUSED_STATEMENT);
    let (_, ignored, _) = accepted_context(&root, IGNORED_STATEMENT);
    let session = "usage-refutation";

    let task = intent_update(&root, session);
    assert!(
        task.context
            .items
            .iter()
            .any(|item| item.context.context_id == reused)
    );
    // The Claim restates the injected Context first, so the refutation has to override a reuse.
    assert!(!checkpoint(&root, session, REUSED_STATEMENT, &[reused]));
    let runtime = TaskRuntime::initialize(&root).unwrap();
    assert_eq!(
        runtime.context_usage_totals(&[reused]).unwrap()[&reused],
        ContextUsageTotals {
            reused: 1,
            ignored: 0,
            refuted: 0,
        }
    );

    let candidate_id: CandidateId = candidate_list_at_root(
        &root,
        &CandidateListInput {
            scope: sctx_domain::CandidateReviewScope::Task,
            agent_kind: "codex".to_owned(),
            external_session_id: session.to_owned(),
            status: sctx_domain::CandidateReviewStatus::Pending,
            limit: 10,
            cursor: None,
            token_budget: 8_192,
        },
    )
    .unwrap()
    .reviews
    .first()
    .expect("the Checkpoint produced one Candidate")
    .0
    .candidate_id;
    candidate_confirm_at_root(
        &root,
        &CandidateConfirmInput {
            agent_kind: "codex".to_owned(),
            external_session_id: session.to_owned(),
            expected_task_id: task.context.task_id.to_string(),
            expected_intent_revision_id: task.context.intent_revision_id.to_string(),
            candidate_id: candidate_id.to_string(),
            expected_review_version: 1,
            primary: CandidateConfirmPrimaryInput::Existing(ExistingCandidatePrimaryInput {
                existing_space_id: space_id.to_string(),
            }),
            related_space_ids: Vec::new(),
            edits: OptionalCandidateEdits {
                relations: Some(vec![ContextRelation {
                    target_context_id: reused,
                    kind: ContextRelationKind::Contradicts,
                    rationale: "the reviewed branch no longer preserves the recorded ordering"
                        .to_owned(),
                    supports: vec!["the two statements cannot both hold".to_owned()],
                }]),
                ..OptionalCandidateEdits::default()
            },
        },
    )
    .unwrap();

    let totals = runtime.context_usage_totals(&[reused, ignored]).unwrap();
    assert_eq!(
        totals[&reused],
        ContextUsageTotals {
            reused: 0,
            ignored: 0,
            refuted: 1,
        }
    );
    assert_eq!(
        totals[&ignored],
        ContextUsageTotals {
            reused: 0,
            ignored: 1,
            refuted: 0,
        },
        "a Context nobody contradicted keeps the Checkpoint verdict"
    );
}

/// Restating an injected Context is not reuse (P3).
///
/// The comparison this replaces scored normalized statement token Jaccard, so a Claim that
/// repeated its input scored highest while the Claim that carried a genuinely *new* conclusion
/// built on that input scored nothing. Both real reuses observed in session `01a060a1` were
/// recorded `ignored` that way, and the usage prior optimized backwards for as long as it ran. A
/// restatement now records exactly what it is.
#[test]
fn restating_an_injected_context_is_not_reuse() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("restatement root");
    let (_, reused, _) = accepted_context(&root, REUSED_STATEMENT);
    let (_, ignored, _) = accepted_context(&root, IGNORED_STATEMENT);
    let session = "usage-restatement";

    intent_update(&root, session);
    // The Claim repeats the injected statement word for word and names nothing.
    assert!(!checkpoint(&root, session, REUSED_STATEMENT, &[]));

    let runtime = TaskRuntime::initialize(&root).unwrap();
    let totals = runtime.context_usage_totals(&[reused, ignored]).unwrap();
    assert_eq!(
        totals[&reused],
        ContextUsageTotals {
            reused: 0,
            ignored: 1,
            refuted: 0,
        },
        "a word-for-word restatement is the weakest possible reuse signal, not the strongest"
    );
    assert_eq!(
        totals[&ignored],
        ContextUsageTotals {
            reused: 0,
            ignored: 1,
            refuted: 0,
        }
    );
}

fn init_repo(path: &Path, relative: &str, content: &str) {
    fs::create_dir_all(path).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .arg(path)
            .status()
            .unwrap()
            .success()
    );
    let target = path.join(relative);
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(target, content).unwrap();
    for args in [
        vec!["config", "user.name", "Usage Signals"],
        vec!["config", "user.email", "usage@example.invalid"],
        vec!["add", "--", "."],
        vec!["commit", "-q", "-m", "fixture"],
    ] {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(path)
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    }
}

/// A Claim that lands on a coordinate its injected Context already referenced is reuse, even
/// though the two statements share no wording at all.
///
/// This is the signal that survives the restatement gate: a Task inherits an Engineering fact,
/// works on the same artifact, and concludes something new about it. The intersection is taken
/// after Candidate Build has derived the Claim's own path spellings, so both sides are
/// server-derived coordinates and the model is asked nothing.
#[test]
#[allow(clippy::too_many_lines)]
fn a_claim_landing_on_the_injected_coordinate_is_reuse_without_shared_wording() {
    const ARTIFACT: &str = "app/src/ledger/ReplaySegment.kt";
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("reference reuse root");
    let checkout = temporary.path().join("reference reuse checkout");
    init_repo(&checkout, ARTIFACT, "// replay segment fixture\n");
    let checkout = fs::canonicalize(&checkout).unwrap();
    let (_, reused, reused_revision) = accepted_context(&root, REUSED_STATEMENT);
    let (_, ignored, _) = accepted_context(&root, IGNORED_STATEMENT);
    let repository_id = UserConfigStore::initialize(&root)
        .unwrap()
        .add_repository("Ledger".parse().unwrap(), std::slice::from_ref(&checkout))
        .unwrap()
        .repository
        .repository_id;
    engineering_reference_record_at_root(
        &root,
        &EngineeringReferenceRecordInput {
            context_id: reused.to_string(),
            revision_id: reused_revision.to_string(),
            repository_id: repository_id.to_string(),
            artifact_kind: ArtifactKind::File,
            relation: ReferenceRelation::Implements,
            locator: ArtifactLocator::File {
                path: RepoRelativePath::new(ARTIFACT).unwrap(),
            },
            supports: "The replay ordering is implemented in this file".to_owned(),
            limitations: vec!["Synthetic fixture".to_owned()],
        },
    )
    .unwrap();

    let session = "usage-reference-reuse";
    let locator = ExternalSessionLocator::new("codex", session).unwrap();
    let catalog = UserConfigStore::open_existing(&root)
        .unwrap()
        .repository_catalog_wait()
        .unwrap();
    AuthorizedSessionScopeStore::initialize(&root)
        .unwrap()
        .try_authorize_missing(&locator, &catalog, &checkout)
        .unwrap();

    let task = intent_update(&root, session);
    let injected = task
        .context
        .items
        .iter()
        .map(|item| item.context.context_id)
        .collect::<Vec<_>>();
    assert!(
        injected.contains(&reused) && injected.contains(&ignored),
        "{injected:?}"
    );

    // Nothing in this statement restates either injected Context; it only names the artifact.
    assert!(!checkpoint(
        &root,
        session,
        "ReplaySegment.kt drops the trailing partial frame before it hands the batch on",
        &[],
    ));

    let runtime = TaskRuntime::initialize(&root).unwrap();
    let totals = runtime.context_usage_totals(&[reused, ignored]).unwrap();
    assert_eq!(
        totals[&reused],
        ContextUsageTotals {
            reused: 1,
            ignored: 0,
            refuted: 0,
        },
        "the Claim landed on the coordinate this Context referenced"
    );
    assert_eq!(
        totals[&ignored],
        ContextUsageTotals {
            reused: 0,
            ignored: 1,
            refuted: 0,
        },
        "the sibling shares no coordinate and was never named"
    );
}
