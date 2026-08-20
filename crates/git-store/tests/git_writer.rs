use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
};

use sctx_event_schema::{
    Applicability, ContextKind, ContextRevisionDraft, Event, EventPayload, EvidenceSnapshotDraft,
    EvidenceType, IntentSnapshot, WorkEpisodeId,
};
use sctx_git_store::{
    AppendRequest, CrashInjector, CrashSeam, Error, ErrorKind, GitStore, OBJECT_PENDING,
    PendingFileKind, Result, TextObject,
};
use tempfile::TempDir;

#[derive(serde::Deserialize)]
struct PrivacyFixture {
    cases: Vec<PrivacyCase>,
}

#[derive(serde::Deserialize)]
struct PrivacyCase {
    kind: String,
    value: String,
}

fn privacy_fixture() -> PrivacyFixture {
    serde_json::from_str(include_str!(
        "../../../fixtures/privacy/common-sensitive.json"
    ))
    .unwrap()
}

struct Fixture {
    _temporary: TempDir,
    home: PathBuf,
    store: GitStore,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("temporary home 中文");
        fs::create_dir(&home).unwrap();
        let store = GitStore::initialize_for_home(&home).unwrap();
        Self {
            _temporary: temporary,
            home,
            store,
        }
    }

    fn git(&self, args: &[&str]) -> String {
        git(self.store.repository(), args)
    }
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
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn event(label: &str) -> Event {
    Event::space_created(
        IntentSnapshot {
            title: format!("Space {label}"),
            problem: "Repeated rediscovery".to_owned(),
            desired_outcome: "Durable context".to_owned(),
            in_scope: vec!["Git append writer".to_owned()],
            out_of_scope: vec!["CLI".to_owned()],
            acceptance_conditions: vec!["Committed exactly once".to_owned()],
            domain_terms: vec!["Batch".to_owned()],
        },
        None,
    )
    .unwrap()
}

fn candidate_event(source_episode_id: WorkEpisodeId, statement: &str) -> Event {
    Event::context_candidate_created(
        source_episode_id,
        ContextRevisionDraft {
            kind: ContextKind::Discovery,
            topic_key: None,
            statement: statement.to_owned(),
            rationale: "The append-once contract needs authoritative provenance".to_owned(),
            applicability: Applicability::default(),
            assumptions: Vec::new(),
            recheck_when: Vec::new(),
            evidence: vec![EvidenceSnapshotDraft {
                kind: EvidenceType::ExperimentRecord,
                supports: "The concurrent request completed".to_owned(),
                content: serde_json::json!({"result": "observed"}),
                interpretation: "The Writer serialized candidate creation".to_owned(),
                limitations: Vec::new(),
            }],
        },
        None,
    )
    .unwrap()
}

#[test]
fn git_append_boundary_rejects_sensitive_event_and_evidence_without_pending_residue() {
    let fixture = Fixture::new();
    let sensitive_event = event("alice@example.com");
    let event_error = fixture
        .store
        .append_event(AppendRequest::event(sensitive_event))
        .unwrap_err();
    let object_value = "password=correct-horse-battery-staple";
    let object_error = fixture
        .store
        .append_event(
            AppendRequest::event(event("safe")).with_object(TextObject::new(object_value)),
        )
        .unwrap_err();

    assert_eq!(event_error.kind(), ErrorKind::InvalidInput);
    assert!(event_error.message().contains("email_address"));
    assert!(!event_error.message().contains("alice@example.com"));
    assert_eq!(object_error.kind(), ErrorKind::InvalidInput);
    assert!(object_error.message().contains("assigned_credential"));
    assert!(!object_error.message().contains(object_value));
    assert!(fixture.store.list_pending().unwrap().is_empty());
    assert_eq!(fixture.git(&["rev-list", "--count", "HEAD"]), "1");
}

#[test]
fn git_append_boundary_rejects_every_shared_privacy_fixture() {
    let fixture = Fixture::new();

    for case in privacy_fixture().cases {
        let error = fixture
            .store
            .append_event(AppendRequest::event(event(&case.value)))
            .unwrap_err();

        assert_eq!(error.kind(), ErrorKind::InvalidInput, "{}", case.kind);
        assert!(
            error.message().contains(&case.kind),
            "{}: {error}",
            case.kind
        );
        assert!(!error.message().contains(&case.value), "{}", case.kind);
    }
    assert!(fixture.store.list_pending().unwrap().is_empty());
    assert_eq!(fixture.git(&["rev-list", "--count", "HEAD"]), "1");
}

#[test]
fn initialization_is_idempotent_and_uses_one_fixed_repository() {
    let fixture = Fixture::new();
    let reopened = GitStore::initialize_for_home(&fixture.home).unwrap();

    assert_eq!(fixture.store.repository(), reopened.repository());
    assert_eq!(
        reopened.repository(),
        fixture.home.join(".shared-context/repository")
    );
    assert_eq!(fixture.git(&["rev-list", "--count", "HEAD"]), "1");
    assert!(
        fixture
            .home
            .join(".shared-context/state/writer.lock")
            .is_file()
    );
}

#[test]
fn one_hundred_concurrent_appends_create_distinct_files_without_overwrite() {
    let fixture = Fixture::new();
    let store = Arc::new(fixture.store.clone());
    let outcomes = Arc::new(Mutex::new(Vec::new()));
    let mut threads = Vec::new();

    for index in 0..100 {
        let store = Arc::clone(&store);
        let outcomes = Arc::clone(&outcomes);
        threads.push(thread::spawn(move || {
            let outcome = store
                .append_event(AppendRequest::event(event(&index.to_string())))
                .unwrap();
            outcomes.lock().unwrap().push(outcome);
        }));
    }
    for handle in threads {
        handle.join().unwrap();
    }

    let outcomes = outcomes.lock().unwrap();
    let event_ids: HashSet<_> = outcomes.iter().map(|outcome| outcome.event_id).collect();
    let paths: HashSet<_> = outcomes.iter().map(|outcome| &outcome.event_path).collect();
    assert_eq!(event_ids.len(), 100);
    assert_eq!(paths.len(), 100);
    assert!(outcomes.iter().all(|outcome| {
        fixture
            .store
            .repository()
            .join(&outcome.event_path)
            .is_file()
    }));
    assert_eq!(fixture.git(&["status", "--porcelain"]), "");
    assert_eq!(fixture.git(&["rev-list", "--count", "HEAD"]), "101");
}

#[test]
fn identical_candidate_retries_converge_but_authoritative_differences_append() {
    let fixture = Fixture::new();
    let store = Arc::new(fixture.store.clone());
    let episode_id = WorkEpisodeId::new();
    let outcomes = Arc::new(Mutex::new(Vec::new()));
    let mut threads = Vec::new();

    for _ in 0..20 {
        let store = Arc::clone(&store);
        let outcomes = Arc::clone(&outcomes);
        threads.push(thread::spawn(move || {
            let outcome = store
                .append_candidate_once(AppendRequest::event(candidate_event(
                    episode_id,
                    "one authoritative discovery",
                )))
                .unwrap();
            outcomes.lock().unwrap().push(outcome);
        }));
    }
    for handle in threads {
        handle.join().unwrap();
    }

    let outcomes = outcomes.lock().unwrap();
    let event_ids = outcomes
        .iter()
        .map(|outcome| outcome.event.event_id())
        .collect::<HashSet<_>>();
    let candidate_ids = outcomes
        .iter()
        .map(|outcome| match outcome.event.payload() {
            EventPayload::ContextCandidateCreated { candidate } => candidate.candidate_id,
            _ => unreachable!(),
        })
        .collect::<HashSet<_>>();
    assert_eq!(event_ids.len(), 1);
    assert_eq!(candidate_ids.len(), 1);
    assert_eq!(outcomes.iter().filter(|outcome| outcome.created).count(), 1);
    drop(outcomes);

    let different = fixture
        .store
        .append_candidate_once(AppendRequest::event(candidate_event(
            episode_id,
            "a different authoritative discovery",
        )))
        .unwrap();
    assert!(different.created);
    assert_eq!(fixture.git(&["rev-list", "--count", "HEAD"]), "3");
}

#[test]
fn commit_contains_only_explicit_batch_paths_and_leaves_untracked_files_alone() {
    let fixture = Fixture::new();
    let unrelated = fixture.store.repository().join("unrelated note.txt");
    fs::write(&unrelated, "do not absorb me").unwrap();
    let outcome = fixture
        .store
        .append_event(
            AppendRequest::event(event("explicit-pathspec"))
                .with_object(TextObject::new("large evidence\n")),
        )
        .unwrap();

    let committed = fixture.git(&[
        "diff-tree",
        "--no-commit-id",
        "--name-only",
        "-r",
        &outcome.commit_oid,
    ]);
    let paths: HashSet<_> = committed.lines().collect();
    assert_eq!(paths.len(), 2);
    assert!(paths.contains(outcome.event_path.as_str()));
    assert!(paths.contains(outcome.objects[0].path.as_str()));
    assert_eq!(fs::read_to_string(unrelated).unwrap(), "do not absorb me");
    assert!(
        fixture
            .git(&["status", "--porcelain"])
            .contains("unrelated note.txt")
    );
}

#[test]
fn foreign_staged_path_is_rejected_without_changing_it() {
    let fixture = Fixture::new();
    let foreign = fixture.store.repository().join("foreign.txt");
    fs::write(&foreign, "user staged content").unwrap();
    fixture.git(&["add", "--", "foreign.txt"]);
    let before = fixture.git(&["rev-parse", "HEAD"]);

    let error = fixture
        .store
        .append_event(AppendRequest::event(event("foreign-stage")))
        .unwrap_err();

    assert_eq!(error.kind(), ErrorKind::InvariantViolation);
    assert!(error.message().contains("foreign staged"));
    assert_eq!(fixture.git(&["rev-parse", "HEAD"]), before);
    assert_eq!(fs::read_to_string(foreign).unwrap(), "user staged content");
    assert!(
        fixture
            .git(&["diff", "--cached", "--name-only"])
            .contains("foreign.txt")
    );
}

#[test]
fn modified_deleted_and_renamed_managed_files_are_each_rejected_untouched() {
    for mode in ["modified", "deleted", "renamed"] {
        let fixture = Fixture::new();
        let baseline = fixture
            .store
            .append_event(AppendRequest::event(event("baseline")))
            .unwrap();
        let path = fixture.store.repository().join(&baseline.event_path);
        let original = fs::read(&path).unwrap();
        let renamed_relative = format!("{}.moved", baseline.event_path);
        let renamed = fixture.store.repository().join(&renamed_relative);
        match mode {
            "modified" => fs::write(&path, b"user modification").unwrap(),
            "deleted" => fs::remove_file(&path).unwrap(),
            "renamed" => {
                fixture.git(&["mv", "--", &baseline.event_path, &renamed_relative]);
            }
            _ => unreachable!(),
        }
        let before = fixture.git(&["rev-parse", "HEAD"]);

        let error = fixture
            .store
            .append_event(AppendRequest::event(event(mode)))
            .unwrap_err();

        assert_eq!(error.kind(), ErrorKind::InvariantViolation, "{mode}");
        assert!(
            error.message().contains("append-only guard"),
            "{mode}: {error}"
        );
        assert_eq!(fixture.git(&["rev-parse", "HEAD"]), before);
        match mode {
            "modified" => assert_eq!(fs::read(&path).unwrap(), b"user modification"),
            "deleted" => assert!(!path.exists()),
            "renamed" => {
                assert!(!path.exists());
                assert_eq!(fs::read(&renamed).unwrap(), original);
            }
            _ => unreachable!(),
        }
    }
}

#[test]
fn object_is_reused_only_from_head_and_pending_worktree_object_is_rejected() {
    let fixture = Fixture::new();
    let first = fixture
        .store
        .append_event(
            AppendRequest::event(event("object-one")).with_object(TextObject::new("same evidence")),
        )
        .unwrap();
    let second = fixture
        .store
        .append_event(
            AppendRequest::event(event("object-two")).with_object(TextObject::new("same evidence")),
        )
        .unwrap();
    assert_eq!(first.objects, second.objects);
    assert_eq!(
        fixture.git(&[
            "diff-tree",
            "--no-commit-id",
            "--name-only",
            "-r",
            &second.commit_oid,
        ]),
        second.event_path
    );

    let pending_text = "not committed evidence";
    let digest = sha256(pending_text.as_bytes());
    let pending_path = fixture
        .store
        .repository()
        .join(format!("objects/sha256/{}/{digest}", &digest[..2]));
    fs::create_dir_all(pending_path.parent().unwrap()).unwrap();
    fs::write(&pending_path, pending_text).unwrap();
    let error = fixture
        .store
        .append_event(
            AppendRequest::event(event("pending-object"))
                .with_object(TextObject::new(pending_text)),
        )
        .unwrap_err();
    assert!(error.message().contains(OBJECT_PENDING), "{error}");
    assert_eq!(fs::read_to_string(pending_path).unwrap(), pending_text);
}

struct FailOnce {
    target: CrashSeam,
    seen: AtomicUsize,
}

impl FailOnce {
    fn at(target: CrashSeam) -> Self {
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
                format!("injected crash at {seam:?}"),
            ));
        }
        Ok(())
    }
}

#[test]
fn every_crash_seam_recovers_stable_content_with_at_most_one_semantic_commit() {
    let seams = [
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
    ];

    for seam in seams {
        let fixture = Fixture::new();
        let event = event(&format!("crash-{seam:?}"));
        let event_id = event.event_id();
        let expected = serde_json::to_value(&event).unwrap();
        let crashing = fixture
            .store
            .clone()
            .with_crash_injector(Arc::new(FailOnce::at(seam)));
        assert!(
            crashing
                .append_event(
                    AppendRequest::event(event)
                        .with_object(TextObject::new(format!("evidence for {seam:?}"))),
                )
                .is_err(),
            "{seam:?}"
        );

        let reopened = GitStore::initialize_for_home(&fixture.home).unwrap();
        reopened.recover_pending().unwrap();
        assert!(reopened.list_pending().unwrap().is_empty(), "{seam:?}");
        let matches = fixture.git(&["ls-tree", "-r", "--name-only", "HEAD", "--", "events"]);
        let event_path = matches
            .lines()
            .find(|path| path.contains(&event_id.to_string()))
            .unwrap_or_else(|| panic!("event missing after {seam:?}"));
        let stored: serde_json::Value =
            serde_json::from_slice(&fs::read(reopened.repository().join(event_path)).unwrap())
                .unwrap();
        assert_eq!(stored, expected, "{seam:?}");
        assert_eq!(
            fixture
                .git(&["log", "--format=%H", "--", event_path])
                .lines()
                .count(),
            1,
            "{seam:?}"
        );
        assert_eq!(fixture.git(&["status", "--porcelain"]), "", "{seam:?}");
    }
}

#[test]
fn pending_batches_support_explicit_commit_and_move_aside() {
    let fixture = Fixture::new();
    let crashing = fixture
        .store
        .clone()
        .with_crash_injector(Arc::new(FailOnce::at(CrashSeam::AfterJournal)));
    crashing
        .append_event(AppendRequest::event(event("explicit-commit")))
        .unwrap_err();
    let pending = fixture.store.list_pending().unwrap();
    assert_eq!(pending.len(), 1);
    let committed = fixture.store.commit_pending(&pending[0].batch_id).unwrap();
    assert_eq!(committed.event_id.to_string(), pending[0].event_id);

    let crashing = fixture
        .store
        .clone()
        .with_crash_injector(Arc::new(FailOnce::at(CrashSeam::AfterJournal)));
    crashing
        .append_event(AppendRequest::event(event("move-aside")))
        .unwrap_err();
    let pending = fixture.store.list_pending().unwrap();
    assert_eq!(pending.len(), 1);
    let destination = fixture
        .store
        .move_pending_aside(&pending[0].batch_id)
        .unwrap();
    assert!(destination.join("journal.json").is_file());
    assert!(fixture.store.list_pending().unwrap().is_empty());
}

#[test]
fn recovery_rejects_missing_payload_and_a_partially_committed_batch() {
    let fixture = Fixture::new();
    let crashing = fixture
        .store
        .clone()
        .with_crash_injector(Arc::new(FailOnce::at(CrashSeam::AfterJournal)));
    crashing
        .append_event(AppendRequest::event(event("missing-payload")))
        .unwrap_err();
    let pending = fixture.store.list_pending().unwrap();
    let payload = fixture
        .store
        .state()
        .join("pending")
        .join(pending[0].batch_id.as_str())
        .join("files")
        .join(&pending[0].files[0].payload_file);
    fs::remove_file(payload).unwrap();
    let error = fixture
        .store
        .commit_pending(&pending[0].batch_id)
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io);
    fixture
        .store
        .move_pending_aside(&pending[0].batch_id)
        .unwrap();

    let crashing = fixture
        .store
        .clone()
        .with_crash_injector(Arc::new(FailOnce::at(CrashSeam::BeforeCommit)));
    crashing
        .append_event(
            AppendRequest::event(event("partial-head"))
                .with_object(TextObject::new("partial object")),
        )
        .unwrap_err();
    let pending = fixture.store.list_pending().unwrap();
    let object_path = pending[0]
        .files
        .iter()
        .find(|file| file.kind == PendingFileKind::Object)
        .unwrap()
        .target_path
        .clone();
    fixture.git(&["reset"]);
    fixture.git(&["add", "--", &object_path]);
    fixture.git(&["commit", "-m", "manual partial commit", "--", &object_path]);

    let error = fixture
        .store
        .commit_pending(&pending[0].batch_id)
        .unwrap_err();
    assert!(
        error.message().contains("partially present in HEAD"),
        "{error}"
    );
}

fn sha256(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    digest
        .iter()
        .fold(String::with_capacity(64), |mut output, byte| {
            write!(output, "{byte:02x}").unwrap();
            output
        })
}
