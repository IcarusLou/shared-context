//! Deterministic, rebuildable `SQLite` projection of the current Git `HEAD` tree.
//!
//! The index never reads managed working-tree files and never walks Git history. It resolves
//! `HEAD^{tree}`, reads event/object blobs by object ID, delegates compatibility parsing to
//! `sctx-event-schema`, and delegates all domain ordering and quarantine decisions to the pure
//! `sctx-domain` reducer.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use fs2::FileExt;
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use sctx_domain::{
    CandidateId, ConfirmationId, ContextId, DomainProjection, EventId, SubmissionId, TaskId,
    TaskSessionId, WorkEpisodeId, WorkEpisodeRef,
};
use sctx_git_store::{
    BatchId, CandidateConfirmationIndex, CandidateConfirmationLookup, CandidateConfirmationRecord,
    CandidateSubmissionIndex, CandidateSubmissionLookup, CandidateSubmissionRecord,
};

mod git_tree;
mod project;
mod schema;
mod tokenizer;

pub use sctx_domain::{Error, ErrorKind, Result};
pub use tokenizer::{normalize_search_text, search_tokens};

/// Current physical `SQLite` schema version.
pub const DB_SCHEMA_VERSION: &str = "15";
/// Event parser implementation version recorded in every projection.
pub const EVENT_PARSER_VERSION: &str = "1";
/// Pure reducer implementation version recorded in every projection.
pub const REDUCER_VERSION: &str = "2";
/// Conflict detector implementation version recorded in every projection.
pub const CONFLICT_DETECTOR_VERSION: &str = "1";
/// NFKC, full Unicode case-folding, identifier splitting, and CJK bigram implementation.
pub const NORMALIZER_TOKENIZER_VERSION: &str = "1";
/// Context and Space-Intent weighted-BM25, explanation, and stable-ID ranking implementation.
///
/// It also versions what the ranked columns contain: version 5 folds each revision's `topic_key`
/// into `hint_text` and its alias groups, and version 6 seeds an alias group from a non-ASCII
/// term as well, so an index written by an earlier version must be rebuilt before a topic-keyed
/// or Han-spelled term is reachable through the tokens it names.
pub const SEARCH_RANKING_VERSION: &str = "6";

pub(crate) const IMPLEMENTATION_VERSIONS: [(&str, &str); 6] = [
    ("db_schema_version", DB_SCHEMA_VERSION),
    ("event_parser_version", EVENT_PARSER_VERSION),
    ("reducer_version", REDUCER_VERSION),
    ("conflict_detector_version", CONFLICT_DETECTOR_VERSION),
    ("normalizer_tokenizer_version", NORMALIZER_TOKENIZER_VERSION),
    ("search_ranking_version", SEARCH_RANKING_VERSION),
];

/// Why one synchronization did or did not rebuild the projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RebuildReason {
    /// The healthy database already represented this Tree and implementation set.
    Current,
    /// No database file existed.
    MissingDatabase,
    /// The prior file could not pass `quick_check` and was isolated.
    CorruptDatabase,
    /// At least one implementation version was absent or different.
    ImplementationVersionChanged,
    /// Git `HEAD^{tree}` was different from the indexed Tree.
    TreeChanged,
    /// The caller explicitly requested a scratch rebuild.
    Forced,
}

/// Physical update strategy used for a synchronization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IndexUpdateKind {
    /// No write was needed after the pre-lock or post-lock double check.
    Current,
    /// Only the proven reverse-reference impact closure was replaced.
    Incremental,
    /// A complete deterministic projection was activated.
    FullRebuild,
}

/// Why a Tree change could not safely use the incremental path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IncrementalFallback {
    /// The prior indexed Tree object is no longer accessible.
    IndexedTreeUnavailable,
    /// Cached committed blobs did not exactly represent the indexed Tree.
    CachedSourceMismatch,
    /// A managed path was modified, deleted, or renamed outside the append protocol.
    AppendProtocolBypassed,
    /// The reverse-reference closure did not cover every observed projection change.
    ImpactClosureUnproven,
}

/// Non-projection warning produced by comparing old and new Trees.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationalWarning {
    pub code: &'static str,
    pub paths: Vec<String>,
}

/// Owned twin of [`OperationalWarning`], read back from `meta` (WP-V6 fix 4).
///
/// [`OperationalWarning::code`] is `&'static str` because every live warning is produced from a
/// literal at its one call site; a warning read back from `SQLite` has no `'static` string to
/// borrow, so this carries an owned `code` instead of reusing that type.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct PersistedOperationalWarning {
    pub code: String,
    pub paths: Vec<String>,
}

/// `meta` key the most recent rebuild's [`OperationalWarning`]s are persisted under (WP-V6 fix 4).
///
/// Additive to the existing generic `(key, value)` `meta` table, so no `DB_SCHEMA_VERSION` bump or
/// migration is needed: an index built before this key existed simply has no row for it, which
/// [`ProjectionIndex::last_rebuild_operational_warnings`] already treats as "nothing to report."
pub(crate) const OPERATIONAL_WARNINGS_META_KEY: &str = "last_rebuild_operational_warnings";

/// What one candidate [`UpdatePlan`] establishes about append-protocol integrity, and therefore
/// what `meta`'s persisted [`OperationalWarning`]s should become (WP-V6 fix 4).
///
/// The two are different questions answered by the same comparison: [`RebuildOutcome`] reports
/// what *this* `synchronize()` call found, which was already correct before this type existed and
/// stays that way (only [`Self::Fresh`] ever contributes to it, same as before). This type is
/// additionally about what `sctx doctor` should still be able to see afterward -- which requires
/// knowing not just *whether* a violation was found, but whether the check that could have found
/// one actually ran this time.
#[derive(Clone, Debug, Eq, PartialEq)]
enum OperationalWarningsUpdate {
    /// A Tree diff against the previously indexed generation completed, so these warnings (empty
    /// for a clean diff) are this generation's authoritative verdict and replace whatever was
    /// persisted before.
    Fresh(Vec<OperationalWarning>),
    /// No comparable diff ran this synchronization -- a missing database, a forced rebuild, an
    /// implementation-version change, or an old Tree unavailable for comparison all reach the
    /// projection by some path other than comparing it against the last one. Whatever `meta`
    /// already holds stands, unexamined and unchanged, exactly as an operator would expect
    /// "unrelated to Git history integrity" to behave.
    CarryForward,
}

/// Serializes warnings for [`OPERATIONAL_WARNINGS_META_KEY`], the one direction
/// [`OperationalWarning::code`]'s `&'static str` never needs to round-trip back out of.
fn encode_operational_warnings(warnings: &[OperationalWarning]) -> Result<String> {
    let persisted = warnings
        .iter()
        .map(|warning| PersistedOperationalWarning {
            code: warning.code.to_owned(),
            paths: warning.paths.clone(),
        })
        .collect::<Vec<_>>();
    serde_json::to_string(&persisted)
        .map_err(|error| invariant(format!("encode operational warnings for meta: {error}")))
}

/// Metadata that identifies one atomic projection generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexMetadata {
    pub indexed_tree_oid: String,
    pub projection_generation: u64,
    pub db_schema_version: String,
    pub event_parser_version: String,
    pub reducer_version: String,
    pub conflict_detector_version: String,
    pub normalizer_tokenizer_version: String,
    pub search_ranking_version: String,
}

/// Result of a projection synchronization.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RebuildOutcome {
    pub rebuilt: bool,
    pub reason: RebuildReason,
    pub metadata: IndexMetadata,
    pub source_file_count: u64,
    pub diagnostic_count: u64,
    pub update_kind: IndexUpdateKind,
    pub incremental_fallback: Option<IncrementalFallback>,
    pub operational_warnings: Vec<OperationalWarning>,
    /// Isolated main database path. WAL/SHM companions use the same suffix when present.
    pub quarantined_database: Option<PathBuf>,
}

/// Data returned from one `SQLite` read transaction together with its exact projection identity.
#[derive(Debug)]
pub struct QuerySnapshot<T> {
    pub metadata: IndexMetadata,
    pub data: T,
}

/// One deterministic domain projection read from the exact Git tree named by
/// [`metadata`](Self::metadata). This is a thin read boundary for governance
/// clients that need causal heads rather than SQL implementation details.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DomainSnapshot {
    pub metadata: IndexMetadata,
    pub projection: DomainProjection,
    pub diagnostics: Vec<ProjectionDiagnosticView>,
}

/// Stable, user-presentable projection diagnostic.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectionDiagnosticView {
    pub key: String,
    pub source_path: Option<String>,
    pub code: String,
    pub entity_id: String,
    pub event_ids_json: String,
    pub message: String,
}

/// Reusable read connection. Before each request it verifies the database file identity and
/// Generation, reopening automatically after corrupt-file isolation and replacement.
#[derive(Debug)]
pub struct QueryConnection {
    index: ProjectionIndex,
    connection: Option<Connection>,
    identity: Option<DatabaseFileIdentity>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DatabaseFileIdentity {
    device: u64,
    inode: u64,
}

enum UpdatePlan {
    Incremental {
        input: project::BuildInput,
        affected_spaces: BTreeSet<String>,
    },
    Full {
        input: project::BuildInput,
        fallback: Option<IncrementalFallback>,
        operational_warnings: OperationalWarningsUpdate,
    },
}

/// Result of checking a database file without repairing it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuickCheck {
    pub healthy: bool,
    pub detail: String,
}

/// Runtime settings required for every index connection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DatabasePragmas {
    pub journal_mode: String,
    pub synchronous: i64,
    pub foreign_keys: bool,
    pub busy_timeout_ms: i64,
}

/// Projection manager for one repository and its external state directory.
///
/// Clones share one [`IndexCaches`], so a long-lived server that hands clones to its Search
/// Engine and its Runtime reduces one Git tree at most once.
#[derive(Clone, Debug)]
pub struct ProjectionIndex {
    repository: PathBuf,
    state: PathBuf,
    database: PathBuf,
    caches: Arc<IndexCaches>,
}

/// Derived state that is a pure function of the indexed Git tree and may therefore be reused
/// until that tree changes. Nothing here is authoritative: every entry can be recomputed.
#[derive(Debug, Default)]
struct IndexCaches {
    snapshot: Mutex<Option<CachedDomainSnapshot>>,
    event_commits: Mutex<EventCommitCache>,
    head_trees: Mutex<BTreeMap<String, String>>,
}

/// Bound on remembered `HEAD commit -> Tree` pairs. Only the current commit is ever asked for.
const MAX_REMEMBERED_HEAD_TREES: usize = 8;

/// One reduced Domain Snapshot together with the exact projection identity it was reduced from.
#[derive(Debug)]
struct CachedDomainSnapshot {
    metadata: IndexMetadata,
    snapshot: Arc<DomainSnapshot>,
}

/// Memoized `event_path -> introducing commits`, warmed from the projection database once per
/// process and refreshed by one history walk whenever an Event path is still unknown.
#[derive(Debug, Default)]
struct EventCommitCache {
    entries: BTreeMap<String, Vec<git_tree::EventAddition>>,
    warmed: bool,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl ProjectionIndex {
    /// Creates a manager. No database or Git operation occurs until synchronization.
    #[must_use]
    pub fn new(repository: impl Into<PathBuf>, state: impl Into<PathBuf>) -> Self {
        let state = state.into();
        Self {
            repository: repository.into(),
            database: state.join("index.sqlite"),
            state,
            caches: Arc::default(),
        }
    }

    /// Creates a manager for an initialized append-only store.
    #[must_use]
    pub fn for_store(store: &sctx_git_store::GitStore) -> Self {
        Self::new(store.repository(), store.state())
    }

    /// Database file managed by this index.
    #[must_use]
    pub fn database_path(&self) -> &Path {
        &self.database
    }

    /// Synchronizes to the current `HEAD` Tree, rebuilding only when required.
    ///
    /// # Errors
    ///
    /// Returns an error for Git access, locking, `SQLite`, or projection materialization failures.
    pub fn synchronize(&self) -> Result<RebuildOutcome> {
        self.synchronize_inner(false)
    }

    /// Forces a deterministic scratch rebuild from the current `HEAD` Tree.
    ///
    /// # Errors
    ///
    /// Returns an error for Git access, locking, `SQLite`, or projection materialization failures.
    pub fn rebuild(&self) -> Result<RebuildOutcome> {
        self.synchronize_inner(true)
    }

    /// Reads the complete version/generation identity from an existing healthy database.
    ///
    /// # Errors
    ///
    /// Returns an error if the file is absent, corrupt, or lacks complete metadata.
    pub fn metadata(&self) -> Result<IndexMetadata> {
        let connection = self.open_read_only()?;
        require_quick_check(&connection)?;
        read_metadata(&connection)?.ok_or_else(|| {
            invariant("projection database does not contain complete version metadata")
        })
    }

    /// Operational warnings the most recent rebuild that actually compared this generation
    /// against the last one found, persisted in `meta` (WP-V6 fix 4) so `sctx doctor` can see
    /// them without waiting for a live `sctx index sync`.
    ///
    /// Empty is the normal case, and is ambiguous by design between "the last rebuild found
    /// nothing to warn about" and "this database predates the key": both mean there is nothing
    /// for an operator to act on. Persists across every incremental sync after the rebuild that
    /// wrote it -- see [`OperationalWarningsUpdate`] -- until the next rebuild that runs a real
    /// Tree diff updates or clears it.
    ///
    /// # Errors
    ///
    /// Returns an error if the file is absent, corrupt, or the persisted value is not valid JSON.
    pub fn last_rebuild_operational_warnings(&self) -> Result<Vec<PersistedOperationalWarning>> {
        let connection = self.open_read_only()?;
        require_quick_check(&connection)?;
        let Some(raw) = schema::read_meta_value(&connection, OPERATIONAL_WARNINGS_META_KEY)? else {
            return Ok(Vec::new());
        };
        serde_json::from_str(&raw).map_err(|error| {
            invariant(format!(
                "parse persisted operational warnings from meta: {error}"
            ))
        })
    }

    /// Runs `SQLite` `quick_check` without mutating or replacing the database.
    ///
    /// # Errors
    ///
    /// Only filesystem-level inspection failures are returned as errors; `SQLite` corruption is
    /// represented by `healthy = false`.
    pub fn quick_check(&self) -> Result<QuickCheck> {
        if !self.database.exists() {
            return Ok(QuickCheck {
                healthy: false,
                detail: "database file does not exist".to_owned(),
            });
        }
        match Connection::open_with_flags(
            &self.database,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) {
            Ok(connection) => Ok(quick_check_connection(&connection)),
            Err(error) => Ok(QuickCheck {
                healthy: false,
                detail: error.to_string(),
            }),
        }
    }

    /// Reads the effective `SQLite` runtime settings.
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be opened or a PRAGMA cannot be read.
    pub fn pragmas(&self) -> Result<DatabasePragmas> {
        let connection = Connection::open(&self.database).map_err(sql_error("open index"))?;
        configure(&connection)?;
        read_pragmas(&connection)
    }

    /// Creates a reusable query connection that reopens itself after database-file replacement.
    #[must_use]
    pub fn query_connection(&self) -> QueryConnection {
        QueryConnection {
            index: self.clone(),
            connection: None,
            identity: None,
        }
    }

    /// Synchronizes and executes all caller reads in one `SQLite` read transaction.
    ///
    /// # Errors
    ///
    /// Returns an error when synchronization, identity verification, metadata reading, or the
    /// caller's query fails.
    pub fn query_snapshot<T>(
        &self,
        query: impl FnOnce(&Connection) -> Result<T>,
    ) -> Result<QuerySnapshot<T>> {
        self.query_connection().snapshot(query)
    }

    /// Synchronizes the index and reduces events from that generation's exact
    /// committed Git tree. The returned projection never reads the working tree.
    ///
    /// # Errors
    ///
    /// Returns an error when synchronization, Git object access, parsing, or
    /// deterministic reduction cannot complete.
    pub fn domain_snapshot(&self) -> Result<DomainSnapshot> {
        let snapshot = self.shared_domain_snapshot()?;
        Ok(DomainSnapshot::clone(&snapshot))
    }

    /// Shared, reference-counted form of [`Self::domain_snapshot`] for callers that only read.
    ///
    /// Reduction is a pure function of the indexed Git tree and the implementation versions, so
    /// this index and every clone of it reuse the snapshot they already reduced until that exact
    /// projection identity changes. A changed Tree, a new Generation, or a changed implementation
    /// version all invalidate the reuse, which keeps `same Tree => same snapshot` intact.
    ///
    /// # Errors
    ///
    /// Returns an error when synchronization, Git object access, parsing, or deterministic
    /// reduction cannot complete.
    pub fn shared_domain_snapshot(&self) -> Result<Arc<DomainSnapshot>> {
        let outcome = self.synchronize()?;
        if let Some(cached) = self.cached_domain_snapshot(&outcome.metadata) {
            return Ok(cached);
        }
        let tree = git_tree::read_tree(&self.repository, &outcome.metadata.indexed_tree_oid)?;
        let input = self.build_input(&tree.blobs)?;
        Ok(self.publish_domain_snapshot(&outcome.metadata, input))
    }

    /// Records one reduced projection as the snapshot for exactly `metadata`.
    fn publish_domain_snapshot(
        &self,
        metadata: &IndexMetadata,
        input: project::BuildInput,
    ) -> Arc<DomainSnapshot> {
        let snapshot = Arc::new(DomainSnapshot {
            metadata: metadata.clone(),
            projection: input.projection,
            diagnostics: input
                .diagnostics
                .into_iter()
                .map(|diagnostic| ProjectionDiagnosticView {
                    key: diagnostic.key,
                    source_path: diagnostic.source_path,
                    code: diagnostic.code,
                    entity_id: diagnostic.entity_id,
                    event_ids_json: diagnostic.event_ids_json,
                    message: diagnostic.message,
                })
                .collect(),
        });
        *lock(&self.caches.snapshot) = Some(CachedDomainSnapshot {
            metadata: metadata.clone(),
            snapshot: Arc::clone(&snapshot),
        });
        snapshot
    }

    fn cached_domain_snapshot(&self, metadata: &IndexMetadata) -> Option<Arc<DomainSnapshot>> {
        let cached = lock(&self.caches.snapshot);
        cached
            .as_ref()
            .filter(|entry| &entry.metadata == metadata)
            .map(|entry| Arc::clone(&entry.snapshot))
    }

    #[allow(clippy::too_many_lines)]
    fn synchronize_inner(&self, forced: bool) -> Result<RebuildOutcome> {
        fs::create_dir_all(&self.state).map_err(io_error("create index state directory"))?;
        if !forced {
            if let Some(outcome) = self.current_without_lock()? {
                return Ok(outcome);
            }
        }
        let lock = open_lock(&self.state.join("index.lock"))?;
        lock.lock_exclusive().map_err(io_error("lock index.lock"))?;

        let mut isolated_database = None;
        let mut force_next = forced;
        let mut update_kind = IndexUpdateKind::Current;
        let mut incremental_fallback = None;
        let mut operational_warnings = Vec::new();
        let result = loop {
            let (head_oid, head_entries) = git_tree::list_head(&self.repository)?;
            let database_existed = self.database.exists();
            let (mut connection, isolated) = self.open_healthy_or_replace()?;
            if isolated_database.is_none() {
                isolated_database.clone_from(&isolated);
            }

            let current_metadata = read_metadata_if_present(&connection)?;
            let versions_match = schema::is_complete(&connection)?
                && current_metadata.as_ref().is_some_and(versions_are_current);
            let tree_matches = current_metadata
                .as_ref()
                .is_some_and(|metadata| metadata.indexed_tree_oid == head_oid);
            if !force_next && isolated.is_none() && versions_match && tree_matches {
                break Ok(outcome_from_database(
                    &connection,
                    RebuildReason::Current,
                    isolated_database,
                    update_kind,
                    incremental_fallback,
                    operational_warnings,
                )?);
            }

            let reason = if isolated.is_some() {
                RebuildReason::CorruptDatabase
            } else if !database_existed {
                RebuildReason::MissingDatabase
            } else if force_next {
                RebuildReason::Forced
            } else if !versions_match {
                RebuildReason::ImplementationVersionChanged
            } else {
                RebuildReason::TreeChanged
            };
            let previous_generation = current_metadata
                .as_ref()
                .map_or(0, |metadata| metadata.projection_generation);
            let generation = previous_generation.checked_add(1).ok_or_else(|| {
                invariant("projection generation overflowed its u64 representation")
            })?;

            let can_attempt_incremental = !force_next
                && isolated.is_none()
                && database_existed
                && versions_match
                && current_metadata.is_some();
            let plan = if can_attempt_incremental {
                self.plan_tree_change(
                    &connection,
                    current_metadata.as_ref().expect("checked above"),
                    &head_oid,
                    &head_entries,
                )?
            } else {
                UpdatePlan::Full {
                    input: self
                        .build_input(&git_tree::read_tree(&self.repository, &head_oid)?.blobs)?,
                    fallback: None,
                    // No diff ran (a missing database, a forced rebuild, or an implementation
                    // version change all skip `plan_tree_change` entirely), so this has nothing
                    // fresh to say about append-protocol integrity either.
                    operational_warnings: OperationalWarningsUpdate::CarryForward,
                }
            };
            // Resolved from `&plan` before the shadow-rebuild transaction begins: `CarryForward`
            // reads whatever `meta` currently holds so a synchronization that is not itself a
            // rebuild re-examining Git history integrity does not silently erase a warning a past
            // rebuild found. `replace_projection` and `replace_projection_incremental` only ever
            // *write* this string; they never decide what it should be.
            //
            // `UpdatePlan::Incremental` always carries forward, never writes a fresh verdict, even
            // though the diff that produced it did run and came back clean: an incremental update
            // is not a rebuild (`IndexUpdateKind::Incremental`, not `FullRebuild`), and "the most
            // recent rebuild's warning" -- what `sctx doctor` reports -- has to mean what it says.
            // A past bypass stays visible through however many ordinary incremental syncs follow
            // it, until the next real rebuild re-examines history and updates or clears it.
            let operational_warnings_json = match &plan {
                UpdatePlan::Incremental { .. }
                | UpdatePlan::Full {
                    operational_warnings: OperationalWarningsUpdate::CarryForward,
                    ..
                } => {
                    // Also reached by the very first synchronization a database ever does
                    // (`MissingDatabase`, before `meta` exists at all), so the read is guarded by
                    // `schema::is_complete` -- already evaluated once above as part of
                    // `versions_match` -- rather than assuming the table it names is there to read.
                    if schema::is_complete(&connection)? {
                        schema::read_meta_value(&connection, OPERATIONAL_WARNINGS_META_KEY)?
                            .unwrap_or_else(|| "[]".to_owned())
                    } else {
                        "[]".to_owned()
                    }
                }
                UpdatePlan::Full {
                    operational_warnings: OperationalWarningsUpdate::Fresh(warnings),
                    ..
                } => encode_operational_warnings(warnings)?,
            };
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(sql_error("begin shadow rebuild transaction"))?;
            let projected = match plan {
                UpdatePlan::Incremental {
                    input,
                    affected_spaces,
                } => {
                    schema::replace_projection_incremental(
                        &transaction,
                        &input,
                        &head_oid,
                        generation,
                        &affected_spaces,
                        &operational_warnings_json,
                    )?;
                    if update_kind == IndexUpdateKind::Current {
                        update_kind = IndexUpdateKind::Incremental;
                    }
                    input
                }
                UpdatePlan::Full {
                    input,
                    fallback,
                    operational_warnings: warnings_update,
                } => {
                    schema::replace_projection(
                        &transaction,
                        &input,
                        &head_oid,
                        generation,
                        &operational_warnings_json,
                    )?;
                    update_kind = IndexUpdateKind::FullRebuild;
                    if fallback.is_some() {
                        incremental_fallback = fallback;
                    }
                    if let OperationalWarningsUpdate::Fresh(warnings) = warnings_update {
                        operational_warnings.extend(warnings);
                    }
                    input
                }
            };
            transaction
                .commit()
                .map_err(sql_error("commit shadow rebuild transaction"))?;
            require_quick_check(&connection)?;

            let observed_tree = self.head_tree_oid()?;
            if observed_tree != head_oid {
                force_next = false;
                continue;
            }
            let outcome = outcome_from_database(
                &connection,
                reason,
                isolated_database,
                update_kind,
                incremental_fallback,
                operational_warnings,
            )?;
            // Both plans project the complete new Tree, and `observed_tree` just proved that Tree
            // is still `HEAD`. Reduction is a pure function of exactly that input, so the snapshot
            // this rebuild already computed is the snapshot the next reader would recompute.
            self.publish_domain_snapshot(&outcome.metadata, projected);
            break Ok(outcome);
        };

        let unlock = FileExt::unlock(&lock).map_err(io_error("unlock index.lock"));
        match (result, unlock) {
            (Ok(outcome), Ok(())) => Ok(outcome),
            (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        }
    }

    /// Reads `HEAD^{tree}`, reusing the answer while `HEAD` still names the same commit.
    ///
    /// Every read synchronizes, and every synchronization resolves this twice, so the steady state
    /// used to spend two `git` processes per read. A commit's Tree cannot change, so resolving
    /// `HEAD` from Git's own files is an exact substitute whenever it succeeds.
    fn head_tree_oid(&self) -> Result<String> {
        let head_commit = git_tree::head_commit_oid(&self.repository);
        if let Some(commit) = head_commit.as_ref()
            && let Some(tree) = lock(&self.caches.head_trees).get(commit).cloned()
        {
            return Ok(tree);
        }
        let tree = git_tree::tree_oid(&self.repository)?;
        if let Some(commit) = head_commit {
            let mut remembered = lock(&self.caches.head_trees);
            if remembered.len() >= MAX_REMEMBERED_HEAD_TREES {
                remembered.clear();
            }
            remembered.insert(commit, tree.clone());
        }
        Ok(tree)
    }

    fn current_without_lock(&self) -> Result<Option<RebuildOutcome>> {
        if !self.database.exists() {
            return Ok(None);
        }
        let before = self.head_tree_oid()?;
        let Ok(connection) = self.open_read_only() else {
            return Ok(None);
        };
        if !quick_check_connection(&connection).healthy || !schema::is_complete(&connection)? {
            return Ok(None);
        }
        let Some(metadata) = read_metadata(&connection)? else {
            return Ok(None);
        };
        if !versions_are_current(&metadata) || metadata.indexed_tree_oid != before {
            return Ok(None);
        }
        let outcome = outcome_from_database(
            &connection,
            RebuildReason::Current,
            None,
            IndexUpdateKind::Current,
            None,
            Vec::new(),
        )?;
        let after = self.head_tree_oid()?;
        Ok((before == after).then_some(outcome))
    }

    fn plan_tree_change(
        &self,
        connection: &Connection,
        metadata: &IndexMetadata,
        new_tree_oid: &str,
        new_entries: &[git_tree::TreeEntry],
    ) -> Result<UpdatePlan> {
        let full = |fallback, operational_warnings| -> Result<UpdatePlan> {
            Ok(UpdatePlan::Full {
                input: self
                    .build_input(&git_tree::read_tree(&self.repository, new_tree_oid)?.blobs)?,
                fallback: Some(fallback),
                operational_warnings,
            })
        };
        // Neither branch below ran a diff against the previously indexed generation, so neither
        // has a fresh answer about append-protocol integrity -- whatever `meta` already says
        // stands. Contrast the `AppendProtocolBypassed`, `CachedSourceMismatch`, and
        // `ImpactClosureUnproven` branches further down, all reached only after `changes` was
        // computed successfully.
        if !git_tree::tree_exists(&self.repository, &metadata.indexed_tree_oid) {
            return full(
                IncrementalFallback::IndexedTreeUnavailable,
                OperationalWarningsUpdate::CarryForward,
            );
        }
        let Ok(changes) =
            git_tree::diff_trees(&self.repository, &metadata.indexed_tree_oid, new_tree_oid)
        else {
            return full(
                IncrementalFallback::IndexedTreeUnavailable,
                OperationalWarningsUpdate::CarryForward,
            );
        };
        if changes.iter().any(|change| !change.is_addition()) {
            let paths = changes
                .iter()
                .filter(|change| !change.is_addition())
                .map(|change| match change {
                    git_tree::TreeChange::Renamed { old_path, new_path } => {
                        format!("{old_path} -> {new_path}")
                    }
                    _ => change.path().to_owned(),
                })
                .collect();
            return full(
                IncrementalFallback::AppendProtocolBypassed,
                OperationalWarningsUpdate::Fresh(vec![OperationalWarning {
                    code: "APPEND_PROTOCOL_BYPASSED",
                    paths,
                }]),
            );
        }

        let old_blobs = schema::cached_blobs(connection)?;
        let mut new_blobs = old_blobs.clone();
        for change in &changes {
            if let git_tree::TreeChange::Added { path, oid } = change {
                new_blobs.push(git_tree::TreeBlob {
                    path: path.clone(),
                    oid: oid.clone(),
                    bytes: git_tree::read_blob(&self.repository, oid)?,
                });
            }
        }
        new_blobs.sort_by(|left, right| left.path.cmp(&right.path));
        let expected: BTreeMap<_, _> = new_entries
            .iter()
            .map(|entry| (entry.path.as_str(), entry.oid.as_str()))
            .collect();
        let cached: BTreeMap<_, _> = new_blobs
            .iter()
            .map(|blob| (blob.path.as_str(), blob.oid.as_str()))
            .collect();
        // Reached only once `changes` was computed and confirmed to contain no non-addition
        // change, so append-protocol integrity has a fresh, clean answer here even though the
        // plan still degrades to a full rebuild for an unrelated reason.
        if expected != cached || expected.len() != new_blobs.len() {
            return full(
                IncrementalFallback::CachedSourceMismatch,
                OperationalWarningsUpdate::Fresh(Vec::new()),
            );
        }

        let old_input = self.build_input(&old_blobs)?;
        let new_input = self.build_input(&new_blobs)?;
        let changed_paths: BTreeSet<_> = changes
            .iter()
            .map(|change| change.path().to_owned())
            .collect();
        let affected_spaces = project::impact_closure(&old_input, &new_input, &changed_paths);
        let observed_changes = project::changed_projection_spaces(&old_input, &new_input);
        if !observed_changes.is_subset(&affected_spaces) {
            return full(
                IncrementalFallback::ImpactClosureUnproven,
                OperationalWarningsUpdate::Fresh(Vec::new()),
            );
        }
        Ok(UpdatePlan::Incremental {
            input: new_input,
            affected_spaces,
        })
    }

    fn build_input(&self, blobs: &[git_tree::TreeBlob]) -> Result<project::BuildInput> {
        let mut input = project::build(blobs);
        let wanted = input
            .candidate_events
            .values()
            .map(|metadata| metadata.event_path.clone())
            .chain(
                input
                    .confirmation_events
                    .values()
                    .map(|metadata| metadata.event_path.clone()),
            )
            .chain(input.publication_event_paths.values().cloned())
            .collect::<BTreeSet<_>>();
        let additions = self.event_additions(&wanted)?;
        for metadata in input.candidate_events.values_mut() {
            metadata.commit_oid = Some(introducing_commit_oid(&additions, &metadata.event_path)?);
        }
        for metadata in input.confirmation_events.values_mut() {
            metadata.commit_oid = Some(introducing_commit_oid(&additions, &metadata.event_path)?);
        }
        input.publication_times = input
            .publication_event_paths
            .iter()
            .filter_map(|(publication_id, path)| {
                additions
                    .get(path)
                    .and_then(|entries| entries.first())
                    .map(|entry| (*publication_id, entry.commit_time))
            })
            .collect();
        Ok(input)
    }

    /// Answers `event_path -> introducing commits` for `wanted`, spending at most one Git process.
    ///
    /// Event paths are append-only, so an answer for a path never changes: this memo is warmed
    /// from the projection database once per process, and only a path it has never seen forces
    /// the single history walk that re-answers every path at once.
    fn event_additions(
        &self,
        wanted: &BTreeSet<String>,
    ) -> Result<BTreeMap<String, Vec<git_tree::EventAddition>>> {
        if wanted.is_empty() {
            return Ok(BTreeMap::new());
        }
        let mut cache = lock(&self.caches.event_commits);
        if !cache.warmed {
            cache.warmed = true;
            if let Ok(connection) = self.open_read_only()
                && let Ok(persisted) = schema::read_event_commits(&connection)
            {
                cache.entries.extend(persisted);
            }
        }
        if wanted.iter().any(|path| !cache.entries.contains_key(path)) {
            let scanned = git_tree::introducing_commits(&self.repository)?;
            self.persist_event_commits(&scanned);
            cache.entries.extend(scanned);
        }
        Ok(wanted
            .iter()
            .filter_map(|path| {
                cache
                    .entries
                    .get(path)
                    .map(|entries| (path.clone(), entries.clone()))
            })
            .collect())
    }

    /// Best-effort write of the Event introduction memo; a failure only costs the next walk.
    fn persist_event_commits(&self, additions: &BTreeMap<String, Vec<git_tree::EventAddition>>) {
        if !self.database.exists() {
            return;
        }
        let Ok(mut connection) = Connection::open_with_flags(
            &self.database,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) else {
            return;
        };
        if connection
            .busy_timeout(std::time::Duration::from_secs(3))
            .is_err()
        {
            return;
        }
        let _ = schema::write_event_commits(&mut connection, additions);
    }

    fn open_healthy_or_replace(&self) -> Result<(Connection, Option<PathBuf>)> {
        if !self.database.exists() {
            let connection = Connection::open(&self.database)
                .map_err(sql_error("create projection database"))?;
            configure(&connection)?;
            return Ok((connection, None));
        }

        let connection = Connection::open(&self.database)
            .map_err(sql_error("open existing projection database"))?;
        if quick_check_connection(&connection).healthy {
            configure(&connection)?;
            return Ok((connection, None));
        }
        drop(connection);
        let quarantined = quarantine_database(&self.database)?;
        let connection = Connection::open(&self.database)
            .map_err(sql_error("create replacement projection database"))?;
        configure(&connection)?;
        Ok((connection, Some(quarantined)))
    }

    /// Records the local-only `context_item.stale_reason` derivation for a set of Contexts.
    ///
    /// `stale_reason` is *not* a Git fact: it is the outcome of evaluating a Context's structured
    /// `recheck_when` entries against the current local checkouts. It therefore lives only in this
    /// machine's projection and is cleared whenever the projection is rebuilt or a Space closure
    /// is replaced; re-run the evaluator (`sctx doctor --recheck`) after new Events land.
    ///
    /// Contexts absent from `reasons` keep whatever they already carry; pass `None` to clear one.
    ///
    /// # Errors
    ///
    /// Returns an error when synchronization or the write transaction fails.
    pub fn record_stale_reasons(&self, reasons: &[(String, Option<String>)]) -> Result<usize> {
        self.synchronize()?;
        let mut connection = Connection::open(&self.database)
            .map_err(sql_error("open projection for stale write"))?;
        configure(&connection)?;
        let transaction = connection
            .transaction()
            .map_err(sql_error("begin stale-reason transaction"))?;
        let mut updated = 0;
        {
            let mut statement = transaction
                .prepare("UPDATE context_item SET stale_reason = ?2 WHERE context_id = ?1")
                .map_err(sql_error("prepare stale-reason update"))?;
            for (context_id, reason) in reasons {
                updated += statement
                    .execute(rusqlite::params![context_id, reason])
                    .map_err(sql_error("write stale-reason derivation"))?;
            }
        }
        transaction
            .commit()
            .map_err(sql_error("commit stale-reason transaction"))?;
        Ok(updated)
    }

    fn open_read_only(&self) -> Result<Connection> {
        if !self.database.exists() {
            return Err(invariant("projection database does not exist"));
        }
        Connection::open_with_flags(
            &self.database,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(sql_error("open projection database read-only"))
    }
}

impl QueryConnection {
    /// Synchronizes first, then runs `query` after pinning metadata and all page reads to one
    /// read-only `SQLite` transaction.
    ///
    /// # Errors
    ///
    /// Returns an error when synchronization, connection reopening, transaction control, or the
    /// caller query fails.
    pub fn snapshot<T>(
        &mut self,
        query: impl FnOnce(&Connection) -> Result<T>,
    ) -> Result<QuerySnapshot<T>> {
        loop {
            let synchronized = self.index.synchronize()?;
            let disk_identity = database_file_identity(&self.index.database)?;
            if self.identity.as_ref() != Some(&disk_identity) {
                self.connection = None;
                self.connection = Some(self.index.open_read_only()?);
                self.identity = Some(disk_identity.clone());
            }
            let connection = self
                .connection
                .as_ref()
                .ok_or_else(|| invariant("query connection was not opened"))?;
            connection
                .execute_batch("BEGIN DEFERRED TRANSACTION;")
                .map_err(sql_error("begin query snapshot"))?;
            let metadata = read_metadata(connection)?;
            let still_same_file = database_file_identity(&self.index.database)
                .is_ok_and(|identity| identity == disk_identity);
            let synchronized_generation = metadata
                .as_ref()
                .is_some_and(|value| value == &synchronized.metadata);
            if !still_same_file || !synchronized_generation {
                let _ = connection.execute_batch("ROLLBACK;");
                self.connection = None;
                self.identity = None;
                continue;
            }
            let Some(metadata) = metadata else {
                let _ = connection.execute_batch("ROLLBACK;");
                self.connection = None;
                self.identity = None;
                continue;
            };
            let result = query(connection);
            match result {
                Ok(data) => {
                    connection
                        .execute_batch("COMMIT;")
                        .map_err(sql_error("commit query snapshot"))?;
                    return Ok(QuerySnapshot { metadata, data });
                }
                Err(error) => {
                    let _ = connection.execute_batch("ROLLBACK;");
                    return Err(error);
                }
            }
        }
    }
}

impl sctx_git_store::CommitObserver for ProjectionIndex {
    fn committed(&self, repository: &Path, _commit_oid: &str) -> Result<()> {
        let expected = fs::canonicalize(&self.repository)
            .map_err(io_error("canonicalize index repository"))?;
        let actual =
            fs::canonicalize(repository).map_err(io_error("canonicalize committed repository"))?;
        if actual != expected {
            return Err(invariant(format!(
                "index observer was called for another repository: {}",
                actual.display()
            )));
        }
        self.synchronize().map(|_| ())
    }
}

impl CandidateSubmissionIndex for ProjectionIndex {
    fn synchronize(&self) -> Result<()> {
        ProjectionIndex::synchronize(self).map(|_| ())
    }

    fn lookup(&self, submission_id: SubmissionId) -> Result<CandidateSubmissionLookup> {
        let connection = self.open_read_only()?;
        if let Some((event_ids_json, candidate_ids_json, content_hashes_json)) = connection
            .query_row(
                "SELECT event_ids_json, candidate_ids_json, content_hashes_json
                 FROM candidate_submission_conflict WHERE submission_id = ?1",
                [submission_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(sql_error("read Candidate submission conflict"))?
        {
            return Ok(CandidateSubmissionLookup::Conflict {
                submission_id,
                event_ids: serde_json::from_str(&event_ids_json)
                    .map_err(json_error("parse Candidate conflict Event IDs"))?,
                candidate_ids: serde_json::from_str(&candidate_ids_json)
                    .map_err(json_error("parse Candidate conflict Candidate IDs"))?,
                content_hashes: serde_json::from_str(&content_hashes_json)
                    .map_err(json_error("parse Candidate conflict content hashes"))?,
            });
        }
        let row = connection
            .query_row(
                "SELECT candidate_id, event_id, source_episode_id,
                        source_task_session_id, source_task_id, content_hash,
                        batch_id, commit_oid, event_path
                 FROM candidate_submission WHERE submission_id = ?1",
                [submission_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, String>(8)?,
                    ))
                },
            )
            .optional()
            .map_err(sql_error("read Candidate submission"))?;
        let Some((
            candidate_id,
            event_id,
            episode_id,
            task_session_id,
            task_id,
            content_hash,
            batch_id,
            commit_oid,
            event_path,
        )) = row
        else {
            return Ok(CandidateSubmissionLookup::NotFound);
        };
        Ok(CandidateSubmissionLookup::Found(
            CandidateSubmissionRecord {
                submission_id,
                candidate_id: CandidateId::from_str(&candidate_id)
                    .map_err(|error| invariant(format!("invalid indexed CandidateId: {error}")))?,
                event_id: EventId::from_str(&event_id)
                    .map_err(|error| invariant(format!("invalid indexed EventId: {error}")))?,
                source_episode: WorkEpisodeRef {
                    episode_id: WorkEpisodeId::from_str(&episode_id).map_err(|error| {
                        invariant(format!("invalid indexed WorkEpisodeId: {error}"))
                    })?,
                    task_session_id: TaskSessionId::from_str(&task_session_id).map_err(
                        |error| invariant(format!("invalid indexed TaskSessionId: {error}")),
                    )?,
                    task_id: TaskId::from_str(&task_id)
                        .map_err(|error| invariant(format!("invalid indexed TaskId: {error}")))?,
                },
                content_hash,
                batch_id: BatchId::from_str(&batch_id)?,
                commit_oid,
                event_path,
            },
        ))
    }
}

impl CandidateConfirmationIndex for ProjectionIndex {
    fn synchronize(&self) -> Result<()> {
        ProjectionIndex::synchronize(self).map(|_| ())
    }

    fn lookup(&self, candidate_id: CandidateId) -> Result<CandidateConfirmationLookup> {
        let connection = self.open_read_only()?;
        if let Some((confirmation_ids_json, event_ids_json)) = connection
            .query_row(
                "SELECT confirmation_ids_json, event_ids_json
                 FROM candidate_confirmation_conflict WHERE candidate_id = ?1",
                [candidate_id.to_string()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(sql_error("read Candidate Confirmation conflict"))?
        {
            return Ok(CandidateConfirmationLookup::Conflict {
                candidate_id,
                confirmation_ids: serde_json::from_str(&confirmation_ids_json)
                    .map_err(json_error("parse Confirmation conflict IDs"))?,
                event_ids: serde_json::from_str(&event_ids_json)
                    .map_err(json_error("parse Confirmation conflict Event IDs"))?,
            });
        }
        let row = connection
            .query_row(
                "SELECT confirmation_id, result_context_id, operation_hash, plan_hash,
                        batch_id, commit_oid, event_ids_json, event_paths_json
                 FROM candidate_confirmation WHERE candidate_id = ?1",
                [candidate_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, String>(7)?,
                    ))
                },
            )
            .optional()
            .map_err(sql_error("read Candidate Confirmation mapping"))?;
        let Some((
            confirmation_id,
            result_context_id,
            operation_hash,
            plan_hash,
            batch_id,
            commit_oid,
            event_ids_json,
            event_paths_json,
        )) = row
        else {
            return Ok(CandidateConfirmationLookup::NotFound);
        };
        Ok(CandidateConfirmationLookup::Found(
            CandidateConfirmationRecord {
                candidate_id,
                confirmation_id: ConfirmationId::from_str(&confirmation_id).map_err(|error| {
                    invariant(format!("invalid indexed ConfirmationId: {error}"))
                })?,
                result_context_id: ContextId::from_str(&result_context_id)
                    .map_err(|error| invariant(format!("invalid indexed ContextId: {error}")))?,
                operation_hash,
                plan_hash,
                batch_id: BatchId::from_str(&batch_id)?,
                commit_oid,
                event_ids: serde_json::from_str(&event_ids_json)
                    .map_err(json_error("parse Confirmation batch Event IDs"))?,
                event_paths: serde_json::from_str(&event_paths_json)
                    .map_err(json_error("parse Confirmation batch Event paths"))?,
            },
        ))
    }
}

fn configure(connection: &Connection) -> Result<()> {
    configure_sqlite(connection).map_err(sql_error("configure SQLite connection"))?;
    let pragmas = read_pragmas(connection)?;
    if pragmas.journal_mode != "wal"
        || pragmas.synchronous != 1
        || !pragmas.foreign_keys
        || pragmas.busy_timeout_ms != 3_000
    {
        return Err(invariant(format!(
            "SQLite PRAGMAs were not applied: {pragmas:?}"
        )));
    }
    Ok(())
}

fn configure_sqlite(connection: &Connection) -> rusqlite::Result<()> {
    connection.busy_timeout(std::time::Duration::from_secs(3))?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

fn read_pragmas(connection: &Connection) -> Result<DatabasePragmas> {
    Ok(DatabasePragmas {
        journal_mode: connection
            .query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))
            .map_err(sql_error("read journal_mode"))?
            .to_ascii_lowercase(),
        synchronous: connection
            .query_row("PRAGMA synchronous", [], |row| row.get(0))
            .map_err(sql_error("read synchronous"))?,
        foreign_keys: connection
            .query_row("PRAGMA foreign_keys", [], |row| row.get::<_, i64>(0))
            .map_err(sql_error("read foreign_keys"))?
            != 0,
        busy_timeout_ms: connection
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .map_err(sql_error("read busy_timeout"))?,
    })
}

fn quick_check_connection(connection: &Connection) -> QuickCheck {
    let result = (|| -> rusqlite::Result<Vec<String>> {
        let mut statement = connection.prepare("PRAGMA quick_check")?;
        statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect()
    })();
    match result {
        Ok(rows) if rows.as_slice() == ["ok"] => QuickCheck {
            healthy: true,
            detail: "ok".to_owned(),
        },
        Ok(rows) => QuickCheck {
            healthy: false,
            detail: rows.join("; "),
        },
        Err(error) => QuickCheck {
            healthy: false,
            detail: error.to_string(),
        },
    }
}

fn require_quick_check(connection: &Connection) -> Result<()> {
    let check = quick_check_connection(connection);
    if check.healthy {
        Ok(())
    } else {
        Err(invariant(format!(
            "SQLite quick_check failed: {}",
            check.detail
        )))
    }
}

fn read_metadata_if_present(connection: &Connection) -> Result<Option<IndexMetadata>> {
    let has_meta = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'meta')",
            [],
            |row| row.get::<_, bool>(0),
        )
        .map_err(sql_error("inspect projection schema"))?;
    if has_meta {
        let compatible_columns = connection
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('meta') WHERE name IN ('key', 'value')",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map_err(sql_error("inspect projection metadata layout"))?;
        if compatible_columns == 2 {
            read_metadata(connection)
        } else {
            Ok(None)
        }
    } else {
        Ok(None)
    }
}

fn read_metadata(connection: &Connection) -> Result<Option<IndexMetadata>> {
    let Some(indexed_tree_oid) = schema::read_meta_value(connection, "indexed_tree_oid")? else {
        return Ok(None);
    };
    let Some(projection_generation) = schema::read_generation(connection)? else {
        return Ok(None);
    };
    let version = |key| schema::read_meta_value(connection, key);
    let Some(db_schema_version) = version("db_schema_version")? else {
        return Ok(None);
    };
    let Some(event_parser_version) = version("event_parser_version")? else {
        return Ok(None);
    };
    let Some(reducer_version) = version("reducer_version")? else {
        return Ok(None);
    };
    let Some(conflict_detector_version) = version("conflict_detector_version")? else {
        return Ok(None);
    };
    let Some(normalizer_tokenizer_version) = version("normalizer_tokenizer_version")? else {
        return Ok(None);
    };
    let Some(search_ranking_version) = version("search_ranking_version")? else {
        return Ok(None);
    };
    Ok(Some(IndexMetadata {
        indexed_tree_oid,
        projection_generation,
        db_schema_version,
        event_parser_version,
        reducer_version,
        conflict_detector_version,
        normalizer_tokenizer_version,
        search_ranking_version,
    }))
}

/// Resolves the one commit that introduced an append-only Event path.
///
/// The zero and many cases stay hard errors: a Candidate or Confirmation Event whose introduction
/// is not unique has no defensible publication identity.
fn introducing_commit_oid(
    additions: &BTreeMap<String, Vec<git_tree::EventAddition>>,
    path: &str,
) -> Result<String> {
    match additions.get(path).map(Vec::as_slice) {
        Some([entry]) => Ok(entry.commit_oid.clone()),
        None | Some([]) => Err(Error::new(
            ErrorKind::External,
            format!("Candidate event has no introducing commit: {path}"),
        )),
        Some(_) => Err(Error::new(
            ErrorKind::External,
            format!("Candidate event has multiple introducing commits: {path}"),
        )),
    }
}

fn versions_are_current(metadata: &IndexMetadata) -> bool {
    metadata.db_schema_version == DB_SCHEMA_VERSION
        && metadata.event_parser_version == EVENT_PARSER_VERSION
        && metadata.reducer_version == REDUCER_VERSION
        && metadata.conflict_detector_version == CONFLICT_DETECTOR_VERSION
        && metadata.normalizer_tokenizer_version == NORMALIZER_TOKENIZER_VERSION
        && metadata.search_ranking_version == SEARCH_RANKING_VERSION
}

fn outcome_from_database(
    connection: &Connection,
    reason: RebuildReason,
    quarantined_database: Option<PathBuf>,
    update_kind: IndexUpdateKind,
    incremental_fallback: Option<IncrementalFallback>,
    operational_warnings: Vec<OperationalWarning>,
) -> Result<RebuildOutcome> {
    let metadata = read_metadata(connection)?
        .ok_or_else(|| invariant("rebuilt projection is missing metadata"))?;
    let source_file_count = count(connection, "source_file")?;
    let diagnostic_count = count(connection, "diagnostic")?;
    Ok(RebuildOutcome {
        rebuilt: update_kind == IndexUpdateKind::FullRebuild,
        reason,
        metadata,
        source_file_count,
        diagnostic_count,
        update_kind,
        incremental_fallback,
        operational_warnings,
        quarantined_database,
    })
}

#[cfg(unix)]
fn database_file_identity(path: &Path) -> Result<DatabaseFileIdentity> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = fs::metadata(path).map_err(io_error("inspect projection database identity"))?;
    Ok(DatabaseFileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(not(unix))]
fn database_file_identity(path: &Path) -> Result<DatabaseFileIdentity> {
    let metadata = fs::metadata(path).map_err(io_error("inspect projection database identity"))?;
    let modified = metadata
        .modified()
        .map_err(io_error("inspect projection database modification time"))?
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| invariant(format!("projection database predates Unix epoch: {error}")))?;
    Ok(DatabaseFileIdentity {
        device: metadata.len(),
        inode: modified.as_nanos() as u64,
    })
}

fn count(connection: &Connection, table: &str) -> Result<u64> {
    connection
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .map_err(sql_error("count projection rows"))
}

fn quarantine_database(database: &Path) -> Result<PathBuf> {
    let suffix = format!("corrupt-{}", uuid::Uuid::new_v4().hyphenated());
    let mut target_name = database
        .file_name()
        .ok_or_else(|| invariant("database path has no file name"))?
        .to_os_string();
    target_name.push(".");
    target_name.push(suffix);
    let target = database.with_file_name(target_name);
    fs::rename(database, &target).map_err(io_error("isolate corrupt projection database"))?;
    for suffix in ["-wal", "-shm"] {
        let source = append_to_path(database, suffix)?;
        if source.exists() {
            let destination = append_to_path(&target, suffix)?;
            fs::rename(&source, &destination)
                .map_err(io_error("isolate corrupt SQLite sidecar"))?;
        }
    }
    Ok(target)
}

fn append_to_path(path: &Path, suffix: &str) -> Result<PathBuf> {
    let mut name = path
        .file_name()
        .ok_or_else(|| invariant("SQLite path has no file name"))?
        .to_os_string();
    name.push(suffix);
    Ok(path.with_file_name(name))
}

fn open_lock(path: &Path) -> Result<File> {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .map_err(io_error("open index lock"))
}

pub(crate) fn sql_error(context: &'static str) -> impl FnOnce(rusqlite::Error) -> Error {
    move |error| Error::new(ErrorKind::Io, format!("{context}: {error}"))
}

fn json_error(context: &'static str) -> impl FnOnce(serde_json::Error) -> Error {
    move |error| Error::new(ErrorKind::InvariantViolation, format!("{context}: {error}"))
}

fn invariant(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvariantViolation, message)
}

fn io_error(context: &'static str) -> impl FnOnce(std::io::Error) -> Error {
    move |error| Error::new(ErrorKind::Io, format!("{context}: {error}"))
}
