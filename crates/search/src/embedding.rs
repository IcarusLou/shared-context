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
//!   thread. The query path reads that cache and encodes one query under a hard wall-clock budget;
//!   every failure mode -- absent model, unfinished load, slow encode -- degrades to
//!   [`SemanticOutcome::Unavailable`], which the Pack reports as `embedding_unavailable` while the
//!   lexical channels answer unchanged.
//!
//!   That budget is the one number in this file that has already been wrong once, and the way it
//!   was wrong is worth keeping in view. It was set for a *short* query, because the probe suite
//!   that validated it asked short questions. A real query is a Working Intent flattened to a few
//!   hundred characters, the encode cost scales with that length, and the result was a channel
//!   that loaded a two-gigabyte model, embedded the whole corpus, reported no error, and
//!   contributed to nothing in any real session. Degrading silently is right; degrading silently
//!   *and leaving no trace* is what turned one mis-calibrated constant into an invisible total
//!   failure. Hence [`EncodeSample`]: every encode says what it cost and whether it fit.
//! * **It is one channel, never the answer.** The prototype that motivated ADR-0004 lost
//!   identifier queries outright and could not tell a near-duplicate from its neighbour. Its
//!   output is fused, never substituted.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock, RwLock,
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

/// The same floor for the F2LLM encoder over a Qwen3-0.6B decoder.
///
/// The value this constant held first came from a torch/Python benchmark run before any of this
/// existed in the workspace, and it said so, because ADR-0004 has twice recorded what happens when
/// a retrieval constant is calibrated somewhere other than where it runs. This is the T5a procedure
/// repeated on the stack that ships: measured 2026-09-07 on Apple Silicon / macOS 24.6.0, ONNX
/// Runtime 1.28.1, the `codefuse-ai/F2LLM-v2-0.6B` Hub export, release profile, over all three
/// association fixtures at once -- 27 Contexts encoded as corpus, 85 probe queries encoded through
/// the query path with the model's instruction prefix, by
/// `crates/search/tests/embedding_qwen3_floor_calibration.rs`.
///
/// That run separates into three regions, and the middle one is why this number is not 2400:
///
/// | region | range |
/// |---|---|
/// | 8 noise queries | 303 -- **2070** |
/// | 2 outlying positives (`probe-v1` zh-07, en-04) | 2403, 2447 |
/// | the other 75 positives | **3217** -- 8588 |
///
/// 2800 sits in the widest empty band the distribution has: 730 basis points above every noise
/// query and 417 below the lowest positive of the main mass. Noise rejection is the constraint that
/// does not bend -- a semantic hit is its own injection eligibility path, so one noise query above
/// the floor is enough to put a Context in front of an Agent who asked about something else -- and
/// the remaining room is spent on the positives that are still to come rather than on the two this
/// fixture happens to hold.
///
/// Those two are given up deliberately. Reaching them costs a floor of 2400, three basis points
/// under zh-07, which is an overfit to one fixture row rather than a threshold; and zh-07 would not
/// even be answered by admitting it, because a distractor outscores its expected Context (2445
/// against 2403). Their category survives regardless: `chinese_natural_language` keeps 24 of 25 and
/// `english_natural_language` 6 of 7. Both are also two-word keyword queries -- `功能等价` and
/// `functional equivalence` -- which is the shape with the least for an encoder to work with and the
/// most for a lexical index, and `association_probe_workflow` measures both as lexical hits through
/// both entry points today. Giving them up costs this suite nothing.
/// Fusion over the extended fixture measures the same 32/39 at 2800 as at any floor down to 2200,
/// so the band is a choice about safety margin and nothing else
/// (`crates/cli/tests/association_probe_ext_semantic_f2llm.rs`).
///
/// A second constant rather than a second opinion about the first: cosine floors are not portable
/// between embedding spaces. bge-m3's CLS vectors and a decoder's last-token vectors spread their
/// similarities over different ranges -- this family's noise ceiling, 2070, is below bge-m3's floor
/// by more than three thousand basis points -- and carrying 5200 across would reject nearly
/// everything this family retrieves, exactly as carrying 2800 back would admit noise bge-m3 was
/// calibrated to refuse. Which one applies is a property of the loaded model, so the loaded provider
/// is what reports it -- see [`EmbeddingProvider::similarity_floor_basis_points`].
pub const QWEN3_SEMANTIC_SIMILARITY_FLOOR_BASIS_POINTS: u16 = 2_800;

/// Most revisions one query may contribute through the semantic channel.
///
/// Fusion ranks within a channel, so an unbounded channel would hand a rank to every vector above
/// the floor and let the long tail of weak semantic neighbours outvote a strong lexical match by
/// sheer count.
pub const SEMANTIC_CHANNEL_LIMIT: usize = 16;

/// Default wall clock a single query encode may spend before the channel reports itself
/// unavailable.
///
/// The first value here was 200 ms, taken from ADR-0004's "30--85 ms p95". That number came from a
/// torch prototype, not the `ort` stack this workspace ships, and it was validated against probe
/// queries 15--40 characters long. A real query is a Working Intent flattened -- goal, current
/// direction and in-scope list concatenated -- and the encode cost is roughly linear in its
/// length. Codex session 01a06646 built a 283-character query and every one of its automatic
/// retrievals reported `embedding_unavailable`: the channel was not slow on that machine, it was
/// calibrated against a query length no real session produces.
///
/// **bge-m3.** Measured 2026-09-03, Apple Silicon / macOS 24.6.0, ONNX Runtime 1.28.1, bge-m3 ONNX
/// export, release profile, warm (`crates/search/tests/embedding_encode_latency.rs`, 24 samples
/// each):
///
/// | chars | p50 | p95 | max |
/// |------:|----:|----:|----:|
/// |    20 |  28 |  33 |  37 |
/// |    40 |  33 |  37 |  43 |
/// |   100 |  74 |  78 |  81 |
/// |   200 | 139 | 153 | 164 |
/// |   283 | 197 | 235 | 243 |
/// |   400 | 274 | 298 | 301 |
/// |   700 | 458 | 539 | 633 |
/// |  1400 | 757 | 816 | 828 |
///
/// The debug profile runs about 20% slower and tops out at 995 ms p95 for the longest query.
/// [`SEMANTIC_MAX_TOKENS`] caps the sequence, so 1400 characters is already at the truncation
/// ceiling and 816 ms is the worst warm encode this machine can be asked for.
///
/// **F2LLM-v2-0.6B**, the export `sctx embedding install` now defaults to. Measured 2026-09-07 on
/// the same machine, runtime and profile, same 24 samples per length, by the same file. The middle
/// column is what the ladder's characters cost in *tokens* for this tokenizer, because that is what
/// the cap counts:
///
/// | chars | tokens | p50 | p95 | max |
/// |------:|-------:|----:|----:|----:|
/// |    20 |     30 |  58 |  63 |  63 |
/// |    40 |     40 |  71 |  76 |  77 |
/// |   100 |     65 | 111 | 113 | 113 |
/// |   200 |    104 | 169 | 169 | 171 |
/// |   283 |    132 | 215 | 236 | 246 |
/// |   400 |    178 | 295 | 344 | 350 |
/// |   700 |    290 | 493 | 536 | 546 |
/// |  1400 |    512 | 941 | 986 |1000 |
///
/// 1400 characters of this text is exactly [`SEMANTIC_MAX_TOKENS`], so that row is the ceiling for
/// this family too; a separately measured 6000-character query, truncated to the same 512 tokens,
/// returns in 959 ms p95, which is the same number reached from the other side.
///
/// The two families cost the same at the length that matters and diverge at the tail: 236 ms
/// against 235 ms for a real 283-character Intent, 986 ms against 816 ms at the cap. Model load is
/// 2.1--2.5 s (runtime initialisation and session, warm page cache) and the process holds about
/// 1.83 GB resident with the weights in.
///
/// One thing the 2026-09-04 derivation of this constant does not survive on the new table, and it
/// is recorded here rather than quietly fixed: 2000 ms is 2.0x the F2LLM ceiling, where the load
/// factor that session 01a06b3e measured was 3--5x. It was already only 2.45x the bge-m3 ceiling,
/// so the shortfall is not a property of the new default -- what the new default does is make it
/// slightly worse at the longest query the constant can be asked for, while leaving the real-Intent
/// case (283 characters, 8x headroom) exactly where it was. Raising the budget is an ADR-0004
/// decision, not a doc-comment one.
///
/// Every row above is a quiet machine, and that is the table's limit. Codex session 01a06b3e --
/// 6.7 hours of real work, the machine also running builds and the Agent itself -- encoded at
/// three to five times these numbers: a 76-character query took 379 ms against a 78 ms quiet p95,
/// and a 394-character query took **1505 ms and timed out**, which is a length the quiet table
/// answers in under 300 ms. The 1200 ms budget did not fail because the machine is slow; it failed
/// because a busy machine is the normal case and the calibration only ever saw an idle one.
///
/// 2000 ms is the quiet 512-token ceiling (816 ms) carried through that measured 3--5x load
/// factor: even the longest query this constant can be asked for stays inside the budget while
/// the machine is under the load a real session puts it under. It remains a *ceiling*, not a
/// typical cost -- the 283-character query that exposed the original defect returns in about
/// 200 ms quiet, and a repeat of any query is served from the query vector cache for nothing at
/// all. What the number buys is that the channel keeps answering during exactly the long sessions
/// that accumulate the most Context, instead of degrading silently once the machine gets busy.
///
/// Operators on slower hardware raise it with `[retrieval] embedding_encode_budget_ms`; `sctx
/// doctor` says so when it sees encodes timing out.
pub const SEMANTIC_ENCODE_BUDGET: Duration = Duration::from_secs(2);

/// Query vectors kept in the process-lifetime LRU that fronts the encoder.
///
/// A Working Intent changes far more slowly than it is read: `task_context` re-reads the same
/// Intent, `task_artifact_focus` fires repeatedly against one Task, and every one of those builds
/// the identical query string. Sixty-four entries is a few hundred kilobytes and covers every
/// Intent a session realistically holds open at once.
pub const SEMANTIC_QUERY_CACHE_CAPACITY: usize = 64;

/// Encode observations kept in the discardable cache for `sctx embedding status` and `sctx doctor`.
///
/// Enough to show a distribution and a timeout rate; small enough that the trim is one statement
/// and the rows never become a storage decision.
pub const SEMANTIC_ENCODE_SAMPLE_HISTORY: usize = 64;

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

    /// Cosine similarity, in basis points, below which this provider's vectors say nothing useful.
    ///
    /// The floor belongs to the provider because it is a property of the embedding space, not of
    /// retrieval: it is read off a calibration run against one model, and the number that keeps
    /// noise out of one space is the number that empties another. A caller that reached for a
    /// constant instead would be asserting that every encoder scores alike, which is the assumption
    /// that breaks the moment a second family is installed.
    ///
    /// The default is [`SEMANTIC_SIMILARITY_FLOOR_BASIS_POINTS`], which is bge-m3's -- the space
    /// every provider that predates a second family produces vectors in.
    fn similarity_floor_basis_points(&self) -> u16 {
        SEMANTIC_SIMILARITY_FLOOR_BASIS_POINTS
    }

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

    /// Whether corpus encodes are competing for this provider right now.
    ///
    /// It is what separates "this encode overran because the machine cannot meet the budget" from
    /// "this encode overran because it queued behind the corpus backfill". The first is a reason
    /// to raise `[retrieval] embedding_encode_budget_ms`; the second is a window that closes by
    /// itself, and telling an operator to raise a budget over it would be wrong.
    fn is_backfilling(&self) -> bool {
        false
    }
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

/// Process-lifetime LRU of query vectors, shared by every channel built over the same generation.
///
/// The corpus cache on disk answers "what does this Context embed to". This one answers "what does
/// *this query* embed to", and it exists because the query side turned out to be the expensive
/// half: one encode of a real Working Intent costs about as much as the entire lexical retrieval
/// it is supposed to augment, and a session asks the same question repeatedly.
///
/// Two properties make it worth more than the memory it costs. A hit is not merely fast, it is
/// *free*: it skips the worker thread and the budget entirely. And an encode that overran the
/// budget still lands here when it finishes, so the caller that gave up is the only one that pays
/// -- the next read of the same Intent is a hit. That turns a systematic timeout from permanent
/// silence into one slow first call.
#[derive(Debug, Default)]
pub struct QueryVectorCache {
    entries: Mutex<QueryVectorEntries>,
    capacity: usize,
}

#[derive(Debug, Default)]
struct QueryVectorEntries {
    /// Digest of the generation and the query text, to the vector and the tick it was last read.
    vectors: BTreeMap<[u8; 32], (Arc<Vec<f32>>, u64)>,
    /// Monotonic read counter. Recency, not wall clock: a clock that can move backwards would let
    /// eviction pick the entry it just stored.
    tick: u64,
}

impl QueryVectorCache {
    /// Builds an empty cache holding at most `capacity` vectors.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: Mutex::new(QueryVectorEntries::default()),
            capacity: capacity.max(1),
        }
    }

    /// The shared cache for one cache generation, created on first use.
    ///
    /// Sharing it across channel instances is the point. [`crate::EmbeddingSemanticChannel`] is
    /// rebuilt and republished when the corpus backfill finishes, and a per-instance cache would
    /// be thrown away at exactly the moment a session has warmed it up.
    #[must_use]
    pub fn shared(key: &SemanticCacheKey) -> Arc<Self> {
        static SHARED: OnceLock<Mutex<BTreeMap<String, Arc<QueryVectorCache>>>> = OnceLock::new();
        let namespace = format!("{}\u{0}{}", key.model_fingerprint, key.ranking_version);
        let caches = SHARED.get_or_init(|| Mutex::new(BTreeMap::new()));
        let Ok(mut caches) = caches.lock() else {
            // A poisoned registry costs cache sharing, never an answer.
            return Arc::new(Self::with_capacity(SEMANTIC_QUERY_CACHE_CAPACITY));
        };
        Arc::clone(
            caches
                .entry(namespace)
                .or_insert_with(|| Arc::new(Self::with_capacity(SEMANTIC_QUERY_CACHE_CAPACITY))),
        )
    }

    /// Returns the cached vector for `query_text` under `key`, if one is held.
    #[must_use]
    pub fn get(&self, key: &SemanticCacheKey, query_text: &str) -> Option<Arc<Vec<f32>>> {
        let digest = Self::digest(key, query_text);
        let mut entries = self.entries.lock().ok()?;
        entries.tick = entries.tick.wrapping_add(1);
        let tick = entries.tick;
        let (vector, last_read) = entries.vectors.get_mut(&digest)?;
        *last_read = tick;
        Some(Arc::clone(vector))
    }

    /// Stores one query vector, evicting the least recently read entry when full.
    pub fn store(&self, key: &SemanticCacheKey, query_text: &str, vector: Arc<Vec<f32>>) {
        let digest = Self::digest(key, query_text);
        let Ok(mut entries) = self.entries.lock() else {
            return;
        };
        entries.tick = entries.tick.wrapping_add(1);
        let tick = entries.tick;
        entries.vectors.insert(digest, (vector, tick));
        while entries.vectors.len() > self.capacity {
            let Some(coldest) = entries
                .vectors
                .iter()
                .min_by_key(|(_, (_, last_read))| *last_read)
                .map(|(digest, _)| *digest)
            else {
                break;
            };
            entries.vectors.remove(&coldest);
        }
    }

    /// How many vectors the cache currently holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries
            .lock()
            .map_or(0, |entries| entries.vectors.len())
    }

    /// True when the cache holds nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Hashes the generation and the query together, so a model swap cannot serve a stale vector.
    fn digest(key: &SemanticCacheKey, query_text: &str) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(key.model_fingerprint.as_bytes());
        hasher.update(b"\0");
        hasher.update(key.ranking_version.as_bytes());
        hasher.update(b"\0");
        hasher.update(query_text.as_bytes());
        hasher.finalize().into()
    }
}

/// One observed query encode.
///
/// The channel's whole failure surface is `Unavailable`, which is honest but says nothing about
/// *why*. A timeout and an absent model read identically in the Pack, and the defect this type
/// exists to prevent -- a budget that no real query can meet -- was invisible for exactly that
/// reason: fail-open with no trace. These rows are what makes the difference legible after the
/// fact.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
pub struct EncodeSample {
    /// Characters in the query that was encoded, which is what the cost scales with.
    pub query_chars: u32,
    /// Wall clock the encode itself took, whether or not the caller was still waiting.
    pub elapsed_ms: u32,
    /// The budget in force when it ran.
    pub budget_ms: u32,
    /// Whether the encode overran that budget, which is what the caller saw as `Unavailable`.
    pub timed_out: bool,
    /// Whether the corpus backfill was competing for the encoder while this encode ran.
    ///
    /// A timeout with this set is a transient contention timeout inside a window that closes when
    /// the backfill finishes. A timeout without it is the encoder failing to meet the budget on
    /// its own, which is the only one an operator should act on.
    pub backfill_active: bool,
}

/// Where a channel reports what its encodes cost. Implemented by [`SemanticVectorCache`]; absent
/// on every channel that has no discardable cache to write to, such as the in-memory test ones.
pub trait EncodeSampleRecorder: Send + Sync {
    /// Records one observation. Failure is not reportable: a diagnostic that can fail a retrieval
    /// is worse than no diagnostic.
    fn record(&self, sample: EncodeSample);
}

/// What the recorded encodes add up to, for `sctx embedding status` and `sctx doctor`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize)]
pub struct EncodeLatencySummary {
    /// Observations the summary is built from.
    pub samples: usize,
    /// How many of them overran the budget.
    pub timed_out: usize,
    /// How many of those timeouts happened while the corpus backfill held the encoder.
    ///
    /// Subtracting this from `timed_out` leaves the timeouts that say something about the budget.
    pub timed_out_during_backfill: usize,
    /// Median observed encode, in milliseconds.
    pub p50_ms: u32,
    /// 95th percentile observed encode, in milliseconds.
    pub p95_ms: u32,
    /// Slowest observed encode, in milliseconds.
    pub max_ms: u32,
    /// The budget in force at the most recent observation.
    pub budget_ms: u32,
}

impl EncodeLatencySummary {
    /// Summarises a batch of observations, newest first.
    #[must_use]
    pub fn from_samples(samples: &[EncodeSample]) -> Self {
        if samples.is_empty() {
            return Self::default();
        }
        let mut elapsed = samples
            .iter()
            .map(|sample| sample.elapsed_ms)
            .collect::<Vec<_>>();
        elapsed.sort_unstable();
        let percentile = |fraction: usize| {
            let rank = (elapsed.len() * fraction).div_ceil(100).saturating_sub(1);
            elapsed[rank.min(elapsed.len() - 1)]
        };
        Self {
            samples: samples.len(),
            timed_out: samples.iter().filter(|sample| sample.timed_out).count(),
            timed_out_during_backfill: samples
                .iter()
                .filter(|sample| sample.timed_out && sample.backfill_active)
                .count(),
            p50_ms: percentile(50),
            p95_ms: percentile(95),
            max_ms: elapsed.last().copied().unwrap_or_default(),
            budget_ms: samples.first().map_or(0, |sample| sample.budget_ms),
        }
    }

    /// True when the recorded history is long enough to trust and mostly timeouts.
    ///
    /// One timeout is a busy machine. A majority of them over a full history is the shape of the
    /// defect this whole module was repaired for: a budget the local hardware cannot meet, which
    /// silently costs every automatic retrieval its semantic channel.
    ///
    /// Timeouts taken while the corpus backfill held the encoder do not count. They are real
    /// degradations and they are recorded as such, but they say nothing about the budget: they end
    /// when the backfill does, and telling an operator to raise a budget over them would send them
    /// to change a number that was never the cause.
    #[must_use]
    pub const fn budget_is_unfit(&self) -> bool {
        let attributable = self
            .timed_out
            .saturating_sub(self.timed_out_during_backfill);
        self.samples >= 8 && attributable * 2 > self.samples
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
        // Nothing on the synchronous query path -- `EmbeddingSemanticChannel::similar_revisions`,
        // reached from `task_context` -- ever touches this connection. The corpus vectors it
        // scores against are loaded into memory once, at channel construction
        // (`SemanticVectorCache::load`, called from `from_cache`), and that construction happens
        // only on the background loader thread in `spawn_semantic_loader` or inside `sctx doctor
        // --fix`'s synchronous warm-up -- never inline in a request. The one write this connection
        // takes that a query ever provokes is the `EncodeSampleRecorder::record` call in
        // `encode_within_budget`, and that runs on a spawned per-query thread *after* it has
        // already sent the vector back over the channel the caller is blocked on: the caller has
        // its answer, and its p95, before this connection is touched at all. So a wait here is
        // invisible to `task_context` latency by construction, not by measurement, and the same
        // budget that is safe for the write paths below (cache fill, `prune_superseded`,
        // `encode_sample`) is safe for the whole connection.
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
                 CREATE TABLE IF NOT EXISTS encode_sample (
                     observed_at INTEGER PRIMARY KEY AUTOINCREMENT,
                     query_chars INTEGER NOT NULL,
                     elapsed_ms INTEGER NOT NULL,
                     budget_ms INTEGER NOT NULL,
                     timed_out INTEGER NOT NULL,
                     backfill_active INTEGER NOT NULL DEFAULT 0
                 );",
            )
            .map_err(|error| {
                Error::new(
                    ErrorKind::Io,
                    format!("initialize semantic cache schema: {error}"),
                )
            })?;
        migrate_encode_sample(&connection)?;
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

    /// Returns the most recent encode observations, newest first.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::Io`] when the cache cannot be read.
    pub fn recent_encode_samples(&self, limit: usize) -> Result<Vec<EncodeSample>> {
        let connection = self.locked()?;
        let mut statement = connection
            .prepare(
                "SELECT query_chars, elapsed_ms, budget_ms, timed_out, backfill_active
                 FROM encode_sample ORDER BY observed_at DESC LIMIT ?1",
            )
            .map_err(|error| Error::new(ErrorKind::Io, format!("read encode samples: {error}")))?;
        let rows = statement
            .query_map([i64::try_from(limit).unwrap_or(i64::MAX)], |row| {
                Ok(EncodeSample {
                    query_chars: row.get::<_, i64>(0)?.try_into().unwrap_or(u32::MAX),
                    elapsed_ms: row.get::<_, i64>(1)?.try_into().unwrap_or(u32::MAX),
                    budget_ms: row.get::<_, i64>(2)?.try_into().unwrap_or(u32::MAX),
                    timed_out: row.get::<_, i64>(3)? != 0,
                    backfill_active: row.get::<_, i64>(4)? != 0,
                })
            })
            .map_err(|error| Error::new(ErrorKind::Io, format!("read encode samples: {error}")))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| Error::new(ErrorKind::Io, format!("read encode samples: {error}")))
    }

    /// Summarises the recorded encode history.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::Io`] when the cache cannot be read.
    pub fn encode_latency_summary(&self) -> Result<EncodeLatencySummary> {
        Ok(EncodeLatencySummary::from_samples(
            &self.recent_encode_samples(SEMANTIC_ENCODE_SAMPLE_HISTORY)?,
        ))
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

/// Encode observations go to the same discardable file the vectors do.
///
/// They belong there and nowhere durable: they describe how this machine performed, they are worth
/// nothing after the operator changes hardware or model, and deleting `semantic.sqlite` to reclaim
/// disk must never be a decision about diagnostics. Every write is best-effort for the same
/// reason -- a full disk degrades observability, never retrieval.
impl EncodeSampleRecorder for SemanticVectorCache {
    fn record(&self, sample: EncodeSample) {
        let Ok(connection) = self.locked() else {
            return;
        };
        if connection
            .execute(
                "INSERT INTO encode_sample
                     (query_chars, elapsed_ms, budget_ms, timed_out, backfill_active)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    i64::from(sample.query_chars),
                    i64::from(sample.elapsed_ms),
                    i64::from(sample.budget_ms),
                    i64::from(sample.timed_out),
                    i64::from(sample.backfill_active),
                ],
            )
            .is_err()
        {
            return;
        }
        let _trimmed = connection.execute(
            "DELETE FROM encode_sample WHERE observed_at <= (
                 SELECT observed_at FROM encode_sample
                 ORDER BY observed_at DESC LIMIT 1 OFFSET ?1
             )",
            [i64::try_from(SEMANTIC_ENCODE_SAMPLE_HISTORY).unwrap_or(i64::MAX)],
        );
    }
}

/// Brings a cache written before `backfill_active` existed up to the current sample schema.
///
/// The observations are dropped rather than migrated, and only the observations. They are a rolling
/// window of at most [`SEMANTIC_ENCODE_SAMPLE_HISTORY`] rows describing how this machine performed
/// in the last few minutes, worth nothing after a restart, and refilled by the next few queries --
/// whereas the corpus vectors in the same file cost hours of encoding, so recreating the *file*
/// to add one diagnostic column would be a real loss for no reason. Backfilling the missing column
/// with `false` would be worse than dropping: it would assert about old rows exactly the thing the
/// column exists to establish.
///
/// # Errors
///
/// Returns [`ErrorKind::Io`] when the table cannot be inspected or replaced.
fn migrate_encode_sample(connection: &Connection) -> Result<()> {
    let has_column = connection
        .prepare("SELECT 1 FROM pragma_table_info('encode_sample') WHERE name = 'backfill_active'")
        .and_then(|mut statement| statement.exists([]))
        .map_err(|error| {
            Error::new(
                ErrorKind::Io,
                format!("inspect encode sample schema: {error}"),
            )
        })?;
    if has_column {
        return Ok(());
    }
    connection
        .execute_batch(
            "DROP TABLE encode_sample;
             CREATE TABLE encode_sample (
                 observed_at INTEGER PRIMARY KEY AUTOINCREMENT,
                 query_chars INTEGER NOT NULL,
                 elapsed_ms INTEGER NOT NULL,
                 budget_ms INTEGER NOT NULL,
                 timed_out INTEGER NOT NULL,
                 backfill_active INTEGER NOT NULL DEFAULT 0
             );",
        )
        .map_err(|error| {
            Error::new(
                ErrorKind::Io,
                format!("rebuild encode sample table: {error}"),
            )
        })
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
    /// Read from the provider at construction, not from a constant: the floor that admits a hit is
    /// a fact about the encoder that produced both sides of the comparison.
    floor_basis_points: u16,
    limit: usize,
    budget: Duration,
    /// Generation this channel's query vectors belong to. A channel built over a bare vector
    /// snapshot has no fingerprint to name, so it gets a private one and shares nothing.
    key: SemanticCacheKey,
    query_cache: Arc<QueryVectorCache>,
    recorder: Option<Arc<dyn EncodeSampleRecorder>>,
}

impl EmbeddingSemanticChannel {
    /// Builds a channel over an in-memory vector snapshot.
    ///
    /// Its query cache is private to this channel: with no model fingerprint there is no way to
    /// tell whether another channel's vectors came from the same encoder, and a shared cache that
    /// might be wrong is worse than one that is merely small.
    #[must_use]
    pub fn new(provider: Arc<dyn EmbeddingProvider>, vectors: Vec<(RevisionId, Vec<f32>)>) -> Self {
        let key = SemanticCacheKey::new("in-memory", "in-memory");
        let query_cache = Arc::new(QueryVectorCache::with_capacity(
            SEMANTIC_QUERY_CACHE_CAPACITY,
        ));
        let floor_basis_points = provider.similarity_floor_basis_points();
        Self {
            provider,
            vectors: Arc::new(vectors),
            floor_basis_points,
            limit: SEMANTIC_CHANNEL_LIMIT,
            budget: SEMANTIC_ENCODE_BUDGET,
            key,
            query_cache,
            recorder: None,
        }
    }

    /// Builds a channel over everything the cache currently holds under `key`.
    ///
    /// The query vector cache is the shared one for `key`, so the second publish that follows a
    /// finished backfill inherits whatever the first one warmed up.
    ///
    /// # Errors
    ///
    /// Returns a typed error when the cache cannot be read.
    pub fn from_cache(
        provider: Arc<dyn EmbeddingProvider>,
        cache: &SemanticVectorCache,
        key: &SemanticCacheKey,
    ) -> Result<Self> {
        let vectors = cache.load(key)?;
        Ok(Self {
            key: key.clone(),
            query_cache: QueryVectorCache::shared(key),
            ..Self::new(provider, vectors)
        })
    }

    /// Overrides the encode budget. `sctx mcp serve` sets it from `[retrieval]
    /// embedding_encode_budget_ms`, and tests that must observe the timeout degradation set it
    /// deliberately low.
    #[must_use]
    pub const fn with_budget(mut self, budget: Duration) -> Self {
        self.budget = budget;
        self
    }

    /// Attaches the sink that records what encodes cost.
    #[must_use]
    pub fn with_encode_recorder(mut self, recorder: Arc<dyn EncodeSampleRecorder>) -> Self {
        self.recorder = Some(recorder);
        self
    }

    /// Replaces the query vector cache, for tests that need an isolated one.
    #[must_use]
    pub fn with_query_cache(mut self, query_cache: Arc<QueryVectorCache>) -> Self {
        self.query_cache = query_cache;
        self
    }

    /// The query vector cache this channel reads and fills.
    #[must_use]
    pub fn query_cache(&self) -> &Arc<QueryVectorCache> {
        &self.query_cache
    }

    /// How many corpus vectors this channel can rank against.
    #[must_use]
    pub fn corpus_size(&self) -> usize {
        self.vectors.len()
    }

    /// Returns the query vector, from cache if possible and from the encoder otherwise.
    ///
    /// The cache lookup comes first and costs no thread, no budget and no model call. Only a miss
    /// reaches the encoder.
    ///
    /// The encode runs on a detached worker so a slow model costs the caller the budget rather
    /// than the encode. Returning `None` on timeout leaves that worker running, and it now does
    /// two useful things before it exits: it stores its vector in the query cache, so the next
    /// read of the same Intent is an immediate hit rather than a second timeout, and it records
    /// what it cost. A budget this machine cannot meet used to be indistinguishable from a missing
    /// model; it is now one slow call followed by hits, and a row in the encode history either
    /// way.
    fn encode_within_budget(&self, text: &str) -> Option<Arc<Vec<f32>>> {
        if let Some(cached) = self.query_cache.get(&self.key, text) {
            return Some(cached);
        }
        let provider = Arc::clone(&self.provider);
        let query_cache = Arc::clone(&self.query_cache);
        let recorder = self.recorder.clone();
        let key = self.key.clone();
        let budget = self.budget;
        let owned = text.to_owned();
        let (sender, receiver) = sync_channel(1);
        let started = Instant::now();
        if std::thread::Builder::new()
            .name("sctx-embedding-query".to_owned())
            .spawn(move || {
                // Asked on both sides of the encode. A backfill that started while this query was
                // queued and one that finished while it ran are both contention this encode paid
                // for, and either reading alone would miss one of them.
                let contended_before = provider.is_backfilling();
                let outcome = provider.encode(&owned);
                let elapsed = started.elapsed();
                let contended = contended_before || provider.is_backfilling();
                let vector = outcome.map(Arc::new);
                if let Ok(vector) = &vector {
                    query_cache.store(&key, &owned, Arc::clone(vector));
                }
                let _ignored = sender.send(vector);
                if let Some(recorder) = recorder {
                    recorder.record(EncodeSample {
                        query_chars: u32::try_from(owned.chars().count()).unwrap_or(u32::MAX),
                        elapsed_ms: u32::try_from(elapsed.as_millis()).unwrap_or(u32::MAX),
                        budget_ms: u32::try_from(budget.as_millis()).unwrap_or(u32::MAX),
                        timed_out: elapsed > budget,
                        backfill_active: contended,
                    });
                }
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
