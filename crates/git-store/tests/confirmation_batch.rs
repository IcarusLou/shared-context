//! Atomicity of the multi-plan Candidate Confirmation writer.
//!
//! The Confirmation index is owned by the Index crate, which cannot be depended on from here, so
//! these tests supply a minimal `HEAD`-reading stand-in with the same observable contract: one
//! Confirmation per Candidate, and the Writer batch of a Confirmation Event names its own fact
//! closure.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
    str::FromStr,
    sync::Arc,
};

use sctx_domain::{
    CandidateConfirmationOperation, CandidateConfirmationPrimaryReference,
    CandidatePrimarySelection, ConflictParticipant, SemanticConflictOpeningDraft,
};
use sctx_event_schema::{
    Applicability, CandidateConfirmationPlan, CandidateId, ContextCandidate, ContextKind,
    ContextRevisionDraft, Event, EventId, EventPayload, EvidenceSnapshotDraft, EvidenceType,
    IntentSnapshot, OptionalCandidateEdits, ParsedEvent, SpaceId, SubmissionId, TaskId,
    TaskSessionId, TopicKeyEdit, WorkEpisodeId, WorkEpisodeRef, parse_event,
};
use sctx_git_store::{
    BatchId, CandidateConfirmationIndex, CandidateConfirmationLookup, CandidateConfirmationRecord,
    CandidateConfirmationWriteStatus, ErrorKind, GitStore, Result,
};
use tempfile::TempDir;

/// Confirmation index derived from committed `HEAD` Events only.
#[derive(Debug)]
struct HeadConfirmationIndex {
    repository: PathBuf,
}

impl CandidateConfirmationIndex for HeadConfirmationIndex {
    fn synchronize(&self) -> Result<()> {
        Ok(())
    }

    fn lookup(&self, candidate_id: CandidateId) -> Result<CandidateConfirmationLookup> {
        let events = head_events(&self.repository);
        let mut batches = BTreeMap::<String, Vec<(EventId, String)>>::new();
        for (path, event) in &events {
            if let Some(batch_id) = event.writer_batch_id() {
                batches
                    .entry(batch_id.to_owned())
                    .or_default()
                    .push((event.event_id(), path.clone()));
            }
        }
        let matched = events
            .iter()
            .filter(|(_, event)| match event.payload() {
                EventPayload::CandidateConfirmed { confirmation } => {
                    confirmation.candidate_id == candidate_id
                }
                _ => false,
            })
            .collect::<Vec<_>>();
        let [(path, event)] = matched.as_slice() else {
            if matched.is_empty() {
                return Ok(CandidateConfirmationLookup::NotFound);
            }
            return Ok(CandidateConfirmationLookup::Conflict {
                candidate_id,
                confirmation_ids: Vec::new(),
                event_ids: matched.iter().map(|(_, event)| event.event_id()).collect(),
            });
        };
        let EventPayload::CandidateConfirmed { confirmation } = event.payload() else {
            unreachable!("filtered to Confirmation Events")
        };
        let (operation_hash, plan_hash) = event
            .confirmation_hashes()
            .expect("Confirmation Event carries its server hashes");
        let batch_id = event
            .writer_batch_id()
            .expect("Confirmation Event carries its Writer batch");
        let mut batch = batches.get(batch_id).cloned().unwrap_or_default();
        batch.sort();
        Ok(CandidateConfirmationLookup::Found(
            CandidateConfirmationRecord {
                candidate_id,
                confirmation_id: confirmation.confirmation_id,
                result_context_id: confirmation.result_context_id,
                operation_hash: operation_hash.to_owned(),
                plan_hash: plan_hash.to_owned(),
                batch_id: BatchId::from_str(batch_id)?,
                commit_oid: introducing_commit(&self.repository, path),
                event_ids: batch.iter().map(|entry| entry.0).collect(),
                event_paths: batch.into_iter().map(|entry| entry.1).collect(),
            },
        ))
    }
}

fn head_events(repository: &Path) -> Vec<(String, Box<Event>)> {
    git(repository, &["ls-tree", "-r", "--name-only", "HEAD"])
        .lines()
        .filter(|path| path.starts_with("events/"))
        .filter_map(|path| {
            let bytes = std::fs::read(repository.join(path)).ok()?;
            match parse_event(&bytes) {
                Ok(ParsedEvent::Known(event)) => Some((path.to_owned(), event)),
                _ => None,
            }
        })
        .collect()
}

fn introducing_commit(repository: &Path, path: &str) -> String {
    git(
        repository,
        &["log", "--diff-filter=A", "--format=%H", "-1", "--", path],
    )
    .trim()
    .to_owned()
}

fn git(repository: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn commit_count(repository: &Path) -> usize {
    git(repository, &["rev-list", "--count", "HEAD"])
        .trim()
        .parse()
        .unwrap()
}

fn event_count(repository: &Path) -> usize {
    git(repository, &["ls-tree", "-r", "--name-only", "HEAD"])
        .lines()
        .filter(|path| path.starts_with("events/"))
        .count()
}

fn confirmation_store() -> (TempDir, GitStore, SpaceId) {
    let temporary = TempDir::new().unwrap();
    let store = GitStore::bootstrap_local(temporary.path().join("installation")).unwrap();
    let repository = store.repository().to_path_buf();
    let store =
        store.with_candidate_confirmation_index(Arc::new(HeadConfirmationIndex { repository }));
    let space = Event::space_created(
        IntentSnapshot {
            title: "Confirmation batch Space".to_owned(),
            problem: "Several reviewed Candidates need one atomic decision".to_owned(),
            desired_outcome: "Publish every confirmed Candidate together".to_owned(),
            in_scope: vec!["Candidate confirmation".to_owned()],
            out_of_scope: Vec::new(),
            acceptance_conditions: vec!["One Git commit per batch".to_owned()],
            domain_terms: Vec::new(),
        },
        None,
    )
    .unwrap();
    let EventPayload::SpaceCreated { space_id, .. } = space.payload() else {
        unreachable!()
    };
    let space_id = *space_id;
    store
        .append_event(sctx_git_store::AppendRequest::event(space))
        .unwrap();
    (temporary, store, space_id)
}

fn candidate(statement: &str) -> ContextCandidate {
    ContextCandidate::from_verified_submission(
        SubmissionId::new(),
        WorkEpisodeRef {
            episode_id: WorkEpisodeId::new(),
            task_session_id: TaskSessionId::new(),
            task_id: TaskId::new(),
        },
        ContextRevisionDraft {
            problem_view: None,
            hints: Vec::new(),
            kind: ContextKind::Discovery,
            topic_key: Some("candidate/confirmation-batch".to_owned()),
            statement: statement.to_owned(),
            rationale: "a closed Work Episode produced governable knowledge".to_owned(),
            applicability: Applicability {
                domains: vec!["candidate".to_owned()],
                platforms: Vec::new(),
                conditions: vec!["verified source Episode".to_owned()],
            },
            assumptions: Vec::new(),
            recheck_when: vec!["the Candidate is confirmed or discarded".to_owned()],
            relations: Vec::new(),
            evidence: vec![EvidenceSnapshotDraft {
                kind: EvidenceType::ExperimentRecord,
                supports: "the batch write was exercised".to_owned(),
                content: serde_json::json!({"operation": "candidate_confirm_batch"}),
                interpretation: "the Candidate is attributable to one operation".to_owned(),
                limitations: vec!["governance is outside Candidate creation".to_owned()],
            }],
        },
    )
    .unwrap()
}

fn plan(
    candidate: &ContextCandidate,
    space_id: SpaceId,
    edits: OptionalCandidateEdits,
) -> CandidateConfirmationPlan {
    conflicting_plan(candidate, space_id, edits, Vec::new())
}

fn conflicting_plan(
    candidate: &ContextCandidate,
    space_id: SpaceId,
    edits: OptionalCandidateEdits,
    conflicts: Vec<SemanticConflictOpeningDraft>,
) -> CandidateConfirmationPlan {
    CandidateConfirmationPlan::reserve(
        candidate,
        CandidateConfirmationOperation {
            candidate_id: candidate.candidate_id,
            review_parent_version: 1,
            analysis_generation: 1,
            primary: CandidateConfirmationPrimaryReference::ExistingSpace { space_id },
            related_space_ids: Vec::new(),
            edits,
        },
        CandidatePrimarySelection::Existing { space_id },
        Vec::new(),
        conflicts,
    )
    .unwrap()
}

fn head_event_type_count(repository: &Path, event_type: &str) -> usize {
    head_events(repository)
        .iter()
        .filter(|(_, event)| event.event_type().as_str() == event_type)
        .count()
}

/// Three valid plans become exactly one commit whose Events are the per-plan closures in order.
#[test]
fn confirmation_batch_writes_every_plan_in_one_commit_and_replays_as_already_exists() {
    let (_temporary, store, space_id) = confirmation_store();
    let candidates = ["first fact", "second fact", "third fact"]
        .map(candidate)
        .to_vec();
    let plans = candidates
        .iter()
        .map(|candidate| plan(candidate, space_id, OptionalCandidateEdits::default()))
        .collect::<Vec<_>>();

    let commits_before = commit_count(store.repository());
    let events_before = event_count(store.repository());
    let write = store.confirm_candidates(&plans).unwrap();

    assert_eq!(write.status, CandidateConfirmationWriteStatus::Created);
    assert_eq!(
        commit_count(store.repository()),
        commits_before + 1,
        "the whole batch is one Git commit"
    );
    let expected_events: usize = plans
        .iter()
        .map(CandidateConfirmationPlan::expected_event_count)
        .sum();
    assert_eq!(
        event_count(store.repository()),
        events_before + expected_events
    );

    assert_eq!(
        write
            .entries
            .iter()
            .map(|entry| entry.candidate_id)
            .collect::<Vec<_>>(),
        plans
            .iter()
            .map(|plan| plan.operation.candidate_id)
            .collect::<Vec<_>>(),
        "entries answer in input order"
    );
    let written = write.written.as_ref().expect("a created batch is written");
    assert_eq!(written.event_ids.len(), expected_events);
    assert_eq!(written.event_paths.len(), expected_events);

    // Each Candidate keeps its own fact closure, and the concatenation follows the input order.
    let mut offset = 0;
    for (plan, entry) in plans.iter().zip(&write.entries) {
        assert_eq!(entry.status, CandidateConfirmationWriteStatus::Created);
        assert_eq!(entry.record.event_ids.len(), plan.expected_event_count());
        assert_eq!(entry.append.commit_oid, written.commit_oid);
        assert_ne!(
            entry.append.batch_id, written.batch_id,
            "the Candidate Writer batch is not the Git journal batch"
        );
        assert_eq!(
            written.event_ids[offset..offset + plan.expected_event_count()],
            entry.record.event_ids[..]
        );
        offset += plan.expected_event_count();
    }
    assert_eq!(
        write
            .entries
            .iter()
            .map(|entry| entry.record.result_context_id)
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        3
    );

    // An identical replay writes nothing at all.
    let commits = commit_count(store.repository());
    let replay = store.confirm_candidates(&plans).unwrap();
    assert_eq!(
        replay.status,
        CandidateConfirmationWriteStatus::AlreadyExists
    );
    assert!(replay.written.is_none());
    assert!(
        replay
            .entries
            .iter()
            .all(|entry| entry.status == CandidateConfirmationWriteStatus::AlreadyExists)
    );
    assert_eq!(
        replay
            .entries
            .iter()
            .map(|entry| entry.record.confirmation_id)
            .collect::<Vec<_>>(),
        write
            .entries
            .iter()
            .map(|entry| entry.record.confirmation_id)
            .collect::<Vec<_>>()
    );
    assert_eq!(commit_count(store.repository()), commits);
    assert!(store.list_pending().unwrap().is_empty());
}

/// One rejected member in the middle writes no Event for any member.
#[test]
fn confirmation_batch_rejects_one_member_and_writes_nothing() {
    let (_temporary, store, space_id) = confirmation_store();
    let settled = candidate("a fact that was already confirmed");
    let settled_plan = plan(&settled, space_id, OptionalCandidateEdits::default());
    store.confirm_candidate(&settled_plan).unwrap();

    // The same Candidate, confirmed a second time with different semantics.
    let restated = plan(
        &settled,
        space_id,
        OptionalCandidateEdits {
            topic_key: Some(TopicKeyEdit::Set {
                value: "candidate/restated".to_owned(),
            }),
            ..OptionalCandidateEdits::default()
        },
    );
    let leading = candidate("a fact ahead of the rejected member");
    let trailing = candidate("a fact behind the rejected member");
    let batch = vec![
        plan(&leading, space_id, OptionalCandidateEdits::default()),
        restated,
        plan(&trailing, space_id, OptionalCandidateEdits::default()),
    ];

    let commits_before = commit_count(store.repository());
    let events_before = event_count(store.repository());
    let error = store.confirm_candidates(&batch).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Conflict, "{error}");
    assert!(
        error.message().contains("batch item 1")
            && error.message().contains(&settled.candidate_id.to_string())
            && error
                .message()
                .contains("no Candidate in this batch was written"),
        "the rejection names the exact member: {}",
        error.message()
    );
    assert_eq!(commit_count(store.repository()), commits_before);
    assert_eq!(event_count(store.repository()), events_before);
    assert!(store.list_pending().unwrap().is_empty());

    // The untouched members are still confirmable afterwards.
    let recovered = store
        .confirm_candidates(&[batch[0].clone(), batch[2].clone()])
        .unwrap();
    assert_eq!(recovered.status, CandidateConfirmationWriteStatus::Created);
    assert_eq!(commit_count(store.repository()), commits_before + 1);
}

/// A batch that mixes an identical replay with a new plan writes only the new plan.
#[test]
fn confirmation_batch_writes_only_the_members_that_are_not_already_confirmed() {
    let (_temporary, store, space_id) = confirmation_store();
    let confirmed = candidate("a fact confirmed on its own");
    let confirmed_plan = plan(&confirmed, space_id, OptionalCandidateEdits::default());
    let first = store.confirm_candidate(&confirmed_plan).unwrap();
    assert_eq!(first.status, CandidateConfirmationWriteStatus::Created);

    let fresh = candidate("a fact confirmed with the replay");
    let fresh_plan = plan(&fresh, space_id, OptionalCandidateEdits::default());
    let commits_before = commit_count(store.repository());
    let events_before = event_count(store.repository());
    let write = store
        .confirm_candidates(&[confirmed_plan.clone(), fresh_plan.clone()])
        .unwrap();

    assert_eq!(write.status, CandidateConfirmationWriteStatus::Created);
    assert_eq!(
        write.entries[0].status,
        CandidateConfirmationWriteStatus::AlreadyExists
    );
    assert_eq!(
        write.entries[0].record.confirmation_id,
        first.record.confirmation_id
    );
    assert_eq!(
        write.entries[1].status,
        CandidateConfirmationWriteStatus::Created
    );
    assert_eq!(commit_count(store.repository()), commits_before + 1);
    assert_eq!(
        event_count(store.repository()),
        events_before + fresh_plan.expected_event_count(),
        "only the new plan contributes Events"
    );
    let written = write.written.as_ref().unwrap();
    assert_eq!(written.event_ids.len(), fresh_plan.expected_event_count());
}

/// A repeated Candidate is a batch input error, not a silent second Confirmation.
#[test]
fn confirmation_batch_refuses_a_repeated_candidate_and_an_empty_slice() {
    let (_temporary, store, space_id) = confirmation_store();
    let repeated = candidate("a fact named twice in one batch");
    let repeated_plan = plan(&repeated, space_id, OptionalCandidateEdits::default());
    let error = store
        .confirm_candidates(&[repeated_plan.clone(), repeated_plan])
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidInput, "{error}");
    assert!(
        error.message().contains("batch item 1")
            && error.message().contains(&repeated.candidate_id.to_string()),
        "{}",
        error.message()
    );

    let empty = store.confirm_candidates(&[]).unwrap_err();
    assert_eq!(empty.kind(), ErrorKind::InvalidInput, "{empty}");
    assert!(store.list_pending().unwrap().is_empty());
}

/// A plan that opens a semantic conflict writes that Event exactly once inside the shared batch.
#[test]
fn confirmation_batch_opens_each_reserved_semantic_conflict_exactly_once() {
    let (_temporary, store, space_id) = confirmation_store();
    let settled = candidate("the accepted fact this batch contradicts");
    let settled_plan = plan(&settled, space_id, OptionalCandidateEdits::default());
    let accepted = store.confirm_candidate(&settled_plan).unwrap();
    assert_eq!(accepted.status, CandidateConfirmationWriteStatus::Created);

    let contradicting = candidate("a fact that contradicts the accepted one");
    let contradicting_plan = conflicting_plan(
        &contradicting,
        space_id,
        OptionalCandidateEdits::default(),
        vec![SemanticConflictOpeningDraft {
            target: ConflictParticipant {
                context_id: settled_plan.result_context_id,
                revision_id: settled_plan.result_revision.revision_id,
                publication_id: settled_plan.publication.publication_id,
            },
            reason: "the two statements cannot both hold".to_owned(),
        }],
    );
    let neighbour = candidate("an unrelated fact confirmed in the same batch");
    let neighbour_plan = plan(&neighbour, space_id, OptionalCandidateEdits::default());

    let conflicts_before = head_event_type_count(store.repository(), "semantic_conflict.opened");
    let commits_before = commit_count(store.repository());
    let write = store
        .confirm_candidates(&[contradicting_plan.clone(), neighbour_plan.clone()])
        .unwrap();

    assert_eq!(write.status, CandidateConfirmationWriteStatus::Created);
    assert_eq!(commit_count(store.repository()), commits_before + 1);
    assert_eq!(
        head_event_type_count(store.repository(), "semantic_conflict.opened"),
        conflicts_before + 1,
        "the batch opens the reserved conflict exactly once"
    );
    assert_eq!(
        write.entries[0].record.event_ids.len(),
        contradicting_plan.expected_event_count(),
        "the conflict Event belongs to its own Candidate closure"
    );
    assert_eq!(
        write.entries[1].record.event_ids.len(),
        neighbour_plan.expected_event_count()
    );
    assert_eq!(
        contradicting_plan.expected_event_count(),
        neighbour_plan.expected_event_count() + 1
    );

    let replay = store
        .confirm_candidates(&[contradicting_plan, neighbour_plan])
        .unwrap();
    assert_eq!(
        replay.status,
        CandidateConfirmationWriteStatus::AlreadyExists
    );
    assert_eq!(
        head_event_type_count(store.repository(), "semantic_conflict.opened"),
        conflicts_before + 1,
        "a batch replay opens nothing new"
    );
}
