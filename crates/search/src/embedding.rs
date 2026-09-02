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
//!   thread. The query path reads that cache and encodes one short query under a hard wall-clock
//!   budget; every failure mode -- absent model, unfinished load, slow encode -- degrades to
//!   [`SemanticOutcome::Unavailable`], which the Pack reports as `embedding_unavailable` while the
//!   lexical channels answer unchanged.
//! * **It is one channel, never the answer.** The prototype that motivated ADR-0004 lost
//!   identifier queries outright and could not tell a near-duplicate from its neighbour. Its
//!   output is fused, never substituted.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, RwLock,
        mpsc::{RecvTimeoutError, sync_channel},
    },
    time::{Duration, Instant, UNIX_EPOCH},
};

use rusqlite::Connection;
use sctx_domain::{Error, ErrorKind, Result, RevisionId};
use sha2::{Digest, Sha256};

#[cfg(feature = "embedding-onnx")]
pub mod onnx;

/// Cosine similarity, in basis points, a corpus revision must reach to enter the channel.
///
/// ADR-0004 set a provisional 0.50 from a two-fixture prototype and explicitly deferred the final
/// value to the T5a extended set. This is that recalibration, and the extended set moved it:
/// measured against `fixtures/association/probe-ext-v1.json` with a real bge-m3 export, the
/// highest-scoring noise query reaches 5095 and the lowest-scoring cross-lingual positive reaches
/// 5640. A floor of 0.50 therefore admits one noise query outright -- and because a semantic hit
/// is its own injection eligibility path, admitting it is enough to put a Context in front of an
/// Agent who asked about something else entirely.
///
/// 5200 sits 105 basis points above the noise ceiling and 440 below the lowest positive the
/// channel exists to rescue. One `multi_hop` probe scores 5160 and is given up deliberately: the
/// alternative is defending a five-basis-point gap, which is not a threshold but an overfit to one
/// fixture. Noise rejection is the constraint that does not bend, because a wrong Context nobody
/// asked for is worse than no Context at all.
pub const SEMANTIC_SIMILARITY_FLOOR_BASIS_POINTS: u16 = 5_200;

/// Most revisions one query may contribute through the semantic channel.
///
/// Fusion ranks within a channel, so an unbounded channel would hand a rank to every vector above
/// the floor and let the long tail of weak semantic neighbours outvote a strong lexical match by
/// sheer count.
pub const SEMANTIC_CHANNEL_LIMIT: usize = 16;

/// Wall clock a single query encode may spend before the channel reports itself unavailable.
///
/// ADR-0004 measured 30--85 ms p95 for a loaded bge-m3. The budget is the point past which a
/// retrieval that was never asked for stops being worth the Agent's latency.
pub const SEMANTIC_ENCODE_BUDGET: Duration = Duration::from_millis(200);

/// Longest query, in model tokens, handed to the encoder. bge-m3 accepts far more; retrieval
/// queries built from a Working Intent do not need them, and truncation keeps the encode inside
/// [`SEMANTIC_ENCODE_BUDGET`].
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
}

/// One corpus revision the semantic channel matched, with the similarity that admitted it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SemanticHit {
    /// The accepted revision whose cached vector matched.
    pub revision_id: RevisionId,
    /// Cosine similarity scaled to basis points and clamped to `[0, 10000]`.
    pub similarity_basis_points: u16,
}

/// What the semantic channel had to say about one query.
///
/// The two variants are deliberately not collapsed into `Vec`: an empty hit list means the channel
/// ran and found nothing above the floor, while [`Self::Unavailable`] means it could not run at
/// all. Only the second one is worth an omission line in the Pack.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SemanticOutcome {
    /// The model was missing, still loading, or too slow for [`SEMANTIC_ENCODE_BUDGET`].
    Unavailable,
    /// The channel ran. The vector is sorted by descending similarity and already truncated to
    /// [`SEMANTIC_CHANNEL_LIMIT`].
    Hits(Vec<SemanticHit>),
}

/// The query-time face of the channel, so retrieval never depends on how vectors are produced.
pub trait SemanticChannel: Send + Sync {
    /// Ranks cached corpus revisions against one query text.
    fn similar_revisions(&self, query_text: &str) -> SemanticOutcome;
}

/// Identifies one generation of cached vectors.
///
/// Both halves matter. `model_fingerprint` changes when the operator swaps model files, and
/// comparing vectors from two different models is meaningless rather than merely inaccurate.
/// `ranking_version` follows `SEARCH_RANKING_VERSION`, because the text that gets embedded is the
/// same text the lexical channels index and a ranking-version bump is exactly the signal that it
/// changed shape.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticCacheKey {
    /// Content fingerprint of the model files the vectors were produced by.
    pub model_fingerprint: String,
    /// The `SEARCH_RANKING_VERSION` in force when the vectors were produced.
    pub ranking_version: String,
}

impl SemanticCacheKey {
    /// Builds a key from a model fingerprint and the ranking version in force.
    #[must_use]
    pub fn new(model_fingerprint: impl Into<String>, ranking_version: impl Into<String>) -> Self {
        Self {
            model_fingerprint: model_fingerprint.into(),
            ranking_version: ranking_version.into(),
        }
    }
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
                 ) WITHOUT ROWID;",
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

    fn locked(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.connection.lock().map_err(|_| {
            Error::new(
                ErrorKind::InvariantViolation,
                "semantic cache connection lock was poisoned",
            )
        })
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

fn similarity_basis_points(similarity: f32) -> u16 {
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

/// The live channel: one loaded provider over one snapshot of cached corpus vectors.
///
/// The snapshot is read once, when the background loader publishes the channel, and never
/// consulted again. A query that arrives while the backfill is still running sees the corpus as it
/// stood at publication; missing a vector costs one candidate on one query, and re-reading `SQLite`
/// on the retrieval path to avoid that would cost every query.
pub struct EmbeddingSemanticChannel {
    provider: Arc<dyn EmbeddingProvider>,
    vectors: Arc<Vec<(RevisionId, Vec<f32>)>>,
    floor_basis_points: u16,
    limit: usize,
    budget: Duration,
}

impl EmbeddingSemanticChannel {
    /// Builds a channel over an in-memory vector snapshot.
    #[must_use]
    pub fn new(provider: Arc<dyn EmbeddingProvider>, vectors: Vec<(RevisionId, Vec<f32>)>) -> Self {
        Self {
            provider,
            vectors: Arc::new(vectors),
            floor_basis_points: SEMANTIC_SIMILARITY_FLOOR_BASIS_POINTS,
            limit: SEMANTIC_CHANNEL_LIMIT,
            budget: SEMANTIC_ENCODE_BUDGET,
        }
    }

    /// Builds a channel over everything the cache currently holds under `key`.
    ///
    /// # Errors
    ///
    /// Returns a typed error when the cache cannot be read.
    pub fn from_cache(
        provider: Arc<dyn EmbeddingProvider>,
        cache: &SemanticVectorCache,
        key: &SemanticCacheKey,
    ) -> Result<Self> {
        Ok(Self::new(provider, cache.load(key)?))
    }

    /// Overrides the encode budget. Tests that must observe the timeout degradation set it
    /// deliberately low; nothing else changes it.
    #[must_use]
    pub const fn with_budget(mut self, budget: Duration) -> Self {
        self.budget = budget;
        self
    }

    /// How many corpus vectors this channel can rank against.
    #[must_use]
    pub fn corpus_size(&self) -> usize {
        self.vectors.len()
    }

    /// Encodes on a detached worker so a slow model costs the caller the budget, not the encode.
    ///
    /// Returning `None` on timeout leaves the worker running; it writes into a buffered channel
    /// nobody reads and exits. That wastes one encode. Holding the retrieval path open until an
    /// unbounded model call returns would waste the Agent's turn.
    fn encode_within_budget(&self, text: &str) -> Option<Vec<f32>> {
        let provider = Arc::clone(&self.provider);
        let owned = text.to_owned();
        let (sender, receiver) = sync_channel(1);
        let started = Instant::now();
        if std::thread::Builder::new()
            .name("sctx-embedding-query".to_owned())
            .spawn(move || {
                let _ignored = sender.send(provider.encode(&owned));
            })
            .is_err()
        {
            return None;
        }
        match receiver.recv_timeout(self.budget) {
            Ok(Ok(vector)) if started.elapsed() <= self.budget => Some(vector),
            Ok(_) | Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => None,
        }
    }
}

impl SemanticChannel for EmbeddingSemanticChannel {
    fn similar_revisions(&self, query_text: &str) -> SemanticOutcome {
        if query_text.trim().is_empty() {
            return SemanticOutcome::Hits(Vec::new());
        }
        // An empty corpus is a channel that ran and matched nothing, not a broken one: the
        // backfill simply has not reached this revision yet, and a lexical answer is still whole.
        if self.vectors.is_empty() {
            return SemanticOutcome::Hits(Vec::new());
        }
        let Some(query) = self.encode_within_budget(query_text) else {
            return SemanticOutcome::Unavailable;
        };
        if query.len() != self.provider.dimensions() {
            return SemanticOutcome::Unavailable;
        }
        let mut scored = self
            .vectors
            .iter()
            .filter(|(_, vector)| vector.len() == query.len())
            .map(|(revision_id, vector)| SemanticHit {
                revision_id: *revision_id,
                similarity_basis_points: similarity_basis_points(cosine_similarity(&query, vector)),
            })
            .filter(|hit| hit.similarity_basis_points >= self.floor_basis_points)
            .collect::<Vec<_>>();
        // Descending similarity, then Revision ID: two revisions at the same similarity must rank
        // in the same order on every run or the fused score stops being reproducible.
        scored.sort_by(|left, right| {
            right
                .similarity_basis_points
                .cmp(&left.similarity_basis_points)
                .then_with(|| left.revision_id.cmp(&right.revision_id))
        });
        scored.truncate(self.limit);
        SemanticOutcome::Hits(scored)
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
/// before the 9--12 s model load finishes. Until then it answers [`SemanticOutcome::Unavailable`],
/// which is the honest report: the channel exists and could not run. An installation with no
/// `[retrieval]` table never gets a handle at all, and so never reports an omission.
#[derive(Clone, Default)]
pub struct SemanticChannelHandle {
    inner: Arc<RwLock<Option<Arc<dyn SemanticChannel>>>>,
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
    fn similar_revisions(&self, query_text: &str) -> SemanticOutcome {
        match self.channel() {
            Some(channel) => channel.similar_revisions(query_text),
            None => SemanticOutcome::Unavailable,
        }
    }
}
