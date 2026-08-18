use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};

use fs2::FileExt;
use sctx_domain::{Error, ErrorKind, EventId, Result};
use sctx_event_schema::{Event, ParsedEvent, parse_event};
use sha2::{Digest, Sha256};

use crate::{
    OBJECT_PENDING,
    git::Git,
    journal::{BatchId, Journal, JournalPhase, PendingBatch, PendingFile, PendingFileKind},
};

const JOURNAL_VERSION: u32 = 1;
const MANAGED_ROOTS: [&str; 3] = ["events", "objects", "schemas"];

/// One UTF-8 content-addressed evidence object to append with an event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextObject {
    pub text: String,
}

impl TextObject {
    /// Creates a UTF-8 text object.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

/// Append input. Paths and IDs are intentionally absent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppendRequest {
    pub event: Event,
    pub objects: Vec<TextObject>,
}

impl AppendRequest {
    /// Creates an event-only batch.
    #[must_use]
    pub const fn event(event: Event) -> Self {
        Self {
            event,
            objects: Vec::new(),
        }
    }

    /// Adds a text object to the same atomic batch as the event.
    #[must_use]
    pub fn with_object(mut self, object: TextObject) -> Self {
        self.objects.push(object);
        self
    }
}

/// Stable reference returned for a content-addressed object.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObjectRef {
    pub sha256: String,
    pub path: String,
    pub size: u64,
}

/// Successful append/recovery result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppendOutcome {
    pub batch_id: BatchId,
    pub event_id: EventId,
    pub event_path: String,
    pub commit_oid: String,
    pub objects: Vec<ObjectRef>,
    pub recovered: bool,
}

/// Durable boundaries exposed for deterministic crash testing.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CrashSeam {
    AfterJournal,
    BeforeCreate,
    AfterCreate,
    BeforeAdd,
    AfterAdd,
    BeforeCommit,
    AfterCommit,
    BeforeCommitOid,
    AfterCommitOid,
    BeforeIndex,
    AfterIndex,
    BeforeCleanup,
    AfterCleanup,
}

/// Injects process-equivalent failures at durable writer boundaries.
pub trait CrashInjector: Send + Sync {
    /// Returns an error to simulate a crash at `seam`.
    ///
    /// # Errors
    ///
    /// An error represents abrupt termination at the selected durable boundary.
    fn check(&self, seam: CrashSeam) -> Result<()>;
}

/// Default injector used in production.
#[derive(Debug, Default)]
pub struct NoopCrashInjector;

impl CrashInjector for NoopCrashInjector {
    fn check(&self, _seam: CrashSeam) -> Result<()> {
        Ok(())
    }
}

/// Hook for the rebuildable index to observe a committed `HEAD`.
///
/// Recovery may call the observer more than once for the same or a later
/// `HEAD`, so implementations must be idempotent and synchronize derived state
/// from Git rather than treating the callback as an exactly-once event.
pub trait CommitObserver: Send + Sync {
    /// Updates derived state after Git has committed the batch.
    ///
    /// # Errors
    ///
    /// Returns an error when derived-state synchronization cannot complete.
    fn committed(&self, repository: &Path, commit_oid: &str) -> Result<()>;
}

/// Default observer for deployments where indexing is performed lazily.
#[derive(Debug, Default)]
pub struct NoopCommitObserver;

impl CommitObserver for NoopCommitObserver {
    fn committed(&self, _repository: &Path, _commit_oid: &str) -> Result<()> {
        Ok(())
    }
}

/// The unique append-only store rooted at one installation directory.
#[derive(Clone)]
pub struct GitStore {
    root: PathBuf,
    repository: PathBuf,
    state: PathBuf,
    crash: Arc<dyn CrashInjector>,
    observer: Arc<dyn CommitObserver>,
}

impl GitStore {
    /// Initializes or opens the unique store under `home/.shared-context`.
    ///
    /// # Errors
    ///
    /// Returns an error if the fixed repository path is not a valid standalone
    /// Git worktree or if initialization cannot be completed.
    pub fn initialize_for_home(home: impl AsRef<Path>) -> Result<Self> {
        Self::initialize(home.as_ref().join(".shared-context"))
    }

    /// Initializes or opens the unique store at an explicit installation root.
    ///
    /// # Errors
    ///
    /// Returns an error if filesystem or Git initialization fails.
    pub fn initialize(root: impl AsRef<Path>) -> Result<Self> {
        let root = absolute(root.as_ref())?;
        let state = root.join("state");
        let repository = root.join("repository");
        fs::create_dir_all(state.join("pending")).map_err(io_error("create pending root"))?;
        fs::create_dir_all(&state).map_err(io_error("create state root"))?;
        let lock = open_lock(&state.join("writer.lock"))?;
        lock.lock_exclusive()
            .map_err(io_error("lock writer.lock"))?;

        if repository.exists() {
            verify_repository(&repository)?;
        } else {
            fs::create_dir_all(&repository).map_err(io_error("create repository"))?;
            let output = std::process::Command::new("git")
                .args(["init", "--initial-branch=main"])
                .arg(&repository)
                .output()
                .map_err(|error| {
                    Error::new(
                        ErrorKind::External,
                        format!("failed to execute git init: {error}"),
                    )
                })?;
            if !output.status.success() {
                return Err(Error::new(
                    ErrorKind::External,
                    format!(
                        "git init failed: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    ),
                ));
            }
            let git = Git::new(&repository);
            git.run(["config", "user.name", "Shared Context Writer"])?;
            git.run(["config", "user.email", "shared-context@localhost"])?;
            for directory in MANAGED_ROOTS {
                fs::create_dir_all(repository.join(directory))
                    .map_err(io_error("create managed repository directory"))?;
            }
            git.run([
                "commit",
                "--allow-empty",
                "-m",
                "Initialize Shared Context repository",
            ])?;
        }
        FileExt::unlock(&lock).map_err(io_error("unlock writer.lock"))?;

        Ok(Self {
            root,
            repository,
            state,
            crash: Arc::new(NoopCrashInjector),
            observer: Arc::new(NoopCommitObserver),
        })
    }

    /// Replaces the crash injector, primarily for seam testing.
    #[must_use]
    pub fn with_crash_injector(mut self, injector: Arc<dyn CrashInjector>) -> Self {
        self.crash = injector;
        self
    }

    /// Replaces the derived-index observer.
    #[must_use]
    pub fn with_commit_observer(mut self, observer: Arc<dyn CommitObserver>) -> Self {
        self.observer = observer;
        self
    }

    /// Installation root managed by this store.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Unique Git repository managed by this store.
    #[must_use]
    pub fn repository(&self) -> &Path {
        &self.repository
    }

    /// State directory containing locks and pending journals.
    #[must_use]
    pub fn state(&self) -> &Path {
        &self.state
    }

    /// Appends one immutable event and its text objects in one Git commit.
    ///
    /// # Errors
    ///
    /// Rejects invalid payloads, append-only violations, foreign staged paths,
    /// pending object collisions, Git failures, and injected crash seams.
    pub fn append_event(&self, request: AppendRequest) -> Result<AppendOutcome> {
        let (journal, object_refs) = self.prepare(request)?;
        self.crash.check(CrashSeam::AfterJournal)?;
        let lock = self.writer_lock()?;
        self.recover_all_locked(Some(&journal.batch_id))?;
        let mut outcome = self.commit_journal_locked(&journal, false)?;
        FileExt::unlock(&lock).map_err(io_error("unlock writer.lock"))?;
        outcome.objects = object_refs;
        Ok(outcome)
    }

    /// Lists valid durable pending journals without changing Git state.
    ///
    /// # Errors
    ///
    /// Returns an error for an unreadable or invalid journal.
    pub fn list_pending(&self) -> Result<Vec<PendingBatch>> {
        self.read_journals()
            .map(|journals| journals.iter().map(PendingBatch::from).collect::<Vec<_>>())
    }

    /// Explicitly reconciles and commits one pending batch.
    ///
    /// # Errors
    ///
    /// Refuses ambiguous, corrupt, partially committed, or foreign-staged state.
    pub fn commit_pending(&self, batch_id: &BatchId) -> Result<AppendOutcome> {
        let journal = self.read_journal(batch_id)?;
        let lock = self.writer_lock()?;
        let outcome = self.commit_journal_locked(&journal, true)?;
        FileExt::unlock(&lock).map_err(io_error("unlock writer.lock"))?;
        Ok(outcome)
    }

    /// Recovers every valid pending batch in a stable operational order.
    ///
    /// # Errors
    ///
    /// Stops at the first batch that cannot be reconciled safely.
    pub fn recover_pending(&self) -> Result<Vec<AppendOutcome>> {
        let lock = self.writer_lock()?;
        let outcomes = self.recover_all_locked(None)?;
        FileExt::unlock(&lock).map_err(io_error("unlock writer.lock"))?;
        Ok(outcomes)
    }

    /// Explicitly moves a pending journal aside without modifying repository files.
    ///
    /// # Errors
    ///
    /// Returns an error if the batch is absent or cannot be moved atomically.
    pub fn move_pending_aside(&self, batch_id: &BatchId) -> Result<PathBuf> {
        let lock = self.writer_lock()?;
        let source = self.pending_dir(batch_id);
        if !source.is_dir() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!("pending batch {batch_id} does not exist"),
            ));
        }
        let aside_root = self.state.join("pending-aside");
        fs::create_dir_all(&aside_root).map_err(io_error("create pending-aside"))?;
        let destination = aside_root.join(batch_id.as_str());
        if destination.exists() {
            return Err(Error::new(
                ErrorKind::InvariantViolation,
                format!(
                    "pending-aside destination already exists: {}",
                    destination.display()
                ),
            ));
        }
        fs::rename(&source, &destination).map_err(io_error("move pending batch aside"))?;
        sync_directory(&aside_root)?;
        FileExt::unlock(&lock).map_err(io_error("unlock writer.lock"))?;
        Ok(destination)
    }

    fn prepare(&self, request: AppendRequest) -> Result<(Journal, Vec<ObjectRef>)> {
        let AppendRequest {
            event,
            objects: text_objects,
        } = request;
        let event_bytes = serialize_event(&event)?;
        let event_id = event.event_id();
        let event_text = event_id.to_string();
        let event_prefix = &event_text[EventId::PREFIX.len()..EventId::PREFIX.len() + 2];
        let event_path = format!("events/{event_prefix}/{event_text}.json");
        let batch_id = BatchId::new();
        let batch_dir = self.pending_dir(&batch_id);
        let files_dir = batch_dir.join("files");
        fs::create_dir(&batch_dir).map_err(io_error("create pending batch"))?;
        fs::create_dir(&files_dir).map_err(io_error("create pending files"))?;

        let mut payloads = vec![(PendingFileKind::Event, event_path, event_bytes)];
        let mut unique_objects = BTreeMap::<String, Vec<u8>>::new();
        for object in text_objects {
            let bytes = object.text.into_bytes();
            unique_objects.entry(sha256(&bytes)).or_insert(bytes);
        }
        let mut object_refs = Vec::with_capacity(unique_objects.len());
        for (digest, bytes) in unique_objects {
            let path = object_path(&digest);
            object_refs.push(ObjectRef {
                sha256: digest,
                path: path.clone(),
                size: bytes.len() as u64,
            });
            payloads.push((PendingFileKind::Object, path, bytes));
        }

        let mut files = Vec::with_capacity(payloads.len());
        for (index, (kind, target_path, bytes)) in payloads.into_iter().enumerate() {
            let payload_file = format!("{index:04}.payload");
            let path = files_dir.join(&payload_file);
            write_new_synced(&path, &bytes)?;
            files.push(PendingFile {
                kind,
                target_path,
                payload_file,
                sha256: sha256(&bytes),
                size: bytes.len() as u64,
            });
        }
        sync_directory(&files_dir)?;
        let journal = Journal {
            version: JOURNAL_VERSION,
            batch_id,
            event_id: event_text,
            base_head_oid: None,
            phase: JournalPhase::Prepared,
            commit_oid: None,
            files,
        };
        self.write_journal(&journal)?;
        sync_directory(&self.state.join("pending"))?;
        Ok((journal, object_refs))
    }

    fn recover_all_locked(&self, exclude: Option<&BatchId>) -> Result<Vec<AppendOutcome>> {
        let mut journals = self.read_journals()?;
        journals.retain(|journal| exclude != Some(&journal.batch_id));
        let staged: BTreeSet<String> = Git::new(&self.repository)
            .staged_from_head()?
            .into_iter()
            .flat_map(|entry| entry.paths)
            .map(path_string)
            .collect::<Result<_>>()?;
        if !staged.is_empty() {
            if let Some(position) = journals.iter().position(|journal| {
                staged
                    .iter()
                    .all(|path| journal.files.iter().any(|file| &file.target_path == path))
            }) {
                journals.swap(0, position);
            }
        }
        let mut outcomes = Vec::with_capacity(journals.len());
        for journal in journals {
            outcomes.push(self.commit_journal_locked(&journal, true)?);
        }
        Ok(outcomes)
    }

    #[allow(clippy::too_many_lines)]
    fn commit_journal_locked(&self, journal: &Journal, recovered: bool) -> Result<AppendOutcome> {
        validate_journal(journal)?;
        let git = Git::new(&self.repository);
        Self::reject_managed_changes(&git, journal)?;

        let mut head_matches = BTreeSet::new();
        for file in &journal.files {
            if let Some(bytes) = git.head_file(&file.target_path)? {
                if sha256(&bytes) != file.sha256 {
                    return Err(invariant(format!(
                        "HEAD path {} does not match pending digest",
                        file.target_path
                    )));
                }
                head_matches.insert(file.target_path.clone());
            }
        }
        let event_path = event_path(journal)?;
        if head_matches.contains(&event_path) {
            if head_matches.len() != journal.files.len() {
                return Err(invariant(format!(
                    "batch {} is only partially present in HEAD",
                    journal.batch_id
                )));
            }
            let commit_oid = git.head_oid()?;
            self.observer.committed(&self.repository, &commit_oid)?;
            self.cleanup(journal)?;
            return outcome(journal, event_path, commit_oid, true);
        }

        let mut reconciled = journal.clone();
        if let Some(base_head_oid) = &reconciled.base_head_oid {
            if !git.is_ancestor(base_head_oid, "HEAD")? {
                return Err(invariant(format!(
                    "batch {} base HEAD is not an ancestor of current HEAD",
                    reconciled.batch_id
                )));
            }
        } else {
            reconciled.base_head_oid = Some(git.head_oid()?);
            self.write_journal(&reconciled)?;
        }
        let journal = &reconciled;
        let base_head_oid = journal
            .base_head_oid
            .as_deref()
            .ok_or_else(|| invariant("journal base HEAD was not persisted"))?;
        for file in &journal.files {
            if head_matches.contains(&file.target_path) {
                let baseline = git.file_at(base_head_oid, &file.target_path)?;
                if baseline.as_deref().map(sha256).as_deref() != Some(file.sha256.as_str()) {
                    return Err(invariant(format!(
                        "batch {} is only partially present in HEAD: {} was not reusable from its base tree",
                        journal.batch_id, file.target_path
                    )));
                }
            }
        }

        // A journal payload is required for every not-yet-complete batch even
        // when a matching working-tree file happens to exist. This prevents an
        // untracked file from becoming provenance for an otherwise incomplete
        // journal.
        let payloads: BTreeMap<String, Vec<u8>> = journal
            .files
            .iter()
            .map(|file| {
                self.read_payload(journal, file)
                    .map(|payload| (file.target_path.clone(), payload))
            })
            .collect::<Result<_>>()?;

        let allowed: BTreeSet<String> = journal
            .files
            .iter()
            .filter(|file| !head_matches.contains(&file.target_path))
            .map(|file| file.target_path.clone())
            .collect();
        Self::reject_foreign_staged(&git, &allowed, journal)?;

        for file in &journal.files {
            if head_matches.contains(&file.target_path) {
                if file.kind != PendingFileKind::Object {
                    return Err(invariant("only objects may be reused from HEAD"));
                }
                continue;
            }
            let destination = self.repository.join(&file.target_path);
            if destination.exists() {
                let bytes = read_regular_file(&destination)?;
                if sha256(&bytes) != file.sha256 {
                    return Err(invariant(format!(
                        "working-tree path {} differs from pending journal",
                        file.target_path
                    )));
                }
                if !recovered {
                    return if file.kind == PendingFileKind::Object {
                        Err(Error::new(
                            ErrorKind::InvariantViolation,
                            format!("{OBJECT_PENDING}: {}", file.target_path),
                        ))
                    } else {
                        Err(invariant(format!(
                            "create_new refused existing event path: {}",
                            file.target_path
                        )))
                    };
                }
                continue;
            }
            self.crash.check(CrashSeam::BeforeCreate)?;
            let payload = payloads
                .get(&file.target_path)
                .ok_or_else(|| invariant("validated pending payload disappeared"))?;
            let parent = destination
                .parent()
                .ok_or_else(|| invariant("generated target has no parent"))?;
            fs::create_dir_all(parent).map_err(io_error("create target parent"))?;
            write_new_synced(&destination, payload)?;
            sync_directory(parent)?;
            self.crash.check(CrashSeam::AfterCreate)?;
        }
        self.update_phase(journal, JournalPhase::Created, None)?;

        let paths: Vec<String> = allowed.iter().cloned().collect();
        self.crash.check(CrashSeam::BeforeAdd)?;
        git.stage_paths(&paths)?;
        self.crash.check(CrashSeam::AfterAdd)?;
        self.update_phase(journal, JournalPhase::Staged, None)?;
        Self::verify_exact_staged(&git, &allowed, journal)?;

        self.crash.check(CrashSeam::BeforeCommit)?;
        git.commit_paths(
            &format!("Append Shared Context batch {}", journal.batch_id),
            &paths,
        )?;
        self.crash.check(CrashSeam::AfterCommit)?;
        let commit_oid = git.head_oid()?;
        self.crash.check(CrashSeam::BeforeCommitOid)?;
        self.update_phase(journal, JournalPhase::Committed, Some(&commit_oid))?;
        self.crash.check(CrashSeam::AfterCommitOid)?;
        self.crash.check(CrashSeam::BeforeIndex)?;
        self.observer.committed(&self.repository, &commit_oid)?;
        self.crash.check(CrashSeam::AfterIndex)?;
        self.update_phase(journal, JournalPhase::Indexed, Some(&commit_oid))?;
        self.crash.check(CrashSeam::BeforeCleanup)?;
        self.cleanup(journal)?;
        self.crash.check(CrashSeam::AfterCleanup)?;
        outcome(journal, event_path, commit_oid, recovered)
    }

    fn reject_managed_changes(git: &Git<'_>, journal: &Journal) -> Result<()> {
        let allowed: BTreeSet<&str> = journal
            .files
            .iter()
            .map(|file| file.target_path.as_str())
            .collect();
        let violations: Vec<String> = git
            .changed_from_head()?
            .into_iter()
            .filter(|entry| {
                !entry.status.starts_with('A')
                    || entry
                        .paths
                        .iter()
                        .any(|path| path.to_str().is_none_or(|path| !allowed.contains(path)))
            })
            .map(|entry| {
                format!(
                    "{} {}",
                    entry.status,
                    entry
                        .paths
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect::<Vec<_>>()
                        .join(" -> ")
                )
            })
            .collect();
        if violations.is_empty() {
            Ok(())
        } else {
            Err(invariant(format!(
                "append-only guard rejected managed M/D/R or foreign change: {}; use `sctx context revise` to replace content or `sctx context withdraw` to retire it via a new event",
                violations.join(", "),
            )))
        }
    }

    fn reject_foreign_staged(
        git: &Git<'_>,
        allowed: &BTreeSet<String>,
        journal: &Journal,
    ) -> Result<()> {
        for entry in git.staged_from_head()? {
            if entry.status != "A" {
                return Err(invariant(format!(
                    "staged path has forbidden status {}: {}",
                    entry.status,
                    entry
                        .paths
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
            for path in entry.paths {
                let path = path_string(path)?;
                if !allowed.contains(&path) {
                    return Err(invariant(format!("foreign staged path rejected: {path}")));
                }
                let file = journal
                    .files
                    .iter()
                    .find(|file| file.target_path == path)
                    .ok_or_else(|| invariant("staged path missing from journal"))?;
                let indexed = git.index_file(&path)?.ok_or_else(|| {
                    invariant(format!("staged path disappeared from index: {path}"))
                })?;
                if sha256(&indexed) != file.sha256 {
                    return Err(invariant(format!(
                        "staged path digest differs from journal: {path}"
                    )));
                }
            }
        }
        Ok(())
    }

    fn verify_exact_staged(
        git: &Git<'_>,
        expected: &BTreeSet<String>,
        journal: &Journal,
    ) -> Result<()> {
        let entries = git.staged_from_head()?;
        let actual: BTreeSet<String> = entries
            .iter()
            .flat_map(|entry| entry.paths.iter().cloned())
            .map(path_string)
            .collect::<Result<_>>()?;
        if entries.iter().any(|entry| entry.status != "A") || &actual != expected {
            return Err(invariant(format!(
                "staged diff does not exactly match batch {}",
                journal.batch_id
            )));
        }
        Self::reject_foreign_staged(git, expected, journal)
    }

    fn cleanup(&self, journal: &Journal) -> Result<()> {
        let directory = self.pending_dir(&journal.batch_id);
        if directory.exists() {
            fs::remove_dir_all(&directory).map_err(io_error("remove completed pending batch"))?;
            sync_directory(&self.state.join("pending"))?;
        }
        Ok(())
    }

    fn update_phase(
        &self,
        original: &Journal,
        phase: JournalPhase,
        commit_oid: Option<&str>,
    ) -> Result<()> {
        let mut journal = original.clone();
        journal.phase = phase;
        journal.commit_oid = commit_oid.map(str::to_owned);
        self.write_journal(&journal)
    }

    fn write_journal(&self, journal: &Journal) -> Result<()> {
        validate_journal(journal)?;
        let directory = self.pending_dir(&journal.batch_id);
        let bytes = serde_json::to_vec_pretty(journal).map_err(|error| {
            Error::new(
                ErrorKind::InvalidInput,
                format!("serialize journal: {error}"),
            )
        })?;
        let temporary = directory.join(format!("journal.{}.tmp", uuid::Uuid::new_v4()));
        write_new_synced(&temporary, &bytes)?;
        fs::rename(&temporary, directory.join("journal.json"))
            .map_err(io_error("atomically replace journal"))?;
        sync_directory(&directory)
    }

    fn read_journals(&self) -> Result<Vec<Journal>> {
        let root = self.state.join("pending");
        let mut directories = fs::read_dir(&root)
            .map_err(io_error("read pending root"))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(io_error("read pending entry"))?;
        directories.sort_by_key(std::fs::DirEntry::file_name);
        let mut journals = Vec::new();
        for entry in directories {
            if !entry
                .file_type()
                .map_err(io_error("inspect pending entry"))?
                .is_dir()
            {
                continue;
            }
            let path = entry.path().join("journal.json");
            if !path.is_file() {
                continue;
            }
            let journal = read_journal_file(&path)?;
            let directory_name = entry.file_name().to_string_lossy().into_owned();
            if journal.batch_id.as_str() != directory_name {
                return Err(invariant(format!(
                    "journal batch ID does not match directory {directory_name}"
                )));
            }
            journals.push(journal);
        }
        Ok(journals)
    }

    fn read_journal(&self, batch_id: &BatchId) -> Result<Journal> {
        let path = self.pending_dir(batch_id).join("journal.json");
        if !path.is_file() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!("pending batch {batch_id} does not exist"),
            ));
        }
        let journal = read_journal_file(&path)?;
        if journal.batch_id != *batch_id {
            return Err(invariant("journal batch ID does not match requested batch"));
        }
        Ok(journal)
    }

    fn read_payload(&self, journal: &Journal, file: &PendingFile) -> Result<Vec<u8>> {
        let path = self
            .pending_dir(&journal.batch_id)
            .join("files")
            .join(&file.payload_file);
        let bytes = read_regular_file(&path)?;
        if bytes.len() as u64 != file.size || sha256(&bytes) != file.sha256 {
            return Err(invariant(format!(
                "pending payload does not match journal: {}",
                file.payload_file
            )));
        }
        Ok(bytes)
    }

    fn writer_lock(&self) -> Result<File> {
        let lock = open_lock(&self.state.join("writer.lock"))?;
        lock.lock_exclusive()
            .map_err(io_error("lock writer.lock"))?;
        Ok(lock)
    }

    fn pending_dir(&self, batch_id: &BatchId) -> PathBuf {
        self.state.join("pending").join(batch_id.as_str())
    }
}

fn verify_repository(repository: &Path) -> Result<()> {
    if !repository.is_dir() {
        return Err(invariant(format!(
            "fixed repository path is not a directory: {}",
            repository.display()
        )));
    }
    let git = Git::new(repository);
    let top = git.output_text(["rev-parse", "--show-toplevel"])?;
    let actual = fs::canonicalize(top).map_err(io_error("canonicalize Git toplevel"))?;
    let expected = fs::canonicalize(repository).map_err(io_error("canonicalize repository"))?;
    if actual != expected {
        return Err(invariant(format!(
            "fixed repository path resolves to another worktree: {}",
            actual.display()
        )));
    }
    git.head_oid()?;
    Ok(())
}

fn validate_journal(journal: &Journal) -> Result<()> {
    if journal.version != JOURNAL_VERSION {
        return Err(invariant(format!(
            "unsupported journal version {}",
            journal.version
        )));
    }
    BatchId::from_str(journal.batch_id.as_str())?;
    let event_id = EventId::from_str(&journal.event_id)
        .map_err(|error| invariant(format!("invalid journal event ID: {error}")))?;
    if journal.files.is_empty() {
        return Err(invariant("journal must contain an event"));
    }
    if let Some(base_head_oid) = &journal.base_head_oid {
        if !matches!(base_head_oid.len(), 40 | 64)
            || !base_head_oid
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(invariant("journal contains an invalid base HEAD OID"));
        }
    }
    let mut targets = BTreeSet::new();
    let mut payloads = BTreeSet::new();
    let mut event_count = 0;
    for file in &journal.files {
        if !targets.insert(&file.target_path) || !payloads.insert(&file.payload_file) {
            return Err(invariant("journal contains duplicate paths"));
        }
        if file.payload_file.contains('/')
            || file.payload_file.contains('\\')
            || !file.payload_file.ends_with(".payload")
        {
            return Err(invariant("journal contains an unsafe payload path"));
        }
        if file.sha256.len() != 64
            || !file
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(invariant("journal contains an invalid SHA-256 digest"));
        }
        match file.kind {
            PendingFileKind::Event => {
                event_count += 1;
                if file.target_path != generated_event_path(event_id) {
                    return Err(invariant(
                        "journal event target is not generated from event ID",
                    ));
                }
            }
            PendingFileKind::Object => {
                if file.target_path != object_path(&file.sha256) {
                    return Err(invariant("journal object target is not content-addressed"));
                }
            }
        }
    }
    if event_count != 1 {
        return Err(invariant("journal must contain exactly one event"));
    }
    Ok(())
}

fn serialize_event(event: &Event) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(event).map_err(|error| {
        Error::new(ErrorKind::InvalidInput, format!("serialize event: {error}"))
    })?;
    bytes.push(b'\n');
    match parse_event(&bytes)? {
        ParsedEvent::Known(parsed) if parsed.event_id() == event.event_id() => Ok(bytes),
        _ => Err(invariant(
            "serialized event did not validate as the same V1 event",
        )),
    }
}

fn outcome(
    journal: &Journal,
    event_path: String,
    commit_oid: String,
    recovered: bool,
) -> Result<AppendOutcome> {
    let event_id = EventId::from_str(&journal.event_id)
        .map_err(|error| invariant(format!("invalid event ID after commit: {error}")))?;
    let objects = journal
        .files
        .iter()
        .filter(|file| file.kind == PendingFileKind::Object)
        .map(|file| ObjectRef {
            sha256: file.sha256.clone(),
            path: file.target_path.clone(),
            size: file.size,
        })
        .collect();
    Ok(AppendOutcome {
        batch_id: journal.batch_id.clone(),
        event_id,
        event_path,
        commit_oid,
        objects,
        recovered,
    })
}

fn event_path(journal: &Journal) -> Result<String> {
    journal
        .files
        .iter()
        .find(|file| file.kind == PendingFileKind::Event)
        .map(|file| file.target_path.clone())
        .ok_or_else(|| invariant("journal event is missing"))
}

fn generated_event_path(event_id: EventId) -> String {
    let text = event_id.to_string();
    let prefix = &text[EventId::PREFIX.len()..EventId::PREFIX.len() + 2];
    format!("events/{prefix}/{text}.json")
}

fn object_path(digest: &str) -> String {
    format!("objects/sha256/{}/{digest}", &digest[..2])
}

fn sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        write!(output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

fn read_journal_file(path: &Path) -> Result<Journal> {
    let bytes = read_regular_file(path)?;
    let journal: Journal = serde_json::from_slice(&bytes).map_err(|error| {
        Error::new(
            ErrorKind::InvalidInput,
            format!("parse {}: {error}", path.display()),
        )
    })?;
    validate_journal(&journal)?;
    Ok(journal)
}

fn read_regular_file(path: &Path) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path).map_err(io_error("inspect file"))?;
    if !metadata.file_type().is_file() {
        return Err(invariant(format!(
            "expected regular file: {}",
            path.display()
        )));
    }
    let mut file = File::open(path).map_err(io_error("open file"))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(io_error("read file"))?;
    Ok(bytes)
}

fn write_new_synced(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(io_error("create new file"))?;
    file.write_all(bytes).map_err(io_error("write file"))?;
    file.sync_all().map_err(io_error("fsync file"))
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(io_error("fsync directory"))
}

fn open_lock(path: &Path) -> Result<File> {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .map_err(io_error("open lock file"))
}

fn absolute(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|current| current.join(path))
            .map_err(io_error("resolve installation root"))
    }
}

fn path_string(path: PathBuf) -> Result<String> {
    path.into_os_string()
        .into_string()
        .map_err(|_| Error::new(ErrorKind::InvariantViolation, "Git path is not valid UTF-8"))
}

fn invariant(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvariantViolation, message)
}

fn io_error(context: &'static str) -> impl FnOnce(std::io::Error) -> Error {
    move |error| Error::new(ErrorKind::Io, format!("{context}: {error}"))
}
