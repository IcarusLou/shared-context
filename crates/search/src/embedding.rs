//! Local embedding recall channel (ADR-0004).
//!
//! Lexical recall reached its ceiling at pure synonymy: `砍掉` and `排除` share no token, and no
//! field weight can build a bridge that the text never wrote down. This module supplies the one
//! extra fusion channel that can, under three standing constraints.
//!
//! * **It is opt-in.** Nothing here runs unless `[retrieval]` names both a model directory and an
//!   ONNX Runtime library. An installation that configured neither behaves exactly as it did
//!   before this module existed -- not approximately, and not "with an empty channel": the
//!   [`crate::SearchEngine`] holds `None` and never reports an omission it had no channel to fail.
//! * **It never blocks.** Corpus vectors are a discardable local cache filled by a background
//!   thread. Retrieval reads that cache and nothing else: every comparison ADR-0007 makes is
//!   between two vectors the backfill already wrote, so no retrieval can acquire a model call,
//!   and a channel that has not finished loading reports `embedding_unavailable` while the
//!   lexical channels answer unchanged.
//!
//!   There was a query path here, and the way it failed is worth keeping in view. It encoded the
//!   caller's text under a wall-clock budget, and that budget was set for a *short* query because
//!   the probe suite that validated it asked short questions. A real query was a Working Intent
//!   flattened to a few hundred characters, the encode cost scales with length, and the result
//!   was a channel that loaded a two-gigabyte model, embedded the whole corpus, reported no
//!   error, and contributed to nothing in any real session. What retired it in the end was not
//!   the budget: measured on a real installation, the intent-text path took its top hit from an
//!   unrelated topic every time, which is a ranking signal being asked to make an admission
//!   decision. ADR-0007 replaced it with the second hop, and this module now has no query side at
//!   all.
//! * **It is one channel, never the answer.** The prototype that motivated ADR-0004 lost
//!   identifier queries outright and could not tell a near-duplicate from its neighbour. Its
//!   output is fused, never substituted.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, RwLock},
    time::{Duration, UNIX_EPOCH},
};

use rusqlite::Connection;
use sctx_domain::{Error, ErrorKind, Result, RevisionId};
use sha2::{Digest, Sha256};

#[cfg(feature = "embedding-onnx")]
pub mod onnx;

/// Cosine a candidate Context must reach against a *seed Context* to be admitted by the second hop.
///
/// Every other floor in this file cuts a ranked list produced by a query. This one does not cut a
/// list at all: the second hop starts from a Context the Session already touched, compares it
/// document-against-document with each candidate, and either admits the candidate into the Pack or
/// drops it. Nothing downstream re-tests the admitted Context for relevance, so the number has to
/// carry the whole decision -- which is why it is derived from the *highest scoring negative* and
/// not, as a ranking floor would be, from the lowest positive worth keeping.
///
/// ## Derivation
///
/// Measured 2026-09-11 on Apple Silicon / macOS 24.6.0, ONNX Runtime 1.28.1, the
/// `codefuse-ai/F2LLM-v2-0.6B` Hub export, release profile, by
/// `crates/search/tests/embedding_hop2_admission_calibration.rs` over
/// `fixtures/association/hard-negative-v1.json`: 24 Contexts, three topic families across two
/// repository labels, all 276 unordered pairs encoded through the document path and grouped by
/// whether the two share a repository and whether they share a topic.
///
/// | group | n | p50 | max |
/// |---|---|---|---|
/// | same-repo/same-topic | 38 | 5061 | 7598 |
/// | cross-repo/same-topic | 47 | 4631 | 7447 |
/// | same-repo/cross-topic | 98 | 2563 | **5045** |
/// | cross-repo/cross-topic | 93 | 2549 | 4498 |
///
/// AUC 0.9097 over same-topic against cross-topic. The binding number is the bolded one: 5045, the
/// highest any pair of Contexts about *different* topics reaches, and it comes from the hardest
/// group by construction -- two Contexts in the same repository, sharing the product's whole
/// vocabulary while describing unrelated work. 5200 clears it by 155 basis points and admits 0 of
/// 191 cross-topic pairs; it retains 13 of 47 cross-repository joins and 18 of 38 same-repository
/// ones.
///
/// ## Why 5200 and not the 5140 the device run suggested
///
/// The same four-group comparison was run on a real installation's Contexts during the Step 0a
/// experiment behind ADR-0007 -- 83/66/122/54 pairs over one Android and one web checkout -- and it
/// put the cross-topic ceiling at 5130, the best threshold at 5140 (100% precision, 95.5%
/// cross-repository recall) and the whole 5000--5500 range on a plateau. Two corpora, one synthetic
/// and adversarial, one real, therefore place the boundary within a hundred basis points of each
/// other, which is the only reason to trust either.
///
/// The value is the *union* of the two rather than the better of them. 5140 clears this fixture's
/// ceiling comfortably but sits only 10 basis points above the device's, which is a reading of one
/// pair and not a threshold; 5100 clears this fixture but would admit the device's hardest
/// cross-topic pair outright. 5200 is above both ceilings, costs one cross-repository join against
/// 5140 on the device (62/66 rather than 63/66), and costs nothing here -- 5100 and 5200 retain the
/// same 13 of 47. Erring high is the standing rule: a Context nobody asked for is worse than no
/// Context.
///
/// Its numerical equality with [`SEMANTIC_SIMILARITY_FLOOR_BASIS_POINTS`] is a coincidence of two
/// unrelated measurements in two unrelated embedding spaces. Neither is evidence about the other
/// and they must not be merged.
///
/// ## Recall differs between the two corpora, and that is a property of the corpora
///
/// 95.5% on the device against 28% here. The device's same-topic family was one feature's work in
/// one week, so its Contexts repeat each other's field names; this fixture's families span a
/// topic's whole history and include two deliberate hard cases per the fixture's
/// `known_hard_cases` -- cross-cutting knowledge (a contrast-ratio finding that shares almost no
/// vocabulary with its own family) and entity-free validations (procedural prose with no
/// identifier to anchor on). Those are the shapes the device run also lost. A real second hop seeds
/// from a Context the Session just touched and reaches the Contexts written around it, which is the
/// device's shape; this fixture's number is the pessimistic end of the range, kept pessimistic on
/// purpose so the floor is not tuned against an easy positive set.
///
/// ## Pre-registered recalibration
///
/// This is a synthetic corpus cross-checked against one installation, which is one installation
/// more than any floor in this file previously had and still not a distribution. Before this
/// governs injections in the field it becomes a configuration key with its admitted/refused score
/// distribution recorded per decision, and the value is re-derived from that record once real
/// traffic has accumulated -- the same discipline ADR-0004 wrote for the encode budget after
/// setting it twice from the wrong machine. The device run's own limits are the ones to close
/// first: its positives were single-topic (one feature family), and its iOS checkout held no
/// Contexts at all, so the cross-repository claim rests on two repositories rather than three.
pub const SEMANTIC_HOP2_ADMISSION_FLOOR_BASIS_POINTS: u16 = 5_200;

/// Second-hop admission decisions kept in the discardable cache.
///
/// This is the data source ADR-0007 pre-registered for re-deriving
/// [`SEMANTIC_HOP2_ADMISSION_FLOOR_BASIS_POINTS`] from real traffic, and a threshold is re-derived
/// from a *distribution* -- one pass of the second hop over a 26-Context installation with four
/// seeds already writes about a hundred rows, so a window of a few dozen would hold a single
/// retrieval's worth of them and answer nothing. At roughly 100 bytes a row this is a few hundred
/// kilobytes in a file that already holds megabytes of floats, which keeps it what the rest of
/// `semantic.sqlite` is: derived data an operator may delete at any moment.
pub const SEMANTIC_HOP2_SAMPLE_HISTORY: usize = 4_096;

/// Longest text, in model tokens, handed to the encoder. bge-m3 accepts far more; a Context's
/// statement, rationale and problem view do not need them, and truncation bounds what one corpus
/// encode can cost the backfill.
pub const SEMANTIC_MAX_TOKENS: usize = 512;

/// Turns text into a unit-length vector in a fixed embedding space.
///
/// Implementations must be deterministic: the vector cache is keyed on a model fingerprint, so the
/// same text encoded twice by the same model has to produce the same vector or a cached corpus and
/// a freshly encoded query stop being comparable.
pub trait EmbeddingProvider: Send + Sync {
    /// Dimensionality every vector this provider returns must have.
    fn dimensions(&self) -> usize;

    /// Encodes one text into an L2-normalized vector.
    ///
    /// # Errors
    ///
    /// Returns a typed error when the text cannot be tokenized or the model cannot be run.
    fn encode(&self, text: &str) -> Result<Vec<f32>>;

    /// Encodes corpus text nobody is waiting for, yielding the encoder to queries.
    ///
    /// The vector must be identical to [`EmbeddingProvider::encode`]'s -- corpus and query vectors
    /// are compared against each other, so a different answer here would silently break ranking.
    /// The only difference is scheduling: a provider that serialises encodes should let every
    /// waiting query go first, because the backfill runs behind a channel that is already
    /// published and answering.
    ///
    /// The default is `encode`, which is right for any provider whose encodes do not contend.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`EmbeddingProvider::encode`].
    fn encode_bulk(&self, text: &str) -> Result<Vec<f32>> {
        self.encode(text)
    }
}

/// One published generation of corpus vectors, shared by every reader of the same channel.
pub type DocumentVectorSnapshot = Arc<Vec<(RevisionId, Vec<f32>)>>;

/// The retrieval-time face of the channel, so retrieval never depends on how vectors are produced.
///
/// It has no query method. It had one -- `similar_revisions`, which encoded the caller's text and
/// ranked the corpus against it -- and ADR-0007 retired the only path that called it: automatic
/// injection reaches knowledge through the files a Session touched, and the second hop compares
/// one stored Context vector against another. Explicit `context_search` builds its own engine and
/// has never held a channel. So nothing in production ever asked this trait a question again, and
/// the method, its floors, its budget, its query cache and its encode telemetry are gone rather
/// than left as a surface that reads as live.
pub trait SemanticChannel: Send + Sync {
    /// The cached document vectors this channel was published over.
    ///
    /// ADR-0007's second hop compares a seed Context against a candidate Context, so both sides are
    /// corpus vectors the backfill already wrote and neither side is a query. That is why this
    /// returns the snapshot rather than answering a question: the lane does the comparing, the
    /// channel only owns the vectors, and nothing on this path can acquire a model call.
    ///
    /// `None` means the channel cannot serve the hop at all -- the model has not loaded yet, or the
    /// implementation has no corpus snapshot. It is a different fact from `Some` of an empty
    /// snapshot, which is a channel that is running over a corpus the backfill has not reached, and
    /// only the first one is worth an omission line.
    fn document_vectors(&self) -> Option<DocumentVectorSnapshot> {
        None
    }

    /// Records what the second hop decided about each pair it judged.
    ///
    /// Best effort by contract: ADR-0007 pre-registered the record so the floor can be re-derived
    /// from real traffic, and a diagnostic that can fail a retrieval is worse than no diagnostic.
    fn record_hop2_admissions(&self, _samples: &[Hop2AdmissionSample]) {}
}

/// The generation of the rule `SearchEngine::embeddable_revisions` applies to build one corpus
/// text.
///
/// `SEARCH_RANKING_VERSION` cannot carry this on its own, and the attempt to make it is what let
/// the 2026-09-10 corpus defect survive. That constant versions what the *lexical* index holds;
/// the semantic corpus is a second reading of the same three revision fields, and the two move
/// independently. A change to the join, the field set, the separator or the source table changes
/// what every cached vector means while leaving BM25 byte-identical, and nothing in the ranking
/// version would notice.
///
/// - `"1"`: `statement`/`rationale`/`problem_view` read from `context_fts`, i.e. after
///   `normalize_search_text`. Never intended; see `SearchEngine::embeddable_revisions`.
/// - `"2"`: the same three fields read from `context_revision` as written, joined with a newline.
///
/// Bumping it invalidates every cached vector at once: [`SemanticCacheKey::new`] folds it into the
/// key, so `prune_superseded` reclaims the old generation and `cached_revisions` reports the whole
/// corpus as missing, which is exactly what makes the next backfill a full re-encode.
pub const SEMANTIC_CORPUS_VERSION: &str = "2";

/// Identifies one generation of cached vectors.
///
/// Both halves matter. `model_fingerprint` changes when the operator swaps model files, and
/// comparing vectors from two different models is meaningless rather than merely inaccurate. The
/// other half answers "what text was fed to that model", and it has two independent inputs:
/// `SEARCH_RANKING_VERSION`, which moves when the indexed revision content changes shape, and
/// [`SEMANTIC_CORPUS_VERSION`], which moves when the rule that turns a revision into one corpus
/// text changes. [`SemanticCacheKey::new`] takes the first and folds in the second.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticCacheKey {
    /// Content fingerprint of the model files the vectors were produced by.
    pub model_fingerprint: String,
    /// The corpus generation the vectors were produced under: the `SEARCH_RANKING_VERSION` in
    /// force, with [`SEMANTIC_CORPUS_VERSION`] folded in by [`SemanticCacheKey::new`]. It is the
    /// composed string, not the ranking version alone -- read it as an opaque generation tag.
    pub ranking_version: String,
}

impl SemanticCacheKey {
    /// Builds a key from a model fingerprint and the ranking version in force.
    ///
    /// The stored `ranking_version` is `"<ranking_version>+corpus<SEMANTIC_CORPUS_VERSION>"`, so a
    /// caller passing `SEARCH_RANKING_VERSION` gets invalidation on either input without having to
    /// know that the second one exists.
    #[must_use]
    pub fn new(model_fingerprint: impl Into<String>, ranking_version: impl Into<String>) -> Self {
        let ranking_version = ranking_version.into();
        Self {
            model_fingerprint: model_fingerprint.into(),
            ranking_version: format!("{ranking_version}+corpus{SEMANTIC_CORPUS_VERSION}"),
        }
    }
}

/// One second-hop admission decision, as it was taken.
///
/// Every pair the hop judged produces one of these, refused pairs included. The refused ones are
/// the more valuable half: [`SEMANTIC_HOP2_ADMISSION_FLOOR_BASIS_POINTS`] is derived from the
/// highest-scoring negative, so a record that held only what was admitted could never say whether
/// the floor sits too high or too low -- it would be the fixture's positive set again, measured in
/// the field.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
pub struct Hop2AdmissionSample {
    /// The seed Context's accepted revision -- the side the Session had already touched.
    pub seed_revision_id: RevisionId,
    /// The candidate revision the seed was compared against.
    pub candidate_revision_id: RevisionId,
    /// Cosine of the two document vectors, in basis points, by the same rounding the floor is
    /// compared with.
    pub score_basis_points: u16,
    /// Whether that score reached the floor in force when the decision was taken.
    ///
    /// Stored rather than recomputed from the score, because the floor is a configuration key: a
    /// row written under an operator's tuned floor has to stay readable as the decision that was
    /// actually made, not as the decision today's constant would have made.
    pub admitted: bool,
}

/// One recorded decision, with the wall clock it was taken at.
///
/// The clock is the writer's, not the caller's: a sample is a fact about when the installation
/// retrieved, and letting a caller stamp it would let two callers disagree about the ordering of
/// one history.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
pub struct RecordedHop2Admission {
    /// Seconds since the Unix epoch, as the writing process read them.
    pub recorded_at_unix_seconds: i64,
    pub sample: Hop2AdmissionSample,
}

/// Where a channel reports what the second hop decided. Implemented by [`SemanticVectorCache`];
/// absent on every channel that has no discardable cache to write to, such as the in-memory test
/// ones.
///
/// It was `EncodeSampleRecorder` and watched query encodes as well. That half went with the query
/// path: an encode history answered "is this budget fit for this machine", and with no query
/// encode there is no budget and no machine question to answer.
pub trait Hop2AdmissionRecorder: Send + Sync {
    /// Records one pass of the second hop's admission decisions.
    ///
    /// Failure is not reportable: a diagnostic that can fail a retrieval is worse than no
    /// diagnostic.
    fn record_hop2_admissions(&self, samples: &[Hop2AdmissionSample]);
}

/// Fingerprints a model directory from the size and modification time of the files that define it.
///
/// Hashing 2 GB of ONNX weights on every serve start would cost more than the load it guards, and
/// the fingerprint does not need to be a content hash: it needs to change when the operator points
/// `[retrieval]` at different files, which size and mtime already detect. A false match is only
/// reachable by replacing a model with a different one of identical byte length and timestamp.
///
/// # Errors
///
/// Returns [`ErrorKind::Io`] when the directory holds no readable model file.
pub fn model_fingerprint(model_directory: &Path) -> Result<String> {
    let mut entries = BTreeMap::new();
    let listing = std::fs::read_dir(model_directory).map_err(|error| {
        Error::new(
            ErrorKind::Io,
            format!(
                "read embedding model directory {}: {error}",
                model_directory.display()
            ),
        )
    })?;
    for entry in listing {
        let entry = entry.map_err(|error| {
            Error::new(
                ErrorKind::Io,
                format!("list embedding model directory: {error}"),
            )
        })?;
        let metadata = match entry.metadata() {
            Ok(metadata) if metadata.is_file() => metadata,
            _ => continue,
        };
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |elapsed| elapsed.as_nanos());
        entries.insert(
            entry.file_name().to_string_lossy().into_owned(),
            (metadata.len(), modified),
        );
    }
    if entries.is_empty() {
        return Err(Error::new(
            ErrorKind::Io,
            format!(
                "embedding model directory {} holds no files",
                model_directory.display()
            ),
        ));
    }
    let mut hasher = Sha256::new();
    for (name, (length, modified)) in entries {
        hasher.update(name.as_bytes());
        hasher.update(b"\0");
        hasher.update(length.to_le_bytes());
        hasher.update(modified.to_le_bytes());
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// How long a write against `semantic.sqlite` waits on another connection's lock before giving up.
///
/// The two real writers -- an editor's long-lived `serve` process backfilling on a background
/// thread, and a `sctx doctor --fix` synchronous warm running at the same time -- are two separate
/// connections to the same file, exactly the shape that produces `SQLITE_BUSY`. Both `serve` and
/// `doctor --fix` already treat a failed [`SemanticVectorCache::store`] as best-effort and swallow
/// the error (`crates/mcp/src/semantic.rs`), so a timeout that is too short does not fail loudly,
/// it silently drops the vector -- which is exactly the bug this constant closes. See
/// [`SemanticVectorCache::open`] for why this one number also covers the cache's reads.
const SEMANTIC_CACHE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Discardable local cache of corpus vectors, in its own `SQLite` file.
///
/// It is deliberately not a table in `index.sqlite`. Vectors are derived data an operator may
/// delete at any moment to reclaim disk, they are invalidated by facts the projection knows
/// nothing about (which model files `[retrieval]` points at), and nothing in the domain reads
/// them. Keeping them out means a corrupt or stale cache can be removed without rebuilding the
/// projection, and a projection rebuild never has to carry a gigabyte of floats.
pub struct SemanticVectorCache {
    connection: Mutex<Connection>,
}

impl SemanticVectorCache {
    /// Opens or creates the cache database at `path`.
    ///
    /// # Errors
    ///
    /// Returns a typed error when the file cannot be opened or the schema cannot be applied.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                Error::new(
                    ErrorKind::Io,
                    format!("create semantic cache directory: {error}"),
                )
            })?;
        }
        let connection = Connection::open(path)
            .map_err(|error| Error::new(ErrorKind::Io, format!("open semantic cache: {error}")))?;
        // Explicit, not `Duration::ZERO` the way `artifact_focus`'s read-only connection sets it.
        // That contrast is a decision, not an oversight: `artifact_focus` opens a *second*,
        // read-only connection specifically so a query in progress is never made to wait on it,
        // and it accepts an immediate `SQLITE_BUSY` because the caller has a same-process
        // fallback for that case. This cache has one connection for both halves, and the two
        // halves have opposite tolerance for a wait -- so the choice has to be justified by what
        // actually reaches this connection, not split down the middle.
        //
        // Nothing a retrieval does synchronously touches this connection. The corpus vectors the
        // second hop compares are loaded into memory once, at channel construction
        // (`SemanticVectorCache::load`, called from `from_cache`), and that construction happens
        // only on the background loader thread in `spawn_semantic_loader` or inside `sctx doctor
        // --fix`'s synchronous warm-up -- never inline in a request. The one write a retrieval
        // provokes is `record_hop2_admissions`, and it runs after the Pack has been assembled and
        // handed back. So a wait here is invisible to `task_context` latency by construction, not
        // by measurement, and the same budget that is safe for the write paths below (cache fill,
        // `prune_superseded`, `hop2_admission_sample`) is safe for the whole connection.
        //
        // Five seconds, matching `rusqlite::Connection::open`'s own default
        // (`sqlite3_busy_timeout(db, 5000)`, set unconditionally inside
        // `InnerConnection::open_with_flags`): this call changes no observed behaviour today, it
        // only makes the wait every write here already gets an explicit, reviewable fact instead
        // of an rusqlite implementation detail this crate happened to inherit. Every other
        // connection this codebase opens for a real write path sets its own busy_timeout by name
        // (`engineering-graph`'s and `task-runtime`'s `BUSY_TIMEOUT` at 10s, `index`'s at 3s); this
        // was the one write path relying on the library default instead of stating its own budget,
        // and "hundreds of ms to low seconds" for a corpus backfill or a `doctor --fix` warm sync
        // that nothing waits on puts five seconds squarely inside range.
        connection
            .busy_timeout(SEMANTIC_CACHE_BUSY_TIMEOUT)
            .map_err(|error| {
                Error::new(
                    ErrorKind::Io,
                    format!("set semantic cache busy timeout: {error}"),
                )
            })?;
        connection
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = NORMAL;
                 CREATE TABLE IF NOT EXISTS revision_vector (
                     revision_id TEXT NOT NULL,
                     model_fingerprint TEXT NOT NULL,
                     ranking_version TEXT NOT NULL,
                     dimensions INTEGER NOT NULL,
                     vector BLOB NOT NULL,
                     PRIMARY KEY (revision_id, model_fingerprint, ranking_version)
                 ) WITHOUT ROWID;
                 -- Query encode observations. The query path is retired, so nothing writes
                 -- them and nothing reads them; the rows a previous build left behind are
                 -- reclaimed rather than carried, which is what a discardable cache is for.
                 DROP TABLE IF EXISTS encode_sample;
                 CREATE TABLE IF NOT EXISTS hop2_admission_sample (
                     observed_at INTEGER PRIMARY KEY AUTOINCREMENT,
                     recorded_at_unix_seconds INTEGER NOT NULL,
                     seed_revision_id TEXT NOT NULL,
                     candidate_revision_id TEXT NOT NULL,
                     score_basis_points INTEGER NOT NULL,
                     admitted INTEGER NOT NULL
                 );",
            )
            .map_err(|error| {
                Error::new(
                    ErrorKind::Io,
                    format!("initialize semantic cache schema: {error}"),
                )
            })?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    /// Opens the cache that belongs to an installation root.
    ///
    /// # Errors
    ///
    /// Returns a typed error when the file cannot be opened or the schema cannot be applied.
    pub fn open_at_root(root: &Path) -> Result<Self> {
        Self::open(&semantic_cache_path(root))
    }

    /// Writes one revision vector, replacing any vector already held under the same key.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] when the vector length disagrees with `dimensions`, or
    /// a typed error when the write fails.
    pub fn store(
        &self,
        key: &SemanticCacheKey,
        revision_id: RevisionId,
        vector: &[f32],
    ) -> Result<()> {
        if vector.is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "semantic vector must not be empty",
            ));
        }
        let bytes = encode_vector(vector);
        let dimensions = i64::try_from(vector.len()).unwrap_or(i64::MAX);
        self.locked()?
            .execute(
                "INSERT OR REPLACE INTO revision_vector
                    (revision_id, model_fingerprint, ranking_version, dimensions, vector)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    revision_id.to_string(),
                    key.model_fingerprint,
                    key.ranking_version,
                    dimensions,
                    bytes
                ],
            )
            .map_err(|error| {
                Error::new(ErrorKind::Io, format!("store semantic vector: {error}"))
            })?;
        Ok(())
    }

    /// Reads every vector held under one key.
    ///
    /// # Errors
    ///
    /// Returns a typed error when the read fails.
    pub fn load(&self, key: &SemanticCacheKey) -> Result<Vec<(RevisionId, Vec<f32>)>> {
        let connection = self.locked()?;
        let mut statement = connection
            .prepare(
                "SELECT revision_id, vector FROM revision_vector
                 WHERE model_fingerprint = ?1 AND ranking_version = ?2
                 ORDER BY revision_id",
            )
            .map_err(|error| {
                Error::new(
                    ErrorKind::Io,
                    format!("prepare semantic vector read: {error}"),
                )
            })?;
        let rows = statement
            .query_map(
                rusqlite::params![key.model_fingerprint, key.ranking_version],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .map_err(|error| {
                Error::new(ErrorKind::Io, format!("query semantic vectors: {error}"))
            })?;
        let mut loaded = Vec::new();
        for row in rows {
            let (revision_id, bytes) = row.map_err(|error| {
                Error::new(ErrorKind::Io, format!("collect semantic vectors: {error}"))
            })?;
            let Ok(revision_id) = revision_id.parse::<RevisionId>() else {
                continue;
            };
            // A truncated or corrupt blob is a discardable cache row, not a retrieval failure.
            let Some(vector) = decode_vector(&bytes) else {
                continue;
            };
            loaded.push((revision_id, vector));
        }
        Ok(loaded)
    }

    /// Reads the revisions already cached under one key, for an incremental backfill.
    ///
    /// # Errors
    ///
    /// Returns a typed error when the read fails.
    pub fn cached_revisions(&self, key: &SemanticCacheKey) -> Result<BTreeSet<RevisionId>> {
        let connection = self.locked()?;
        let mut statement = connection
            .prepare(
                "SELECT revision_id FROM revision_vector
                 WHERE model_fingerprint = ?1 AND ranking_version = ?2",
            )
            .map_err(|error| {
                Error::new(
                    ErrorKind::Io,
                    format!("prepare cached revision read: {error}"),
                )
            })?;
        let rows = statement
            .query_map(
                rusqlite::params![key.model_fingerprint, key.ranking_version],
                |row| row.get::<_, String>(0),
            )
            .map_err(|error| {
                Error::new(ErrorKind::Io, format!("query cached revisions: {error}"))
            })?;
        let mut cached = BTreeSet::new();
        for row in rows {
            let revision_id = row.map_err(|error| {
                Error::new(ErrorKind::Io, format!("collect cached revisions: {error}"))
            })?;
            if let Ok(revision_id) = revision_id.parse::<RevisionId>() {
                cached.insert(revision_id);
            }
        }
        Ok(cached)
    }

    /// Drops every row produced by a different model or ranking version.
    ///
    /// Returns how many rows were removed. Superseded generations are dead weight, not history:
    /// nothing can ever compare them against the current one.
    ///
    /// # Errors
    ///
    /// Returns a typed error when the delete fails.
    pub fn prune_superseded(&self, key: &SemanticCacheKey) -> Result<usize> {
        let removed = self
            .locked()?
            .execute(
                "DELETE FROM revision_vector
                 WHERE model_fingerprint <> ?1 OR ranking_version <> ?2",
                rusqlite::params![key.model_fingerprint, key.ranking_version],
            )
            .map_err(|error| {
                Error::new(ErrorKind::Io, format!("prune semantic vectors: {error}"))
            })?;
        Ok(removed)
    }

    /// Drops every row under the current key whose revision is no longer embeddable.
    ///
    /// Returns how many rows were removed. [`prune_superseded`](Self::prune_superseded) reclaims
    /// vectors the *model or corpus generation* left behind; this one reclaims vectors the
    /// *projection* left behind -- a revision that has since been superseded, un-accepted, or made
    /// ineligible for automatic injection keeps its vector forever otherwise, because the key it
    /// is filed under never changes. Measured at 5 of 28 rows (18%) on one real installation.
    ///
    /// A dead vector cannot resurrect its Context: `apply_semantic_context_evidence` re-checks the
    /// safety predicate on every hit. What it can do is take one of the channel's
    /// [`SEMANTIC_CHANNEL_LIMIT`] slots and be discarded after the fact, so the cost is recall, not
    /// correctness. `keep` is the accepted set the backfill just read, which makes this the same
    /// snapshot the next encode pass works from.
    ///
    /// # Errors
    ///
    /// Returns a typed error when the cache cannot be read or the delete fails.
    pub fn retain_revisions(
        &self,
        key: &SemanticCacheKey,
        keep: &BTreeSet<RevisionId>,
    ) -> Result<usize> {
        let dead: Vec<RevisionId> = self
            .cached_revisions(key)?
            .into_iter()
            .filter(|revision_id| !keep.contains(revision_id))
            .collect();
        if dead.is_empty() {
            return Ok(0);
        }
        let mut connection = self.locked()?;
        let transaction = connection.transaction().map_err(|error| {
            Error::new(ErrorKind::Io, format!("open dead vector prune: {error}"))
        })?;
        let mut removed = 0;
        {
            let mut statement = transaction
                .prepare(
                    "DELETE FROM revision_vector
                     WHERE revision_id = ?1 AND model_fingerprint = ?2 AND ranking_version = ?3",
                )
                .map_err(|error| {
                    Error::new(ErrorKind::Io, format!("prepare dead vector prune: {error}"))
                })?;
            for revision_id in dead {
                removed += statement
                    .execute(rusqlite::params![
                        revision_id.to_string(),
                        key.model_fingerprint,
                        key.ranking_version
                    ])
                    .map_err(|error| {
                        Error::new(ErrorKind::Io, format!("prune dead vector: {error}"))
                    })?;
            }
        }
        transaction.commit().map_err(|error| {
            Error::new(ErrorKind::Io, format!("commit dead vector prune: {error}"))
        })?;
        Ok(removed)
    }

    /// Records what the second hop decided about each pair it judged.
    ///
    /// Best-effort and silent: this is the pre-registered recalibration record, and a diagnostic
    /// that can fail a retrieval is worse than a diagnostic with a hole in it. A cache file an operator deleted
    /// mid-session, a full disk, or a table an older build never created all end here as a dropped
    /// batch rather than as a Pack that failed to assemble.
    ///
    /// The whole batch is one transaction, so a reader never sees half of one hop's decisions --
    /// a partial batch would read as a hop that refused the pairs it never got to write.
    pub fn record_hop2_admissions(&self, samples: &[Hop2AdmissionSample]) {
        if samples.is_empty() {
            return;
        }
        let Ok(mut connection) = self.locked() else {
            return;
        };
        let recorded_at = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_secs()),
        )
        .unwrap_or(i64::MAX);
        let Ok(transaction) = connection.transaction() else {
            return;
        };
        {
            let Ok(mut statement) = transaction.prepare(
                "INSERT INTO hop2_admission_sample
                     (recorded_at_unix_seconds, seed_revision_id, candidate_revision_id,
                      score_basis_points, admitted)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            ) else {
                return;
            };
            for sample in samples {
                if statement
                    .execute(rusqlite::params![
                        recorded_at,
                        sample.seed_revision_id.to_string(),
                        sample.candidate_revision_id.to_string(),
                        i64::from(sample.score_basis_points),
                        i64::from(sample.admitted),
                    ])
                    .is_err()
                {
                    return;
                }
            }
        }
        if transaction.commit().is_err() {
            return;
        }
        let _trimmed = connection.execute(
            "DELETE FROM hop2_admission_sample WHERE observed_at <= (
                 SELECT observed_at FROM hop2_admission_sample
                 ORDER BY observed_at DESC LIMIT 1 OFFSET ?1
             )",
            [i64::try_from(SEMANTIC_HOP2_SAMPLE_HISTORY).unwrap_or(i64::MAX)],
        );
    }

    /// Returns the most recent second-hop decisions, newest first.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::Io`] when the cache cannot be read.
    pub fn recent_hop2_admissions(&self, limit: usize) -> Result<Vec<RecordedHop2Admission>> {
        let connection = self.locked()?;
        let mut statement = connection
            .prepare(
                "SELECT recorded_at_unix_seconds, seed_revision_id, candidate_revision_id,
                        score_basis_points, admitted
                 FROM hop2_admission_sample ORDER BY observed_at DESC LIMIT ?1",
            )
            .map_err(|error| Error::new(ErrorKind::Io, format!("read hop2 admissions: {error}")))?;
        let rows = statement
            .query_map([i64::try_from(limit).unwrap_or(i64::MAX)], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)? != 0,
                ))
            })
            .map_err(|error| Error::new(ErrorKind::Io, format!("read hop2 admissions: {error}")))?;
        let mut recorded = Vec::new();
        for row in rows {
            let (recorded_at_unix_seconds, seed, candidate, score, admitted) =
                row.map_err(|error| {
                    Error::new(ErrorKind::Io, format!("collect hop2 admissions: {error}"))
                })?;
            // An unparsable identity is a row of a discardable cache, not a read failure.
            let (Ok(seed_revision_id), Ok(candidate_revision_id)) =
                (seed.parse::<RevisionId>(), candidate.parse::<RevisionId>())
            else {
                continue;
            };
            recorded.push(RecordedHop2Admission {
                recorded_at_unix_seconds,
                sample: Hop2AdmissionSample {
                    seed_revision_id,
                    candidate_revision_id,
                    score_basis_points: score.try_into().unwrap_or(u16::MAX),
                    admitted,
                },
            });
        }
        Ok(recorded)
    }

    fn locked(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.connection.lock().map_err(|_| {
            Error::new(
                ErrorKind::InvariantViolation,
                "semantic cache connection lock was poisoned",
            )
        })
    }
}

/// Second-hop decisions go to the same discardable file the vectors do.
///
/// They belong there and nowhere durable: they describe what this installation retrieved against
/// this corpus under this model, and deleting `semantic.sqlite` to reclaim disk must never be a
/// decision about diagnostics. Every write is best-effort for the same reason -- a full disk
/// degrades observability, never retrieval.
impl Hop2AdmissionRecorder for SemanticVectorCache {
    fn record_hop2_admissions(&self, samples: &[Hop2AdmissionSample]) {
        Self::record_hop2_admissions(self, samples);
    }
}

/// Path of the discardable vector cache inside an installation root.
#[must_use]
pub fn semantic_cache_path(root: &Path) -> PathBuf {
    root.join("state").join("semantic.sqlite")
}

fn encode_vector(vector: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(vector.len() * 4);
    for value in vector {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

fn decode_vector(bytes: &[u8]) -> Option<Vec<f32>> {
    if bytes.is_empty() || bytes.len() % 4 != 0 {
        return None;
    }
    Some(
        bytes
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
            .collect(),
    )
}

/// L2-normalizes a vector in place, returning `false` when it has no direction to keep.
#[must_use]
pub fn normalize(vector: &mut [f32]) -> bool {
    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    if !norm.is_finite() || norm <= f32::EPSILON {
        return false;
    }
    for value in vector.iter_mut() {
        *value /= norm;
    }
    true
}

/// Cosine similarity of two vectors that are already unit length.
#[must_use]
pub fn cosine_similarity(left: &[f32], right: &[f32]) -> f32 {
    if left.len() != right.len() {
        return 0.0;
    }
    left.iter()
        .zip(right)
        .map(|(left, right)| left * right)
        .sum()
}

/// Scales one cosine to the basis points every floor in this file is expressed in.
///
/// `pub(crate)` rather than private because the second hop compares against
/// [`SEMANTIC_HOP2_ADMISSION_FLOOR_BASIS_POINTS`] and has to round identically: a lane that rounded
/// its own way would sit a basis point off the distribution the floor was calibrated against, which
/// is precisely the gap this repository has twice paid for.
pub(crate) fn similarity_basis_points(similarity: f32) -> u16 {
    if !similarity.is_finite() || similarity <= 0.0 {
        return 0;
    }
    let scaled = (similarity * 10_000.0).round();
    if scaled >= 10_000.0 {
        return 10_000;
    }
    // The value is finite, positive and below 10_000 here, so the cast cannot lose meaning.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    {
        scaled as u16
    }
}

/// The live channel: one snapshot of cached corpus vectors, plus somewhere to record what the
/// second hop decided about them.
///
/// The snapshot is read once, when the background loader publishes the channel, and never
/// consulted again. A retrieval that arrives while the backfill is still running sees the corpus
/// as it stood at publication; missing a vector costs one candidate on one retrieval, and
/// re-reading `SQLite` on that path to avoid it would cost every retrieval.
///
/// It holds no provider. It used to, because it encoded the caller's query; with that path retired
/// there is nothing here to encode -- both sides of every comparison are vectors the backfill
/// already wrote -- and holding a two-gigabyte model behind a type that cannot use it would say
/// the opposite.
pub struct EmbeddingSemanticChannel {
    vectors: Arc<Vec<(RevisionId, Vec<f32>)>>,
    recorder: Option<Arc<dyn Hop2AdmissionRecorder>>,
}

impl EmbeddingSemanticChannel {
    /// Builds a channel over an in-memory vector snapshot.
    #[must_use]
    pub fn new(vectors: Vec<(RevisionId, Vec<f32>)>) -> Self {
        Self {
            vectors: Arc::new(vectors),
            recorder: None,
        }
    }

    /// Builds a channel over everything the cache currently holds under `key`.
    ///
    /// # Errors
    ///
    /// Returns a typed error when the cache cannot be read.
    pub fn from_cache(cache: &SemanticVectorCache, key: &SemanticCacheKey) -> Result<Self> {
        Ok(Self::new(cache.load(key)?))
    }

    /// Attaches the sink that records what the second hop decided.
    #[must_use]
    pub fn with_hop2_recorder(mut self, recorder: Arc<dyn Hop2AdmissionRecorder>) -> Self {
        self.recorder = Some(recorder);
        self
    }

    /// How many corpus vectors this channel holds.
    #[must_use]
    pub fn corpus_size(&self) -> usize {
        self.vectors.len()
    }
}

impl SemanticChannel for EmbeddingSemanticChannel {
    fn document_vectors(&self) -> Option<DocumentVectorSnapshot> {
        Some(Arc::clone(&self.vectors))
    }

    fn record_hop2_admissions(&self, samples: &[Hop2AdmissionSample]) {
        if let Some(recorder) = &self.recorder {
            recorder.record_hop2_admissions(samples);
        }
    }
}

/// Loads the configured ONNX encoder, or explains why this build cannot.
///
/// Callers stay free of `cfg` this way: a build without the `embedding-onnx` feature reports
/// [`ErrorKind::Unsupported`] here rather than making every caller carry two code paths.
///
/// # Errors
///
/// Returns a typed error when the runtime library, the model, or the tokenizer cannot be loaded.
#[cfg(feature = "embedding-onnx")]
pub fn load_onnx_provider(
    model_directory: &Path,
    runtime_library: &Path,
) -> Result<Arc<dyn EmbeddingProvider>> {
    onnx::initialize_runtime(runtime_library)?;
    Ok(Arc::new(onnx::OnnxEmbeddingProvider::load_and_probe(
        model_directory,
    )?))
}

/// Reports that this build has no ONNX support compiled in.
///
/// # Errors
///
/// Always returns [`ErrorKind::Unsupported`].
#[cfg(not(feature = "embedding-onnx"))]
pub fn load_onnx_provider(
    _model_directory: &Path,
    _runtime_library: &Path,
) -> Result<Arc<dyn EmbeddingProvider>> {
    Err(Error::new(
        ErrorKind::Unsupported,
        "this build was compiled without the embedding-onnx feature",
    ))
}

/// Process-lifetime slot a background loader fills once the model is ready.
///
/// The MCP server hands this to every retrieval the moment `[retrieval]` is configured, long
/// before the 9--12 s model load finishes. Until then it has no document vectors, which is the
/// honest report: the channel exists and could not serve the hop. An installation with no
/// `[retrieval]` table never gets a handle at all, and so never reports an omission.
#[derive(Clone, Default)]
pub struct SemanticChannelHandle {
    inner: Arc<RwLock<Option<Arc<dyn SemanticChannel>>>>,
    backfill: Arc<BackfillRequests>,
}

/// Most revisions this queue will name before it stops recording names.
///
/// The queue is a wake-up call, not a work list: whoever answers it recomputes the whole missing
/// set from the projection, so a name dropped here is still embedded. The bound exists because an
/// installation whose model failed to load has nobody answering, and an unbounded set of revision
/// ids would then grow for the life of the process.
const MAX_PENDING_BACKFILL_REVISIONS: usize = 1_024;

/// Revisions accepted since the corpus was last filled, and a way to wake whoever fills it.
#[derive(Default)]
struct BackfillRequests {
    pending: Mutex<BTreeSet<RevisionId>>,
    woken: Condvar,
}

impl SemanticChannelHandle {
    /// Creates an unfilled handle.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Publishes the loaded channel. A later publish replaces an earlier one, which is how a
    /// completed backfill swaps in a fuller corpus snapshot.
    pub fn publish(&self, channel: Arc<dyn SemanticChannel>) {
        if let Ok(mut slot) = self.inner.write() {
            *slot = Some(channel);
        }
    }

    /// True once a channel has been published.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.inner.read().is_ok_and(|slot| slot.is_some())
    }

    /// Asks for these newly accepted revisions to be embedded, and wakes the filler.
    ///
    /// The corpus backfill runs at model-load time, so a Context accepted *during* a `serve`
    /// process had no vector and no path to one: the loader had already finished its pass. A
    /// session's own knowledge was therefore invisible to Lane B for the rest of that process --
    /// measured on a real 21.8-hour Session, where all three Contexts it produced were missing
    /// from `revision_vector` while every older revision was present.
    ///
    /// Advisory in both directions. A handle nobody is filling (no model, or a failed load) only
    /// accumulates up to [`MAX_PENDING_BACKFILL_REVISIONS`] names, and a caller never learns
    /// whether the work happened -- writing knowledge must not wait on, or fail because of, a
    /// discardable cache.
    pub fn request_backfill(&self, revisions: impl IntoIterator<Item = RevisionId>) {
        let Ok(mut pending) = self.backfill.pending.lock() else {
            return;
        };
        let mut requested = false;
        for revision_id in revisions {
            if pending.len() >= MAX_PENDING_BACKFILL_REVISIONS {
                break;
            }
            requested |= pending.insert(revision_id);
        }
        if requested {
            self.backfill.woken.notify_all();
        }
    }

    /// The revisions currently waiting for a vector.
    #[must_use]
    pub fn pending_backfill(&self) -> BTreeSet<RevisionId> {
        self.backfill
            .pending
            .lock()
            .map(|pending| pending.clone())
            .unwrap_or_default()
    }

    /// Blocks until at least one revision is waiting, then takes the whole set.
    ///
    /// Taking the set before the work rather than after is deliberate: whoever fills the corpus
    /// recomputes the missing set from the projection anyway, so a revision requested *during* a
    /// fill must wake the next one rather than being swallowed by the current one.
    #[must_use]
    pub fn take_backfill_requests(&self) -> BTreeSet<RevisionId> {
        let Ok(mut pending) = self.backfill.pending.lock() else {
            return BTreeSet::new();
        };
        loop {
            if !pending.is_empty() {
                return std::mem::take(&mut *pending);
            }
            let Ok(next) = self.backfill.woken.wait(pending) else {
                return BTreeSet::new();
            };
            pending = next;
        }
    }

    fn channel(&self) -> Option<Arc<dyn SemanticChannel>> {
        self.inner.read().ok()?.clone()
    }
}

impl std::fmt::Debug for SemanticChannelHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SemanticChannelHandle")
            .field("ready", &self.is_ready())
            .finish()
    }
}

impl SemanticChannel for SemanticChannelHandle {
    fn document_vectors(&self) -> Option<DocumentVectorSnapshot> {
        self.channel()?.document_vectors()
    }

    fn record_hop2_admissions(&self, samples: &[Hop2AdmissionSample]) {
        if let Some(channel) = self.channel() {
            channel.record_hop2_admissions(samples);
        }
    }
}
