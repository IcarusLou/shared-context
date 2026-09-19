//! Server-recorded injection results and the usage prior they feed back into retrieval.
//!
//! Nothing here is Agent-authored: the Checkpoint contract stays the four public fields, and every
//! usage outcome is derived from text the Agent wrote for its own purpose.

use std::{fs, path::Path, process::Command};

use sctx_domain::{
    Applicability, ArtifactKind, ArtifactLocator, CandidateId, ContextId, ContextKind,
    ContextRelation, ContextRelationKind, ContextRevisionDraft, DecisionSource,
    EvidenceSnapshotDraft, EvidenceType, ExternalSessionLocator, IntentSnapshot,
    OptionalCandidateEdits, PublicationAction, PublicationDraft, ReferenceRelation,
    RepoRelativePath, ReviewDraft, ReviewVerdict, RevisionId, SpaceId, WorkingIntentSnapshot,
};
use sctx_event_schema::{Event, EventPayload};
use sctx_git_store::{AppendRequest, GitStore};
use sctx_local_state::{AuthorizedSessionScopeStore, UserConfigStore};
use sctx_mcp::{
    CandidateAnalyzeInput, CandidateConfirmInput, CandidateConfirmPrimaryInput, CandidateListInput,
    EngineeringReferenceRecordInput, ExistingCandidatePrimaryInput, ExpectedRevisionId,
    TaskBoundary, TaskCheckpointClaimInput, TaskCheckpointEvidenceInput, TaskCheckpointInput,
    TaskContextReadInput, TaskIntentUpdateInput, build_closed_episode_at_root,
    candidate_analyze_at_root, candidate_confirm_at_root, candidate_list_at_root,
    engineering_reference_record_at_root, task_checkpoint_at_root,
    task_context_readonly_with_detail_at_root, task_intent_update_at_root,
};
use sctx_search::{ContextPackDetailLevel, SemanticChannelHandle};
use sctx_task_runtime::{ContextInjectionSource, ContextUsageTotals, TaskRuntime};

const REUSED_STATEMENT: &str =
    "quorum ledger replay preserves deterministic ordering across restarts";
const IGNORED_STATEMENT: &str = "quorum ledger replay tolerates truncated segments during recovery";
const GOAL: &str = "quorum ledger replay";

/// The file every fixture Context here is recorded against, and the Working Intent hint that
/// reaches it.
///
/// ADR-0007 retrieves by anchor: a Context is injected because the Session touched a file it is
/// recorded against. None of these tests is about retrieval -- they are about what the server
/// records *after* an injection, and how that record feeds back -- so they need the cheapest honest
/// anchor there is. An `artifact_hints` spelling is exactly that, and it is the one anchor source
/// an MCP caller can supply without a Hook: a field of the Working Intent the Agent already writes.
const LEDGER_FILE: &str = "src/ledger/replay.ts";

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
    anchor_to_ledger_file(&store, context_id, revision_id);
    (space_id, context_id, revision_id)
}

/// Records the Engineering Reference that makes one Context reachable by Lane A.
fn anchor_to_ledger_file(store: &GitStore, context_id: ContextId, revision_id: RevisionId) {
    store
        .append_event(AppendRequest::event(
            Event::engineering_reference_recorded(
                context_id,
                revision_id,
                sctx_domain::EngineeringReferenceDraft {
                    repository_id: "Ledger".parse().unwrap(),
                    artifact_kind: ArtifactKind::File,
                    relation: ReferenceRelation::Implements,
                    locator: ArtifactLocator::File {
                        path: RepoRelativePath::new(LEDGER_FILE).unwrap(),
                    },
                    supports: "the usage fixture anchors this Context to the replay file"
                        .to_owned(),
                    limitations: vec!["Synthetic fixture".to_owned()],
                },
                None,
            )
            .unwrap(),
        ))
        .unwrap();
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
                artifact_hints: vec![LEDGER_FILE.to_owned()],
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
        use std::fmt::Write as _;
        write!(rationale, " (building on {context_id})").unwrap();
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
fn strong_usage_stays_visible_without_reweighting_or_rank_reasons() {
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

    for index in 0..2 {
        let session = format!("usage-prior-more-{index}");
        intent_update(&root, &session);
        assert!(!checkpoint(&root, &session, REUSED_STATEMENT, &[reused]));
    }
    let second = intent_update(&root, "usage-prior-second");
    let full = second
        .context
        .items
        .iter()
        .find(|item| item.context.context_id == reused)
        .expect("the reused Context is still retrievable");
    assert_eq!(full.context.usage.reused, 3);
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
    assert_eq!(
        order,
        first
            .context
            .items
            .iter()
            .map(|item| item.context.context_id)
            .collect::<Vec<_>>()
    );
    let mut normalized = second.context.items.clone();
    for item in &mut normalized {
        item.context.usage = sctx_search::ContextUsageCounts::default();
    }
    assert_eq!(
        normalized, first.context.items,
        "only truthful usage counters may change"
    );
    let ignored_item = second
        .context
        .items
        .iter()
        .find(|item| item.context.context_id == ignored)
        .unwrap();
    assert_eq!(ignored_item.context.usage.ignored, 3);
    assert!(compact.compact_items.iter().all(|item| {
        item.why
            .iter()
            .all(|reason| !reason.contains("Reused in") && !reason.contains("Ignored in"))
    }));
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
            decision_source: DecisionSource::Human,
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

/// Reuse follows the server's own published relation verdict, not a similarity score.
///
/// The comparison the two Claim signals replaced scored normalized statement token Jaccard inside
/// the usage path itself, so a Claim that repeated its input scored highest while the Claim that
/// carried a genuinely *new* conclusion built on that input scored nothing. That rule is gone and
/// stays gone: nothing here measures wording.
///
/// What decides a restatement now is the Candidate analysis, which the review pipeline publishes
/// for its own reasons -- it gates Confirmation, routes duplicate review, and is the same verdict
/// a human reviewer reads. When that verdict relates the Candidate to a Context this very Task was
/// handed, the server has already told itself the injection landed on target, and recording the
/// injection as an omission contradicts its own analysis. Session `3f862e48` is the proof: the
/// stored assessment read `supports ctx_d9689ac4` and all three injections were filed `ignored`.
///
/// The sibling keeps the ordering honest. It was injected into the same Task, it is a sentence
/// about the same subsystem, and no assessment related the Candidate to it, so it stays `ignored`:
/// the decision is the relation, not the topic and not the wording.
#[test]
fn a_restatement_is_reuse_only_because_the_analysis_relates_it_to_the_injection() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("restatement root");
    let (_, reused, _) = accepted_context(&root, REUSED_STATEMENT);
    let (_, ignored, _) = accepted_context(&root, IGNORED_STATEMENT);
    let session = "usage-restatement";

    intent_update(&root, session);
    // The Claim repeats the injected statement word for word and names nothing: neither Claim
    // signal fires, and the Working Intent quotes no identifier either.
    assert!(!checkpoint(&root, session, REUSED_STATEMENT, &[]));

    let runtime = TaskRuntime::initialize(&root).unwrap();
    let totals = runtime.context_usage_totals(&[reused, ignored]).unwrap();
    assert_eq!(
        totals[&reused],
        ContextUsageTotals {
            reused: 1,
            ignored: 0,
            refuted: 0,
        },
        "the analysis related this Candidate to the injected Context"
    );
    assert_eq!(
        totals[&ignored],
        ContextUsageTotals {
            reused: 0,
            ignored: 1,
            refuted: 0,
        },
        "no assessment related the Candidate to the sibling, so it stays an omission"
    );

    // Replaying the Checkpoint re-derives the two Claim signals, which both say `ignored` here.
    // A rerun must not walk the analysis verdict back.
    assert!(checkpoint(&root, session, REUSED_STATEMENT, &[]));
    assert_eq!(
        runtime.context_usage_totals(&[reused, ignored]).unwrap(),
        totals,
        "a Candidate Build rerun re-derives the early signals and must not erase the late one"
    );
}

/// A Context the Working Intent quotes by identity is reuse, whatever the Claims end up saying.
///
/// Session `01a06646` is the shape: `ctx_be6db0a4` was pasted whole into the Task's
/// `current_direction`, the model said it was correcting its implementation accordingly, and the
/// Claims it finally filed were about newly created files that shared no coordinate with the
/// Context's own references. Every Claim signal missed, and the Context that visibly steered the
/// Task was recorded as an omission.
///
/// The Intent is Agent-authored prose written for the Agent's own purpose, exactly like a Claim
/// statement, and the server asks for no new field to read it.
#[test]
fn a_context_quoted_into_the_working_intent_is_reuse() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("intent quote root");
    let (_, quoted, _) = accepted_context(&root, REUSED_STATEMENT);
    let (_, ignored, _) = accepted_context(&root, IGNORED_STATEMENT);
    let session = "usage-intent-quote";

    let task = intent_update(&root, session);
    let injected = task
        .context
        .items
        .iter()
        .map(|item| item.context.context_id)
        .collect::<Vec<_>>();
    assert!(
        injected.contains(&quoted) && injected.contains(&ignored),
        "{injected:?}"
    );

    // The Agent revises its direction around one of the two Contexts it was handed.
    let revised = task_intent_update_at_root(
        &root,
        &TaskIntentUpdateInput {
            agent_kind: "codex".to_owned(),
            external_session_id: session.to_owned(),
            task_boundary: TaskBoundary::Continue,
            expected_revision_id: ExpectedRevisionId::Revision(
                task.context.intent_revision_id.to_string(),
            ),
            intent: WorkingIntentSnapshot {
                goal: GOAL.to_owned(),
                current_direction: Some(format!(
                    "Correcting the implementation against {quoted}, which pins the ordering"
                )),
                in_scope: vec![GOAL.to_owned()],
                out_of_scope: Vec::new(),
                domains: vec!["ledger".to_owned()],
                platforms: Vec::new(),
                constraints: Vec::new(),
                acceptance_conditions: vec!["The Pack is returned".to_owned()],
                artifact_hints: vec![LEDGER_FILE.to_owned()],
                interface_hints: Vec::new(),
                open_questions: Vec::new(),
            },
        },
    )
    .unwrap();
    assert_eq!(revised.context.task_id, task.context.task_id);

    // The Claim is about something else entirely: it names no Context, restates nothing, and
    // lands on no shared coordinate.
    assert!(!checkpoint(
        &root,
        session,
        "the release pipeline pins its toolchain to one minor version per branch",
        &[],
    ));

    let runtime = TaskRuntime::initialize(&root).unwrap();
    let totals = runtime.context_usage_totals(&[quoted, ignored]).unwrap();
    assert_eq!(
        totals[&quoted],
        ContextUsageTotals {
            reused: 1,
            ignored: 0,
            refuted: 0,
        },
        "the Task wrote this Context's identity into its own direction"
    );
    assert_eq!(
        totals[&ignored],
        ContextUsageTotals {
            reused: 0,
            ignored: 1,
            refuted: 0,
        },
        "the sibling appears in no Intent revision and in no Claim"
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

/// Reads the one pending Candidate this Task produced.
fn only_candidate(root: &Path, session: &str) -> CandidateId {
    candidate_list_at_root(
        root,
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
    .candidate_id
}

/// Re-analyzing a Candidate rewrites the same reuse verdict instead of a second one.
///
/// Analysis is produced from three entry points -- Candidate Build's deferred pass,
/// `candidate_analyze`, and the recovery drain behind `candidate_get` -- and every one of them
/// records the assessment signal. A Context is credited once per Task however many of them run.
#[test]
fn re_analyzing_a_candidate_does_not_double_count_the_reuse() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("re-analysis root");
    let (_, reused, _) = accepted_context(&root, REUSED_STATEMENT);
    let (_, ignored, _) = accepted_context(&root, IGNORED_STATEMENT);
    let session = "usage-re-analysis";

    intent_update(&root, session);
    assert!(!checkpoint(&root, session, REUSED_STATEMENT, &[]));

    let runtime = TaskRuntime::initialize(&root).unwrap();
    let before = runtime.context_usage_totals(&[reused, ignored]).unwrap();
    assert_eq!(
        before[&reused],
        ContextUsageTotals {
            reused: 1,
            ignored: 0,
            refuted: 0,
        }
    );

    let analyzed = candidate_analyze_at_root(
        &root,
        &CandidateAnalyzeInput {
            candidate_id: only_candidate(&root, session).to_string(),
            token_budget: 8_192,
            top_k: 8,
        },
    )
    .unwrap();
    assert!(
        analyzed
            .candidate
            .analysis
            .assessments
            .iter()
            .any(|assessment| assessment
                .target
                .is_some_and(|target| target.context_id == reused)),
        "the analysis this test reasons about must actually target the injected Context"
    );
    assert_eq!(
        runtime.context_usage_totals(&[reused, ignored]).unwrap(),
        before,
        "one (context, task) pair contributes one row however often the analysis is rebuilt"
    );
}

/// Publishes one more accepted Context into an existing Space.
fn sibling_context(root: &Path, space_id: SpaceId, statement: &str) -> ContextId {
    let store = GitStore::bootstrap_local(root).unwrap();
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
    anchor_to_ledger_file(&store, context_id, revision_id);
    context_id
}

/// The analysis signal is the *relation*, not the neighbourhood the Context was retrieved from.
///
/// Both Contexts here live in one Space, both were injected into this Task, and the Candidate is
/// recommended into that same Space. Only one of them stands in an assessed relation to the
/// Candidate, and only that one is credited. This is why `unresolved_related` is excluded from the
/// signal by construction: "retrieval found this nearby" is what injection already means, and
/// crediting it would mark every analyzed injection reused and leave the prior saying nothing.
#[test]
fn an_injected_neighbour_the_analysis_never_relates_to_stays_an_omission() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("neighbour root");
    let (space, related, _) = accepted_context(&root, REUSED_STATEMENT);
    let neighbour = sibling_context(
        &root,
        space,
        "the retention sweeper prunes archived snapshots older than ninety days",
    );
    let session = "usage-neighbour";

    let injected = intent_update(&root, session)
        .context
        .items
        .iter()
        .map(|item| item.context.context_id)
        .collect::<Vec<_>>();
    assert!(
        injected.contains(&related) && injected.contains(&neighbour),
        "both Space members must reach this Task: {injected:?}"
    );
    assert!(!checkpoint(&root, session, REUSED_STATEMENT, &[]));

    let analyzed = candidate_analyze_at_root(
        &root,
        &CandidateAnalyzeInput {
            candidate_id: only_candidate(&root, session).to_string(),
            token_budget: 8_192,
            top_k: 8,
        },
    )
    .unwrap();
    let assessed = analyzed
        .candidate
        .analysis
        .assessments
        .iter()
        .filter_map(|assessment| assessment.target.map(|target| target.context_id))
        .collect::<Vec<_>>();
    assert!(assessed.contains(&related), "{assessed:?}");
    assert!(!assessed.contains(&neighbour), "{assessed:?}");

    let runtime = TaskRuntime::initialize(&root).unwrap();
    let totals = runtime.context_usage_totals(&[related, neighbour]).unwrap();
    assert_eq!(
        totals[&related],
        ContextUsageTotals {
            reused: 1,
            ignored: 0,
            refuted: 0,
        }
    );
    assert_eq!(
        totals[&neighbour],
        ContextUsageTotals {
            reused: 0,
            ignored: 1,
            refuted: 0,
        },
        "sharing a Space with a credited Context proves nothing about this one"
    );
}

/// A Confirmation asks for a vector for the knowledge it just accepted.
///
/// The corpus backfill runs once per `serve` process, at model-load time, so until now every
/// Context a session produced was invisible to the semantic lane for the rest of that process.
/// Measured on a real 21.8-hour Session: `index.context_revision` held 32 rows and
/// `semantic.revision_vector` held 28, and the four missing ones were exactly the accepted
/// revisions of the three Contexts that Session had just written.
///
/// The channel handle is the queue, because it is the one object the loader thread and the request
/// path already share. The ask is advisory in both directions -- a handle nobody is filling only
/// records names, and the Confirmation neither waits for nor fails on a discardable cache.
#[test]
fn confirming_a_candidate_asks_the_semantic_corpus_for_the_revision_it_just_accepted() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("semantic backfill root");
    let (space_id, _, _) = accepted_context(&root, REUSED_STATEMENT);
    let session = "semantic-backfill";

    let task = intent_update(&root, session);
    checkpoint(&root, session, IGNORED_STATEMENT, &[]);
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

    let semantic = SemanticChannelHandle::new();
    assert!(
        semantic.pending_backfill().is_empty(),
        "nothing is owed before a Confirmation"
    );
    let confirmed = sctx_mcp::candidate_confirm_at_root_with_semantic_channel(
        &root,
        &CandidateConfirmInput {
            decision_source: DecisionSource::Human,
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
            edits: OptionalCandidateEdits::default(),
        },
        semantic.clone(),
    )
    .unwrap();

    assert_eq!(
        semantic.pending_backfill(),
        std::collections::BTreeSet::from([confirmed.revision_id]),
        "the accepted revision, and only it, is what the corpus is missing"
    );
    // Taking the set is what a filler does, and it leaves nothing behind: a revision requested
    // while a fill is running has to wake the next pass, not be swallowed by the current one.
    assert_eq!(
        semantic.take_backfill_requests(),
        std::collections::BTreeSet::from([confirmed.revision_id])
    );
    assert!(semantic.pending_backfill().is_empty());
}
