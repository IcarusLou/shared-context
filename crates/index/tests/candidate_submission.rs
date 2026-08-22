use std::{
    fs,
    process::Command,
    sync::{
        Arc, Barrier,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use rusqlite::{Connection, params};
use sctx_domain::{
    Applicability, CandidateId, ContextKind, ContextRevisionDraft, EventId, EvidenceSnapshotDraft,
    EvidenceType, SubmissionId, TaskId, TaskSessionId, WorkEpisodeId, WorkEpisodeRef,
};
use sctx_event_schema::Event;
use sctx_git_store::{
    CandidateSubmissionIndex, CandidateSubmissionLookup, CandidateSubmissionRequest, CrashInjector,
    CrashSeam, Error, ErrorKind, GitStore, Result,
};
use sctx_index::{IndexUpdateKind, ProjectionIndex};

fn request(submission_id: SubmissionId, statement: &str) -> CandidateSubmissionRequest {
    CandidateSubmissionRequest {
        submission_id,
        source_episode: WorkEpisodeRef {
            episode_id: WorkEpisodeId::new(),
            task_session_id: TaskSessionId::new(),
            task_id: TaskId::new(),
        },
        content: ContextRevisionDraft {
            kind: ContextKind::Discovery,
            topic_key: Some("candidate/submission-idempotency".to_owned()),
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
                supports: "the submission transaction completed".to_owned(),
                content: serde_json::json!({"operation": "candidate_create"}),
                interpretation: "the Candidate is attributable to one operation".to_owned(),
                limitations: vec!["governance is outside Candidate creation".to_owned()],
            }],
        },
    }
}

fn configured_store() -> (tempfile::TempDir, GitStore, ProjectionIndex) {
    let temporary = tempfile::tempdir().unwrap();
    let base = GitStore::initialize(temporary.path().join("installation")).unwrap();
    let index = ProjectionIndex::for_store(&base);
    let store = base.with_candidate_submission_index(Arc::new(index.clone()));
    (temporary, store, index)
}

fn commit_raw_event(store: &GitStore, label: &str, value: &serde_json::Value) -> String {
    let relative = format!("events/aa/{label}.json");
    let path = store.repository().join(&relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
    let add = Command::new("git")
        .arg("-C")
        .arg(store.repository())
        .args(["add", "--", &relative])
        .output()
        .unwrap();
    assert!(
        add.status.success(),
        "{}",
        String::from_utf8_lossy(&add.stderr)
    );
    let commit = Command::new("git")
        .arg("-C")
        .arg(store.repository())
        .args(["commit", "-m", &format!("Add {label} fixture")])
        .output()
        .unwrap();
    assert!(
        commit.status.success(),
        "{}",
        String::from_utf8_lossy(&commit.stderr)
    );
    relative
}

fn malformed_candidate(submission_id: SubmissionId) -> serde_json::Value {
    serde_json::json!({
        "schema_version": "1",
        "event_type": "context_candidate.created",
        "event_id": EventId::new(),
        "candidate": {
            "candidate_id": CandidateId::new(),
            "submission_id": submission_id,
            "content": {"statement": "missing required Candidate fields"}
        }
    })
}

#[test]
fn malformed_candidate_hints_block_only_their_submission_and_rebuild_deterministically() {
    let (_temporary, store, index) = configured_store();
    index.synchronize().unwrap();

    let malformed_submission = SubmissionId::new();
    let malformed = malformed_candidate(malformed_submission);
    let malformed_event_id = malformed["event_id"].as_str().unwrap().to_owned();
    let malformed_candidate_id = malformed["candidate"]["candidate_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let malformed_path = commit_raw_event(&store, "malformed-candidate", &malformed);
    let incremental = index.synchronize().unwrap();
    assert_eq!(incremental.update_kind, IndexUpdateKind::Incremental);
    let expected = CandidateSubmissionIndex::lookup(&index, malformed_submission).unwrap();
    let CandidateSubmissionLookup::Conflict {
        event_ids,
        candidate_ids,
        content_hashes,
        ..
    } = &expected
    else {
        panic!("expected malformed submission conflict: {expected:?}");
    };
    assert_eq!(
        event_ids.iter().next().unwrap().to_string(),
        malformed_event_id
    );
    assert_eq!(
        candidate_ids.iter().next().unwrap().to_string(),
        malformed_candidate_id
    );
    assert!(content_hashes.is_empty());
    assert_eq!(
        store
            .submit_candidate(request(
                malformed_submission,
                "the malformed definition must block this operation",
            ))
            .unwrap_err()
            .kind(),
        ErrorKind::IdempotencyKeyConflict
    );

    let unrelated = store
        .submit_candidate(request(
            SubmissionId::new(),
            "an unrelated submission remains writable",
        ))
        .unwrap();
    assert!(unrelated.created());
    let connection = Connection::open(index.database_path()).unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT diagnostic_code FROM source_file WHERE path = ?1",
                [&malformed_path],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "EVENT_PARSE_ERROR"
    );
    drop(connection);

    fs::remove_file(index.database_path()).unwrap();
    index.synchronize().unwrap();
    assert_eq!(
        CandidateSubmissionIndex::lookup(&index, malformed_submission).unwrap(),
        expected
    );
}

#[test]
fn unknown_or_unidentifiable_bad_events_never_create_submission_conflicts() {
    let (_temporary, store, index) = configured_store();
    let unknown_submission = SubmissionId::new();
    commit_raw_event(
        &store,
        "unknown-candidate-schema",
        &serde_json::json!({
            "schema_version": "future",
            "event_type": "context_candidate.created",
            "event_id": EventId::new(),
            "candidate": {"submission_id": unknown_submission}
        }),
    );
    commit_raw_event(
        &store,
        "invalid-without-submission",
        &serde_json::from_str(include_str!(
            "../../../fixtures/events/v1/invalid/missing-required-field.json"
        ))
        .unwrap(),
    );
    let created = store
        .submit_candidate(request(
            unknown_submission,
            "unknown schema text cannot reserve a SubmissionId",
        ))
        .unwrap();
    assert!(created.created());
    let diagnostics = index.domain_snapshot().unwrap().diagnostics;
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "UNKNOWN_SCHEMA_VERSION")
    );
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "EVENT_PARSE_ERROR")
    );
}

#[test]
fn malformed_hint_merges_with_an_existing_valid_mapping_and_blocks_only_that_id() {
    let (_temporary, store, index) = configured_store();
    let submission_id = SubmissionId::new();
    let valid = store
        .submit_candidate(request(
            submission_id,
            "valid Candidate before malformed history",
        ))
        .unwrap();
    commit_raw_event(
        &store,
        "malformed-after-valid",
        &malformed_candidate(submission_id),
    );
    index.synchronize().unwrap();
    let conflict = CandidateSubmissionIndex::lookup(&index, submission_id).unwrap();
    let CandidateSubmissionLookup::Conflict {
        event_ids,
        candidate_ids,
        content_hashes,
        ..
    } = conflict
    else {
        panic!("expected merged valid/malformed conflict");
    };
    assert!(event_ids.contains(&valid.record.event_id));
    assert!(candidate_ids.contains(&valid.record.candidate_id));
    assert!(content_hashes.contains(&valid.record.content_hash));
    assert!(
        store
            .submit_candidate(request(
                SubmissionId::new(),
                "another submission bypasses the local conflict",
            ))
            .unwrap()
            .created()
    );
}

#[test]
fn twenty_concurrent_retries_converge_but_distinct_submissions_never_deduplicate() {
    let (_temporary, store, _index) = configured_store();
    let submission_id = SubmissionId::new();
    let request = request(submission_id, "twenty callers share one operation ID");
    let store = Arc::new(store);
    let barrier = Arc::new(Barrier::new(20));
    let handles = (0..20)
        .map(|_| {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            let request = request.clone();
            thread::spawn(move || {
                barrier.wait();
                store.submit_candidate(request).unwrap()
            })
        })
        .collect::<Vec<_>>();
    let outcomes = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        outcomes.iter().filter(|outcome| outcome.created()).count(),
        1
    );
    for outcome in &outcomes[1..] {
        assert_eq!(outcome.record, outcomes[0].record);
        assert_eq!(outcome.append.batch_id, outcomes[0].append.batch_id);
        assert_eq!(outcome.append.commit_oid, outcomes[0].append.commit_oid);
    }

    let mut conflicting = request.clone();
    conflicting.content.statement = "the same operation cannot change content".to_owned();
    let error = store.submit_candidate(conflicting).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::IdempotencyKeyConflict);

    let distinct = store
        .submit_candidate(CandidateSubmissionRequest {
            submission_id: SubmissionId::new(),
            ..request
        })
        .unwrap();
    assert!(distinct.created());
    assert_ne!(
        distinct.record.submission_id,
        outcomes[0].record.submission_id
    );
    assert_ne!(
        distinct.record.candidate_id,
        outcomes[0].record.candidate_id
    );
    assert_ne!(distinct.record.event_id, outcomes[0].record.event_id);
}

struct FailOnce {
    target: CrashSeam,
    seen: AtomicUsize,
}

impl FailOnce {
    const fn at(target: CrashSeam) -> Self {
        Self {
            target,
            seen: AtomicUsize::new(0),
        }
    }
}

impl CrashInjector for FailOnce {
    fn check(&self, seam: CrashSeam) -> Result<()> {
        if seam == self.target && self.seen.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(Error::new(
                ErrorKind::Io,
                format!("injected Candidate crash at {seam:?}"),
            ));
        }
        Ok(())
    }
}

#[test]
fn every_writer_crash_seam_recovers_one_submission_with_stable_batch_metadata() {
    for seam in [
        CrashSeam::AfterJournal,
        CrashSeam::BeforeCreate,
        CrashSeam::AfterCreate,
        CrashSeam::BeforeAdd,
        CrashSeam::AfterAdd,
        CrashSeam::BeforeCommit,
        CrashSeam::AfterCommit,
        CrashSeam::BeforeCommitOid,
        CrashSeam::AfterCommitOid,
        CrashSeam::BeforeIndex,
        CrashSeam::AfterIndex,
        CrashSeam::BeforeCleanup,
        CrashSeam::AfterCleanup,
    ] {
        let (_temporary, store, index) = configured_store();
        let request = request(
            SubmissionId::new(),
            &format!("recover Candidate submission after {seam:?}"),
        );
        let crashing = store
            .clone()
            .with_crash_injector(Arc::new(FailOnce::at(seam)));
        assert!(
            crashing.submit_candidate(request.clone()).is_err(),
            "{seam:?}"
        );

        let recovered = store.submit_candidate(request).unwrap();
        let original = recovered.record.clone();
        assert!(store.list_pending().unwrap().is_empty(), "{seam:?}");
        assert_eq!(original.batch_id, recovered.append.batch_id, "{seam:?}");
        assert_eq!(original.commit_oid, recovered.append.commit_oid, "{seam:?}");

        fs::remove_file(index.database_path()).unwrap();
        let rebuilt = store
            .submit_candidate(CandidateSubmissionRequest {
                submission_id: original.submission_id,
                source_episode: original.source_episode,
                content: request_content(&original, seam),
            })
            .unwrap();
        assert!(!rebuilt.created(), "{seam:?}");
        assert_eq!(rebuilt.record, original, "{seam:?}");
    }
}

fn request_content(
    record: &sctx_git_store::CandidateSubmissionRecord,
    seam: CrashSeam,
) -> ContextRevisionDraft {
    request(
        record.submission_id,
        &format!("recover Candidate submission after {seam:?}"),
    )
    .content
}

#[test]
fn indexed_retry_is_bounded_with_one_hundred_thousand_unrelated_events() {
    let (_temporary, store, index) = configured_store();
    let request = request(
        SubmissionId::new(),
        "indexed retry ignores unrelated Event population",
    );
    let created = store.submit_candidate(request.clone()).unwrap();

    let mut connection = Connection::open(index.database_path()).unwrap();
    let transaction = connection.transaction().unwrap();
    {
        let mut insert = transaction
            .prepare(
                "INSERT INTO source_file(path, blob_oid, parse_status, event_id, \
                 diagnostic_code, diagnostic_message, content) \
                 VALUES (?1, ?2, 'unknown_schema', NULL, 'fixture', 'unrelated', ?3)",
            )
            .unwrap();
        for ordinal in 0..100_000_u32 {
            insert
                .execute(params![
                    format!("events/ff/unrelated-{ordinal:06}.json"),
                    format!("{ordinal:040x}"),
                    b"{}".as_slice(),
                ])
                .unwrap();
        }
    }
    transaction.commit().unwrap();
    drop(connection);

    let started = Instant::now();
    let retried = store.submit_candidate(request).unwrap();
    let elapsed = started.elapsed();
    assert!(!retried.created());
    assert_eq!(retried.record, created.record);
    assert!(
        elapsed < Duration::from_secs(2),
        "indexed Candidate retry scanned unrelated Events: {elapsed:?}"
    );
}

#[test]
fn valid_duplicate_submission_events_project_a_typed_conflict_after_index_rebuild() {
    let (_temporary, store, index) = configured_store();
    let submission_id = SubmissionId::new();
    let first = request(submission_id, "first valid definition of one SubmissionId");
    let second = request(submission_id, "second valid definition of one SubmissionId");
    let first_event = Event::context_candidate_created(
        first.submission_id,
        first.source_episode,
        first.content,
        "bat_00000000-0000-4000-8000-000000000a01",
        None,
    )
    .unwrap();
    let second_event = Event::context_candidate_created(
        second.submission_id,
        second.source_episode,
        second.content,
        "bat_00000000-0000-4000-8000-000000000a02",
        None,
    )
    .unwrap();
    let first_relative = format!("events/fe/{}.json", first_event.event_id());
    let second_relative = format!("events/ff/{}.json", second_event.event_id());
    for (relative, event) in [
        (&first_relative, &first_event),
        (&second_relative, &second_event),
    ] {
        let path = store.repository().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, serde_json::to_vec_pretty(event).unwrap()).unwrap();
    }
    let add = Command::new("git")
        .arg("-C")
        .arg(store.repository())
        .args(["add", "--", &first_relative, &second_relative])
        .output()
        .unwrap();
    assert!(
        add.status.success(),
        "{}",
        String::from_utf8_lossy(&add.stderr)
    );
    let commit = Command::new("git")
        .arg("-C")
        .arg(store.repository())
        .args(["commit", "-m", "Add duplicate Candidate submission fixture"])
        .output()
        .unwrap();
    assert!(
        commit.status.success(),
        "{}",
        String::from_utf8_lossy(&commit.stderr)
    );

    index.synchronize().unwrap();
    let expected = CandidateSubmissionIndex::lookup(&index, submission_id).unwrap();
    let CandidateSubmissionLookup::Conflict {
        event_ids,
        candidate_ids,
        content_hashes,
        ..
    } = &expected
    else {
        panic!("expected duplicate SubmissionId conflict: {expected:?}");
    };
    assert_eq!(event_ids.len(), 2);
    assert_eq!(candidate_ids.len(), 2);
    assert_eq!(content_hashes.len(), 2);
    let connection = Connection::open(index.database_path()).unwrap();
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM candidate_submission", [], |row| row
                .get::<_, i64>(
                0
            ))
            .unwrap(),
        0
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT COUNT(*) FROM candidate_submission_conflict",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        1
    );
    drop(connection);

    fs::remove_file(index.database_path()).unwrap();
    index.synchronize().unwrap();
    assert_eq!(
        CandidateSubmissionIndex::lookup(&index, submission_id).unwrap(),
        expected
    );
}
