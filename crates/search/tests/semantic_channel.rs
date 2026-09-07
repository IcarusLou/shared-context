//! The optional embedding recall channel (ADR-0004), end to end without an ONNX model.
//!
//! The suite is split the way the code is. Everything about *how vectors behave* -- determinism,
//! the similarity floor, the channel cap, the discardable cache and its key, the encode budget --
//! runs against [`HashProvider`], a deterministic character-n-gram projection that satisfies the
//! weakest property a real encoder must satisfy: identical text embeds identically and similar
//! text embeds nearby. Everything about *how retrieval uses the channel* -- the fused weight, the
//! independent injection eligibility, the `embedding_unavailable` degradations, and the promise
//! that an unconfigured installation is byte-identical -- runs against [`ScriptedChannel`], which
//! states the channel's answer outright so the assertion is about the wiring and nothing else.
//!
//! No test here loads a model. That is the point: the pipeline has to be provable on a machine
//! with no 2 GB download, or it cannot be regression-tested in CI at all.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use sctx_domain::{
    Applicability, ContextId, ContextKind, ContextRevisionDraft, EvidenceSnapshotDraft,
    EvidenceType, IntentSnapshot, PublicationAction, PublicationDraft, RevisionId, SpaceId, TaskId,
    WorkingIntentSnapshot,
};
use sctx_event_schema::{Event, EventPayload};
use sctx_git_store::{AppendRequest, GitStore};
use sctx_index::ProjectionIndex;
use sctx_search::{
    ContextPackMode, EmbeddingProvider, EmbeddingSemanticChannel, EncodeLatencySummary,
    EncodeSample, EncodeSampleRecorder, Error, ErrorKind, QueryVectorCache, SEMANTIC_CHANNEL_LIMIT,
    SEMANTIC_SIMILARITY_FLOOR_BASIS_POINTS, SearchEngine, SemanticCacheKey, SemanticChannel,
    SemanticChannelHandle, SemanticHit, SemanticOutcome, SemanticVectorCache,
    TaskAssociationChannel, TaskContextPack, TaskContextRequest, TaskRetrievalPath,
};
use serde_json::Value;
use tempfile::TempDir;

// ---------------------------------------------------------------------------------------------
// Test doubles
// ---------------------------------------------------------------------------------------------

/// Deterministic character-n-gram random projection.
///
/// It is not a language model and makes no claim to be one. It holds exactly the two properties
/// the retrieval pipeline depends on -- the same text always yields the same vector, and text that
/// shares character n-grams yields a nearby vector -- which is enough to drive the cache, the
/// floor, the cap and the fusion path through their real code paths.
struct HashProvider {
    dimensions: usize,
    encodes: AtomicUsize,
    delay: Option<Duration>,
    fail: bool,
    backfilling: bool,
}

impl HashProvider {
    fn new() -> Self {
        Self {
            dimensions: 64,
            encodes: AtomicUsize::new(0),
            delay: None,
            fail: false,
            backfilling: false,
        }
    }

    /// A provider that sleeps before answering, for the encode-budget degradation.
    fn slow(delay: Duration) -> Self {
        Self {
            delay: Some(delay),
            ..Self::new()
        }
    }

    /// A slow provider that says a corpus backfill is competing with every encode, for the
    /// difference between "the budget is unfit" and "the backfill window is open".
    fn slow_while_backfilling(delay: Duration) -> Self {
        Self {
            backfilling: true,
            ..Self::slow(delay)
        }
    }

    /// A provider whose every encode fails, for the load-failure degradation.
    fn failing() -> Self {
        Self {
            fail: true,
            ..Self::new()
        }
    }

    fn encode_count(&self) -> usize {
        self.encodes.load(Ordering::SeqCst)
    }
}

/// One 64-bit mix of an n-gram, expanded into a sparse +/-1 contribution.
fn ngram_seed(ngram: &[char]) -> u64 {
    let mut seed = 0xcbf2_9ce4_8422_2325_u64;
    for value in ngram {
        seed ^= u64::from(*value);
        seed = seed.wrapping_mul(0x0000_0100_0000_01b3);
    }
    seed
}

impl EmbeddingProvider for HashProvider {
    fn dimensions(&self) -> usize {
        self.dimensions
    }

    fn is_backfilling(&self) -> bool {
        self.backfilling
    }

    fn encode(&self, text: &str) -> Result<Vec<f32>, Error> {
        self.encodes.fetch_add(1, Ordering::SeqCst);
        if let Some(delay) = self.delay {
            std::thread::sleep(delay);
        }
        if self.fail {
            return Err(Error::new(
                ErrorKind::External,
                "this provider always fails to encode",
            ));
        }
        let characters = text.to_lowercase().chars().collect::<Vec<_>>();
        let mut vector = vec![0.0_f32; self.dimensions];
        // Trigrams, with the whole string as its own gram so a text shorter than three characters
        // still has a direction.
        let grams = if characters.len() < 3 {
            vec![characters.as_slice()]
        } else {
            characters.windows(3).collect::<Vec<_>>()
        };
        for gram in grams {
            let mut seed = ngram_seed(gram);
            for slot in &mut vector {
                // xorshift64*, so each gram spreads deterministically across the whole vector
                // instead of colliding into one bucket.
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                *slot += if seed & 1 == 0 { 1.0 } else { -1.0 };
            }
        }
        let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
        if norm > f32::EPSILON {
            for value in &mut vector {
                *value /= norm;
            }
        }
        Ok(vector)
    }
}

/// A [`HashProvider`] that reports a similarity floor of its own.
///
/// It stands in for a second model family without needing a second model: what matters to the
/// channel is not which encoder produced the floor but that the floor travelled with the provider
/// instead of being read from a constant at the call site.
struct FlooredProvider {
    inner: HashProvider,
    floor: u16,
}

impl EmbeddingProvider for FlooredProvider {
    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }

    fn encode(&self, text: &str) -> Result<Vec<f32>, Error> {
        self.inner.encode(text)
    }

    fn similarity_floor_basis_points(&self) -> u16 {
        self.floor
    }
}

/// A channel that answers whatever the test told it to answer.
struct ScriptedChannel {
    outcome: SemanticOutcome,
    queries: Arc<AtomicUsize>,
}

impl ScriptedChannel {
    fn hits(hits: Vec<SemanticHit>) -> (Arc<Self>, Arc<AtomicUsize>) {
        let queries = Arc::new(AtomicUsize::new(0));
        (
            Arc::new(Self {
                outcome: SemanticOutcome::Hits(hits),
                queries: Arc::clone(&queries),
            }),
            queries,
        )
    }

    fn unavailable() -> Arc<Self> {
        Arc::new(Self {
            outcome: SemanticOutcome::Unavailable,
            queries: Arc::new(AtomicUsize::new(0)),
        })
    }
}

impl SemanticChannel for ScriptedChannel {
    fn similar_revisions(&self, _query_text: &str) -> SemanticOutcome {
        self.queries.fetch_add(1, Ordering::SeqCst);
        self.outcome.clone()
    }
}

// ---------------------------------------------------------------------------------------------
// Corpus fixture
// ---------------------------------------------------------------------------------------------

/// The only token the Working Intent and the lexical corpus share.
const LEXICAL_NEEDLE: &str = "semanticfixtureneedle";
/// Text that appears in the corpus and nowhere in any query, so the Context carrying it is
/// reachable by the embedding channel alone.
const SILENT_TOPIC: &str = "quiescentbackpressureledger";

struct Fixture {
    _temporary: TempDir,
    store: GitStore,
    index: ProjectionIndex,
    lexical_space: SpaceId,
    silent_space: SpaceId,
}

fn intent(title: &str, problem: &str) -> IntentSnapshot {
    IntentSnapshot {
        title: title.to_owned(),
        problem: problem.to_owned(),
        desired_outcome: format!("{problem} stays explainable"),
        in_scope: vec![problem.to_owned()],
        out_of_scope: vec!["SemanticFixtureExcluded".to_owned()],
        acceptance_conditions: vec!["SemanticFixtureAccepted".to_owned()],
        domain_terms: vec!["retrieval".to_owned()],
    }
}

fn draft(statement: &str) -> ContextRevisionDraft {
    ContextRevisionDraft {
        kind: ContextKind::Decision,
        topic_key: Some(format!("semantic/{}", statement.len())),
        problem_view: None,
        statement: statement.to_owned(),
        rationale: "the accepted fixture captures durable retrieval behavior".to_owned(),
        applicability: Applicability {
            domains: vec!["retrieval".to_owned()],
            platforms: vec!["server".to_owned()],
            conditions: vec!["active".to_owned()],
        },
        assumptions: vec!["fixture inputs remain stable".to_owned()],
        recheck_when: vec!["the fixture contract changes".to_owned()],
        hints: Vec::new(),
        relations: Vec::new(),
        evidence: vec![EvidenceSnapshotDraft {
            kind: EvidenceType::ExperimentRecord,
            supports: "the fixture is safe for retrieval".to_owned(),
            content: serde_json::json!({
                "command": "cargo test -p sctx-search --test semantic_channel",
                "actual": "passed"
            }),
            interpretation: "the Context has complete local evidence".to_owned(),
            limitations: vec!["synthetic fixture".to_owned()],
        }],
    }
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let store =
            GitStore::bootstrap_local(temporary.path().join("semantic-installation")).unwrap();
        let lexical_space = Self::space(&store, "LexicalSpace", LEXICAL_NEEDLE);
        let silent_space = Self::space(&store, "SilentSpace", SILENT_TOPIC);
        let index = ProjectionIndex::for_store(&store);
        Self {
            _temporary: temporary,
            store,
            index,
            lexical_space,
            silent_space,
        }
    }

    fn space(store: &GitStore, title: &str, problem: &str) -> SpaceId {
        let created = Event::space_created(intent(title, problem), None).unwrap();
        let EventPayload::SpaceCreated { space_id, .. } = created.payload() else {
            unreachable!()
        };
        let space_id = *space_id;
        store.append_event(AppendRequest::event(created)).unwrap();
        space_id
    }

    fn accept(&self, space_id: SpaceId, statement: &str) -> (ContextId, RevisionId) {
        let added = Event::context_revision_added(space_id, draft(statement), None).unwrap();
        let EventPayload::ContextRevisionAdded {
            context_id,
            revision,
            ..
        } = added.payload()
        else {
            unreachable!()
        };
        let (context_id, revision_id) = (*context_id, revision.revision_id);
        self.store
            .append_event(AppendRequest::event(added))
            .unwrap();
        let published = Event::publication_changed(
            space_id,
            context_id,
            PublicationDraft {
                previous_publication_ids: Vec::new(),
                action: PublicationAction::Publish,
                revision_id,
                review_event_ids: Vec::new(),
            },
            None,
        )
        .unwrap();
        self.store
            .append_event(AppendRequest::event(published))
            .unwrap();
        (context_id, revision_id)
    }

    fn engine(&self) -> SearchEngine {
        self.index.synchronize().unwrap();
        SearchEngine::new(self.index.clone())
    }
}

fn working_intent() -> WorkingIntentSnapshot {
    WorkingIntentSnapshot {
        goal: format!("{LEXICAL_NEEDLE} routing correctness"),
        current_direction: Some(format!("{LEXICAL_NEEDLE} keeps its ranking honest")),
        in_scope: vec![LEXICAL_NEEDLE.to_owned()],
        out_of_scope: Vec::new(),
        domains: Vec::new(),
        platforms: Vec::new(),
        constraints: Vec::new(),
        acceptance_conditions: Vec::new(),
        artifact_hints: Vec::new(),
        interface_hints: Vec::new(),
        open_questions: Vec::new(),
    }
}

fn automatic_pack(engine: &SearchEngine) -> TaskContextPack {
    automatic_pack_for(engine, TaskId::new())
}

fn automatic_pack_for(engine: &SearchEngine, task_id: TaskId) -> TaskContextPack {
    let mut request = TaskContextRequest::automatic(task_id, working_intent(), Vec::new(), 8_000);
    request.mode = ContextPackMode::AutomaticInjection;
    engine.task_context_pack(&request).unwrap()
}

fn omission_reasons(pack: &TaskContextPack) -> Vec<String> {
    pack.omitted
        .iter()
        .map(|omitted| omitted.reason.clone())
        .collect()
}

// ---------------------------------------------------------------------------------------------
// The provider contract the pipeline relies on
// ---------------------------------------------------------------------------------------------

#[test]
fn the_provider_is_deterministic_and_orders_similar_text_above_unrelated_text() {
    let provider = HashProvider::new();
    let anchor = provider
        .encode("retry outside the deduplication window")
        .unwrap();

    assert_eq!(
        anchor,
        provider
            .encode("retry outside the deduplication window")
            .unwrap(),
        "the same text must embed identically or a cached corpus vector and a freshly encoded \
         query stop being comparable"
    );
    assert_eq!(anchor.len(), provider.dimensions());
    let norm = anchor.iter().map(|value| value * value).sum::<f32>().sqrt();
    assert!(
        (norm - 1.0).abs() < 1e-4,
        "every vector must be L2-normalized so cosine is a dot product, got norm {norm}"
    );

    let near = provider
        .encode("retry outside the deduplication windows")
        .unwrap();
    let far = provider
        .encode("espresso extraction pressure curves")
        .unwrap();
    let near_similarity = cosine(&anchor, &near);
    let far_similarity = cosine(&anchor, &far);
    assert!(
        near_similarity > far_similarity,
        "near text scored {near_similarity} and unrelated text {far_similarity}"
    );
}

fn cosine(left: &[f32], right: &[f32]) -> f32 {
    left.iter().zip(right).map(|(a, b)| a * b).sum()
}

// ---------------------------------------------------------------------------------------------
// Cache: key invalidation and discardability
// ---------------------------------------------------------------------------------------------

#[test]
fn cached_vectors_are_invisible_under_a_different_model_or_ranking_version() {
    let temporary = tempfile::tempdir().unwrap();
    let cache = SemanticVectorCache::open(&temporary.path().join("state/semantic.sqlite")).unwrap();
    let provider = HashProvider::new();
    let revision = RevisionId::new();
    let vector = provider.encode("a decision worth caching").unwrap();

    let original = SemanticCacheKey::new("model-fingerprint-a", "6");
    cache.store(&original, revision, &vector).unwrap();
    assert_eq!(cache.load(&original).unwrap().len(), 1);

    // A different model is a different vector space. Reading its vectors as if they were this
    // model's would not be slightly wrong, it would be meaningless.
    let other_model = SemanticCacheKey::new("model-fingerprint-b", "6");
    assert!(
        cache.load(&other_model).unwrap().is_empty(),
        "vectors must not leak across model fingerprints"
    );
    // A SEARCH_RANKING_VERSION bump is the signal that the indexed text changed shape.
    let other_ranking = SemanticCacheKey::new("model-fingerprint-a", "7");
    assert!(
        cache.load(&other_ranking).unwrap().is_empty(),
        "vectors must not leak across ranking versions"
    );

    assert!(
        cache
            .cached_revisions(&original)
            .unwrap()
            .contains(&revision)
    );
    assert!(cache.cached_revisions(&other_model).unwrap().is_empty());

    // Pruning against the new key must reclaim the superseded generation, not the current one.
    cache.store(&other_model, revision, &vector).unwrap();
    let pruned = cache.prune_superseded(&other_model).unwrap();
    assert_eq!(pruned, 1, "exactly the superseded generation is dropped");
    assert!(cache.load(&original).unwrap().is_empty());
    assert_eq!(cache.load(&other_model).unwrap().len(), 1);
}

#[test]
fn the_cache_round_trips_vectors_bit_for_bit() {
    let temporary = tempfile::tempdir().unwrap();
    let cache = SemanticVectorCache::open(&temporary.path().join("state/semantic.sqlite")).unwrap();
    let provider = HashProvider::new();
    let key = SemanticCacheKey::new("fingerprint", "6");
    let revision = RevisionId::new();
    let vector = provider.encode("round trip me").unwrap();

    cache.store(&key, revision, &vector).unwrap();
    let loaded = cache.load(&key).unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].0, revision);
    assert_eq!(
        loaded[0].1, vector,
        "a re-read vector must be identical, not merely close: cosine against a query is only \
         reproducible if the stored bits are"
    );
}

#[test]
fn a_writer_holding_the_lock_does_not_make_a_concurrent_store_drop_its_write() {
    // Reproduces the two real writers that share `semantic.sqlite`: the editor's long-lived
    // `serve` process backfilling on a background thread, and a `doctor --fix` synchronous warm
    // running at the same time. Both call `SemanticVectorCache::store`, and both are separate
    // connections to the same file -- exactly the shape that produces `SQLITE_BUSY` for as long as
    // the other side holds the write lock, which `crates/mcp/src/semantic.rs` then silently treats
    // as "nothing to store" (`if cache.store(..).is_ok() { .. }`) and the vector is gone for good.
    // A too-short or absent `busy_timeout` turns an ordinary lock hold into a dropped write; this
    // pins `SEMANTIC_CACHE_BUSY_TIMEOUT` as long enough to ride one out. (`Connection::open`
    // already carries rusqlite's own 5 s default, so this also guards against a future change
    // silently narrowing that default underneath an unrelated rusqlite upgrade.)
    //
    // This holds a real write lock open on a *third* connection to force the contention
    // deterministically, then proves the cache's own connection waits it out instead of failing.
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("state/semantic.sqlite");
    let cache = Arc::new(SemanticVectorCache::open(&path).unwrap());
    // Opening the cache once already creates the schema and the parent directory; a second raw
    // connection to the same file can now take the write lock.
    let mut locker = rusqlite::Connection::open(&path).unwrap();
    locker.busy_timeout(Duration::from_millis(0)).unwrap();
    let key = SemanticCacheKey::new("fingerprint", "6");
    let provider = HashProvider::new();

    let hold = Duration::from_millis(400);
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let locker_barrier = Arc::clone(&barrier);
    let locking_thread = std::thread::spawn(move || {
        let transaction = locker
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        locker_barrier.wait();
        std::thread::sleep(hold);
        transaction.commit().unwrap();
    });

    // Give the locking thread a head start so its `BEGIN IMMEDIATE` is in place before the cache
    // tries to write, then race the cache's own store against the held lock.
    barrier.wait();
    std::thread::sleep(Duration::from_millis(20));
    let started = Instant::now();
    let revision = RevisionId::new();
    let vector = provider
        .encode("written while another connection holds the lock")
        .unwrap();
    let result = cache.store(&key, revision, &vector);
    let waited = started.elapsed();

    locking_thread.join().unwrap();

    assert!(
        result.is_ok(),
        "store must wait out the busy_timeout and succeed, not drop the write: {result:?}"
    );
    assert!(
        waited >= Duration::from_millis(200),
        "the store returned in {waited:?} without ever actually waiting on the held lock, so \
         this run did not exercise the busy_timeout at all"
    );
    assert!(
        cache.cached_revisions(&key).unwrap().contains(&revision),
        "the vector written while the lock was held must be durably in the cache"
    );
}

#[test]
fn many_concurrent_writers_lose_no_stores_under_busy_timeout() {
    // The realistic shape: no one is pinning the lock open, several writers are just issuing
    // stores back to back from separate connections (separate `SemanticVectorCache::open` calls,
    // the way two processes each would). This is real writer-writer contention, not staged: with
    // `SEMANTIC_CACHE_BUSY_TIMEOUT` set to `Duration::ZERO` this flakes under exactly this load
    // with `SQLITE_BUSY` on whichever writer loses the race, so it is not a vacuous assertion --
    // every store below must land.
    const WRITERS: usize = 6;
    const STORES_PER_WRITER: usize = 20;

    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("state/semantic.sqlite");
    // Establish the schema up front so every writer thread races on writes only, not on
    // `CREATE TABLE IF NOT EXISTS`.
    drop(SemanticVectorCache::open(&path).unwrap());

    let key = SemanticCacheKey::new("fingerprint", "6");
    let mut revisions = Vec::with_capacity(WRITERS * STORES_PER_WRITER);
    let mut handles = Vec::with_capacity(WRITERS);
    for _ in 0..WRITERS {
        let path = path.clone();
        let key = key.clone();
        let writer_revisions: Vec<RevisionId> =
            (0..STORES_PER_WRITER).map(|_| RevisionId::new()).collect();
        revisions.extend_from_slice(&writer_revisions);
        handles.push(std::thread::spawn(move || {
            let cache = SemanticVectorCache::open(&path).unwrap();
            let provider = HashProvider::new();
            for (index, revision) in writer_revisions.into_iter().enumerate() {
                let vector = provider
                    .encode(&format!("concurrent writer text {index}"))
                    .unwrap();
                cache.store(&key, revision, &vector).expect(
                    "busy_timeout must absorb writer-writer contention, not drop the write",
                );
            }
        }));
    }
    for handle in handles {
        handle.join().unwrap();
    }

    let verifier = SemanticVectorCache::open(&path).unwrap();
    let stored = verifier.cached_revisions(&key).unwrap();
    for revision in &revisions {
        assert!(
            stored.contains(revision),
            "revision {revision} written by a concurrent writer is missing from the cache"
        );
    }
    assert_eq!(stored.len(), revisions.len());
}

// ---------------------------------------------------------------------------------------------
// Channel: floor, cap, and the encode budget
// ---------------------------------------------------------------------------------------------

/// Builds a channel whose corpus is the given texts, encoded by a fresh [`HashProvider`].
fn hash_channel(texts: &[&str]) -> (EmbeddingSemanticChannel, Vec<RevisionId>) {
    let provider = Arc::new(HashProvider::new());
    let mut vectors = Vec::new();
    let mut revisions = Vec::new();
    for text in texts {
        let revision = RevisionId::new();
        revisions.push(revision);
        vectors.push((revision, provider.encode(text).unwrap()));
    }
    (EmbeddingSemanticChannel::new(provider, vectors), revisions)
}

/// The two corpus texts every floor assertion here uses: one the query repeats verbatim, one about
/// something else entirely.
const FLOOR_CORPUS: [&str; 2] = [
    "retry outside the deduplication window is delivered twice",
    "espresso extraction pressure curves for a hand pour",
];

#[test]
fn a_provider_that_names_no_floor_gets_the_bge_m3_calibration() {
    assert_eq!(
        HashProvider::new().similarity_floor_basis_points(),
        SEMANTIC_SIMILARITY_FLOOR_BASIS_POINTS,
        "the trait default is the number T5a measured against bge-m3, so every provider written \
         before a second family existed keeps scoring exactly as it did"
    );
}

#[test]
fn the_channel_takes_its_floor_from_the_provider() {
    // Same corpus and same query as the floor test below, so the only difference between the two
    // outcomes is which floor the channel was handed.
    let provider = Arc::new(FlooredProvider {
        inner: HashProvider::new(),
        floor: 0,
    });
    let vectors = FLOOR_CORPUS
        .iter()
        .map(|text| (RevisionId::new(), provider.encode(text).unwrap()))
        .collect::<Vec<_>>();
    let channel = EmbeddingSemanticChannel::new(provider, vectors);

    let SemanticOutcome::Hits(hits) = channel.similar_revisions(FLOOR_CORPUS[0]) else {
        panic!("a loaded channel over a non-empty corpus must run");
    };
    assert_eq!(
        hits.len(),
        FLOOR_CORPUS.len(),
        "the unrelated text clears a floor of zero, so the channel read the provider's floor and \
         not the bge-m3 constant; got {hits:?}"
    );
}

#[test]
fn the_similarity_floor_admits_the_near_match_and_rejects_the_unrelated_one() {
    let (channel, revisions) = hash_channel(&FLOOR_CORPUS);

    let SemanticOutcome::Hits(hits) = channel.similar_revisions(FLOOR_CORPUS[0]) else {
        panic!("a loaded channel over a non-empty corpus must run");
    };
    assert_eq!(
        hits.len(),
        1,
        "only the near text may clear the floor; got {hits:?}"
    );
    assert_eq!(hits[0].revision_id, revisions[0]);
    assert!(
        hits[0].similarity_basis_points >= SEMANTIC_SIMILARITY_FLOOR_BASIS_POINTS,
        "an admitted hit always reports a similarity at or above the floor"
    );

    // The noise query matches nothing in the corpus, and an empty hit list is a channel that ran,
    // not a broken one.
    let outcome = channel.similar_revisions("an entirely unrelated question about gardening");
    assert!(
        matches!(&outcome, SemanticOutcome::Hits(hits) if hits.is_empty()),
        "a query above nothing must report zero hits, never Unavailable: got {outcome:?}"
    );
}

#[test]
fn the_channel_never_contributes_more_than_its_cap() {
    let text = "the identical decision text every corpus entry carries";
    let texts = vec![text; SEMANTIC_CHANNEL_LIMIT + 9];
    let (channel, _) = hash_channel(&texts);

    let SemanticOutcome::Hits(hits) = channel.similar_revisions(text) else {
        panic!("the channel must run");
    };
    assert_eq!(
        hits.len(),
        SEMANTIC_CHANNEL_LIMIT,
        "every corpus entry is an exact match, so only the cap can bound the channel"
    );
    // Identical similarity everywhere: the tie-break has to be the Revision ID, or the same query
    // would fuse differently on different runs.
    let mut sorted = hits.iter().map(|hit| hit.revision_id).collect::<Vec<_>>();
    let ordered = sorted.clone();
    sorted.sort_unstable();
    assert_eq!(
        ordered, sorted,
        "ties must break on Revision ID so a fused score is reproducible"
    );
}

#[test]
fn an_encode_that_overruns_its_budget_reports_the_channel_unavailable() {
    let provider = Arc::new(HashProvider::slow(Duration::from_millis(400)));
    let vector = provider.encode("corpus entry").unwrap();
    let channel = EmbeddingSemanticChannel::new(provider, vec![(RevisionId::new(), vector)])
        .with_budget(Duration::from_millis(30));

    assert_eq!(
        channel.similar_revisions("a query the model is too slow to answer"),
        SemanticOutcome::Unavailable,
        "overrunning the encode budget degrades the channel; it never stalls the Pack"
    );
}

#[test]
fn a_repeated_query_is_served_from_the_cache_without_encoding_again() {
    let provider = Arc::new(HashProvider::new());
    let vector = provider.encode("corpus entry").unwrap();
    let channel = EmbeddingSemanticChannel::new(
        Arc::clone(&provider) as Arc<dyn EmbeddingProvider>,
        vec![(RevisionId::new(), vector)],
    );

    let first = channel.similar_revisions("the same working intent, read twice");
    let encodes_after_first = provider.encode_count();
    let second = channel.similar_revisions("the same working intent, read twice");

    assert_eq!(
        first, second,
        "a cached query vector must rank exactly as the freshly encoded one did"
    );
    assert_eq!(
        provider.encode_count(),
        encodes_after_first,
        "the second read of one Working Intent must not reach the encoder at all"
    );
    let _third = channel.similar_revisions("a different working intent entirely");
    assert!(
        provider.encode_count() > encodes_after_first,
        "a query the cache has never seen must still be encoded"
    );
}

#[test]
fn an_encode_that_overran_its_budget_still_lands_in_the_cache_for_the_next_call() {
    // The defect this covers is the one that made the channel useless in a real session: a budget
    // the machine cannot meet used to fail every call identically and forever, because the encode
    // that overran was simply thrown away. Now the worker finishes into the cache, so the cost is
    // paid once.
    let provider = Arc::new(HashProvider::slow(Duration::from_millis(200)));
    let vector = provider.encode("corpus entry").unwrap();
    let channel = EmbeddingSemanticChannel::new(
        Arc::clone(&provider) as Arc<dyn EmbeddingProvider>,
        vec![(RevisionId::new(), vector)],
    )
    .with_budget(Duration::from_millis(20));

    assert_eq!(
        channel.similar_revisions("a long working intent this budget cannot encode"),
        SemanticOutcome::Unavailable,
        "the first call still degrades rather than stalling the Pack"
    );

    // Wait for the detached worker, which is still encoding, to finish and fill the cache.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while channel.query_cache().is_empty() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }

    assert!(
        matches!(
            channel.similar_revisions("a long working intent this budget cannot encode"),
            SemanticOutcome::Hits(_)
        ),
        "the next read of the same Intent must be a cache hit, not a second timeout"
    );
}

#[test]
fn the_query_cache_evicts_the_least_recently_read_entry() {
    let cache = QueryVectorCache::with_capacity(2);
    let key = SemanticCacheKey::new("fingerprint", "ranking");
    cache.store(&key, "first", Arc::new(vec![1.0_f32]));
    cache.store(&key, "second", Arc::new(vec![2.0_f32]));
    // Reading "first" makes "second" the coldest entry.
    assert!(cache.get(&key, "first").is_some());
    cache.store(&key, "third", Arc::new(vec![3.0_f32]));

    assert_eq!(cache.len(), 2, "capacity is a bound, not a suggestion");
    assert!(
        cache.get(&key, "first").is_some(),
        "the recently read entry survives"
    );
    assert!(
        cache.get(&key, "third").is_some(),
        "the newest entry survives"
    );
    assert!(
        cache.get(&key, "second").is_none(),
        "the least recently read entry is the one evicted"
    );
}

#[test]
fn a_query_vector_is_never_served_across_model_generations() {
    let cache = QueryVectorCache::with_capacity(8);
    let original = SemanticCacheKey::new("fingerprint-a", "ranking-1");
    cache.store(&original, "one intent", Arc::new(vec![1.0_f32]));

    assert!(cache.get(&original, "one intent").is_some());
    assert!(
        cache
            .get(
                &SemanticCacheKey::new("fingerprint-b", "ranking-1"),
                "one intent"
            )
            .is_none(),
        "a swapped model must not read the previous model's query vectors"
    );
    assert!(
        cache
            .get(
                &SemanticCacheKey::new("fingerprint-a", "ranking-2"),
                "one intent"
            )
            .is_none(),
        "a ranking version bump changes the embedded text, so its vectors are a new generation"
    );
}

#[test]
fn encode_timings_are_recorded_where_status_and_doctor_can_read_them() {
    let directory = TempDir::new().unwrap();
    let cache =
        Arc::new(SemanticVectorCache::open(&directory.path().join("semantic.sqlite")).unwrap());

    assert_eq!(
        cache.encode_latency_summary().unwrap(),
        EncodeLatencySummary::default(),
        "a channel nobody has queried reports nothing rather than a fabricated zero-latency run"
    );

    let provider = Arc::new(HashProvider::slow(Duration::from_millis(120)));
    let vector = provider.encode("corpus entry").unwrap();
    let channel = EmbeddingSemanticChannel::new(provider, vec![(RevisionId::new(), vector)])
        .with_budget(Duration::from_millis(15))
        .with_encode_recorder(Arc::clone(&cache) as Arc<dyn EncodeSampleRecorder>);

    assert_eq!(
        channel.similar_revisions("a query this budget cannot afford"),
        SemanticOutcome::Unavailable
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while cache.recent_encode_samples(8).unwrap().is_empty() && std::time::Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(10));
    }

    let samples = cache.recent_encode_samples(8).unwrap();
    assert_eq!(samples.len(), 1, "one query, one observation");
    assert!(
        samples[0].timed_out,
        "an encode that overran its budget has to be recorded as one: the Pack cannot tell a \
         timeout from a missing model, so this is the only place the difference survives"
    );
    assert_eq!(samples[0].budget_ms, 15);
    assert!(
        samples[0].elapsed_ms >= 100,
        "the real cost is recorded, not the budget"
    );
    assert!(samples[0].query_chars > 0);
    assert!(
        !samples[0].backfill_active,
        "nothing was backfilling, so this timeout is the encoder's own and has to read as one"
    );
}

#[test]
fn a_timeout_taken_while_the_corpus_backfills_is_recorded_as_a_contended_one() {
    let directory = TempDir::new().unwrap();
    let cache =
        Arc::new(SemanticVectorCache::open(&directory.path().join("semantic.sqlite")).unwrap());

    let provider = Arc::new(HashProvider::slow_while_backfilling(Duration::from_millis(
        120,
    )));
    let vector = provider.encode("corpus entry").unwrap();
    let channel = EmbeddingSemanticChannel::new(provider, vec![(RevisionId::new(), vector)])
        .with_budget(Duration::from_millis(15))
        .with_encode_recorder(Arc::clone(&cache) as Arc<dyn EncodeSampleRecorder>);

    assert_eq!(
        channel.similar_revisions("a query the backfill is standing on"),
        SemanticOutcome::Unavailable
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while cache.recent_encode_samples(8).unwrap().is_empty() && std::time::Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(10));
    }

    let samples = cache.recent_encode_samples(8).unwrap();
    assert_eq!(samples.len(), 1, "one query, one observation");
    assert!(samples[0].timed_out);
    assert!(
        samples[0].backfill_active,
        "a timeout the backfill caused has to survive as a different fact from one the budget \
         caused, or `sctx doctor` sends the operator to raise a budget that was never the problem"
    );

    let summary = cache.encode_latency_summary().unwrap();
    assert_eq!(summary.timed_out, 1);
    assert_eq!(summary.timed_out_during_backfill, 1);
}

#[test]
fn a_history_of_timeouts_is_what_marks_a_budget_unfit_and_a_short_one_is_not() {
    let timeout = EncodeSample {
        query_chars: 283,
        elapsed_ms: 240,
        budget_ms: 200,
        timed_out: true,
        backfill_active: false,
    };
    let inside = EncodeSample {
        timed_out: false,
        elapsed_ms: 30,
        ..timeout
    };

    assert!(
        !EncodeLatencySummary::from_samples(&[timeout; 4]).budget_is_unfit(),
        "four observations is a busy machine, not a verdict about the budget"
    );
    assert!(
        EncodeLatencySummary::from_samples(&[timeout; 10]).budget_is_unfit(),
        "a full history of timeouts is a budget this machine cannot meet"
    );

    let mut mixed = vec![inside; 9];
    mixed.push(timeout);
    assert!(
        !EncodeLatencySummary::from_samples(&mixed).budget_is_unfit(),
        "one timeout among nine healthy encodes is not a misconfiguration"
    );
    let summary = EncodeLatencySummary::from_samples(&mixed);
    assert_eq!(summary.samples, 10);
    assert_eq!(summary.timed_out, 1);
    assert_eq!(summary.max_ms, 240);

    // The reason the column exists. The same ten timeouts that condemn the budget above say
    // nothing about it once they are attributed to the backfill window they were taken in.
    let contended = EncodeSample {
        backfill_active: true,
        ..timeout
    };
    let summary = EncodeLatencySummary::from_samples(&[contended; 10]);
    assert_eq!(summary.timed_out, 10, "the degradations are still recorded");
    assert_eq!(summary.timed_out_during_backfill, 10);
    assert!(
        !summary.budget_is_unfit(),
        "a history of timeouts taken while the corpus backfilled is a window that closes, not a \
         budget the machine cannot meet"
    );
}

#[test]
fn a_provider_that_cannot_encode_reports_the_channel_unavailable() {
    let provider = Arc::new(HashProvider::failing());
    let channel =
        EmbeddingSemanticChannel::new(provider, vec![(RevisionId::new(), vec![0.5_f32; 64])]);

    assert_eq!(
        channel.similar_revisions("a query nothing can encode"),
        SemanticOutcome::Unavailable
    );
}

#[test]
fn an_unfilled_handle_is_unavailable_and_becomes_usable_the_moment_it_is_published() {
    let handle = SemanticChannelHandle::new();
    assert!(!handle.is_ready());
    assert_eq!(
        handle.similar_revisions("anything at all"),
        SemanticOutcome::Unavailable,
        "the 9-12 second model load must never be something a query waits for"
    );

    let (channel, revisions) = hash_channel(&["a decision about retry deduplication windows"]);
    handle.publish(Arc::new(channel));
    assert!(handle.is_ready());
    let SemanticOutcome::Hits(hits) =
        handle.similar_revisions("a decision about retry deduplication windows")
    else {
        panic!("a published handle answers from its channel");
    };
    assert_eq!(hits[0].revision_id, revisions[0]);
}

#[test]
fn the_query_encode_costs_exactly_one_model_call() {
    let provider = Arc::new(HashProvider::new());
    let corpus = (0..8)
        .map(|index| {
            (
                RevisionId::new(),
                provider.encode(&format!("corpus entry {index}")).unwrap(),
            )
        })
        .collect::<Vec<_>>();
    let baseline = provider.encode_count();
    let shared: Arc<dyn EmbeddingProvider> = Arc::clone(&provider) as Arc<dyn EmbeddingProvider>;
    let channel = EmbeddingSemanticChannel::new(shared, corpus);

    let _outcome = channel.similar_revisions("corpus entry 3");
    assert_eq!(
        provider.encode_count() - baseline,
        1,
        "cosine runs against the cache; only the query is ever encoded on the read path"
    );
}

// ---------------------------------------------------------------------------------------------
// Retrieval wiring
// ---------------------------------------------------------------------------------------------

#[test]
fn an_unconfigured_installation_retrieves_exactly_as_it_did_before_the_channel_existed() {
    let fixture = Fixture::new();
    fixture.accept(
        fixture.lexical_space,
        &format!("{LEXICAL_NEEDLE} routing is decided here"),
    );
    fixture.accept(
        fixture.silent_space,
        &format!("{SILENT_TOPIC} shapes the outbox drain"),
    );
    let engine = fixture.engine();

    let task_id = TaskId::new();
    let pack = automatic_pack_for(&engine, task_id);
    let serialized = serde_json::to_string(&pack).unwrap();

    for absent in [
        "semantic_similarity",
        "similarity_basis_points",
        "embedding_unavailable",
    ] {
        assert!(
            !serialized.contains(absent),
            "an installation with no `[retrieval]` table must not serialize {absent}: {serialized}"
        );
    }
    assert!(
        !pack.items.is_empty(),
        "the lexical channels still answer, unchanged"
    );

    // The one and only difference an attached-but-unavailable channel may make is the omission.
    let degraded = engine
        .clone()
        .with_semantic_channel(ScriptedChannel::unavailable());
    let degraded_pack = automatic_pack_for(&degraded, task_id);
    assert_eq!(
        omission_reasons(&degraded_pack),
        vec!["embedding_unavailable".to_owned()],
        "a configured channel that could not run says so, once"
    );
    assert_eq!(
        strip_omissions(&pack),
        strip_omissions(&degraded_pack),
        "an unavailable channel changes nothing but the omission it reports"
    );
    assert!(
        omission_reasons(&pack).is_empty(),
        "an installation with no channel has nothing to omit"
    );
}

/// The Pack with its `omitted` list, and the budget that list consumes, removed -- so two Packs
/// can be compared on everything the omission is not allowed to touch.
fn strip_omissions(pack: &TaskContextPack) -> Value {
    let mut value = serde_json::to_value(pack).unwrap();
    if let Some(object) = value.as_object_mut() {
        object.remove("omitted");
        object.remove("estimated_tokens");
    }
    value
}

#[test]
fn every_degradation_reports_one_embedding_unavailable_omission() {
    let fixture = Fixture::new();
    fixture.accept(
        fixture.lexical_space,
        &format!("{LEXICAL_NEEDLE} routing is decided here"),
    );
    let engine = fixture.engine();

    // 1. The background load has not finished yet.
    let loading = engine
        .clone()
        .with_semantic_channel(Arc::new(SemanticChannelHandle::new()));
    assert_eq!(
        omission_reasons(&automatic_pack(&loading)),
        vec!["embedding_unavailable".to_owned()]
    );

    // 2. The model is loaded but cannot encode this query.
    let failing = EmbeddingSemanticChannel::new(
        Arc::new(HashProvider::failing()),
        vec![(RevisionId::new(), vec![0.5_f32; 64])],
    );
    let broken = engine.clone().with_semantic_channel(Arc::new(failing));
    assert_eq!(
        omission_reasons(&automatic_pack(&broken)),
        vec!["embedding_unavailable".to_owned()]
    );

    // 3. The encode overran its budget.
    let provider = Arc::new(HashProvider::slow(Duration::from_millis(300)));
    let vector = provider.encode("corpus entry").unwrap();
    let slow = EmbeddingSemanticChannel::new(provider, vec![(RevisionId::new(), vector)])
        .with_budget(Duration::from_millis(20));
    let stalled = engine.clone().with_semantic_channel(Arc::new(slow));
    let stalled_pack = automatic_pack(&stalled);
    assert_eq!(
        omission_reasons(&stalled_pack),
        vec!["embedding_unavailable".to_owned()]
    );
    assert!(
        !stalled_pack.items.is_empty(),
        "the lexical answer must survive every embedding degradation intact"
    );
}

#[test]
fn a_semantic_hit_alone_makes_a_space_injectable_and_explains_itself() {
    let fixture = Fixture::new();
    fixture.accept(
        fixture.lexical_space,
        &format!("{LEXICAL_NEEDLE} routing is decided here"),
    );
    let (silent_context, silent_revision) = fixture.accept(
        fixture.silent_space,
        &format!("{SILENT_TOPIC} shapes the outbox drain"),
    );
    let engine = fixture.engine();

    // Without the channel, nothing in the Working Intent can reach the silent Space: it shares no
    // token, no hint, no scope and no Artifact with the query.
    let lexical_only = automatic_pack(&engine);
    assert!(
        !lexical_only
            .items
            .iter()
            .any(|item| item.context.context_id == silent_context),
        "the fixture is only meaningful if lexical recall cannot reach the silent Space"
    );

    let (channel, queries) = ScriptedChannel::hits(vec![SemanticHit {
        revision_id: silent_revision,
        similarity_basis_points: 6_400,
    }]);
    let semantic = engine.clone().with_semantic_channel(channel);
    let pack = automatic_pack(&semantic);

    assert_eq!(
        queries.load(Ordering::SeqCst),
        1,
        "one Pack asks the channel once, however many times it rereads the Graph"
    );
    let injected = pack
        .items
        .iter()
        .find(|item| item.context.context_id == silent_context)
        .expect("a semantic hit is an injection eligibility path in its own right");

    assert_eq!(
        injected.context.match_reason.similarity_basis_points,
        Some(6_400),
        "the item reports the similarity that admitted it"
    );
    assert!(
        injected
            .retrieval_paths
            .contains(&TaskRetrievalPath::SemanticSimilarity {
                similarity_basis_points: 6_400,
            }),
        "the route has to be explainable, not merely effective: {:?}",
        injected.retrieval_paths
    );
    assert!(
        injected.context.match_reason.matched_tokens.is_empty(),
        "this Context was reached without sharing a single query token"
    );
}

#[test]
fn the_semantic_channel_fuses_at_the_hint_channel_weight_without_deflating_lexical_scores() {
    let fixture = Fixture::new();
    fixture.accept(
        fixture.lexical_space,
        &format!("{LEXICAL_NEEDLE} routing is decided here"),
    );
    let (_, silent_revision) = fixture.accept(
        fixture.silent_space,
        &format!("{SILENT_TOPIC} shapes the outbox drain"),
    );
    let engine = fixture.engine();

    let lexical_only = automatic_pack(&engine);
    let baseline = lexical_only
        .associations
        .iter()
        .map(|association| (association.space_id, association.score))
        .collect::<Vec<_>>();

    let (channel, _) = ScriptedChannel::hits(vec![SemanticHit {
        revision_id: silent_revision,
        similarity_basis_points: 7_100,
    }]);
    let fused = automatic_pack(&engine.clone().with_semantic_channel(channel));

    // Every Space the lexical channels ranked keeps exactly the score it had. This is the property
    // that lets the channel ship without re-measuring AUTOMATIC_RELEVANCE_FLOOR_BASIS_POINTS.
    for (space_id, score) in baseline {
        let after = fused
            .associations
            .iter()
            .find(|association| association.space_id == space_id)
            .map(|association| association.score);
        assert_eq!(
            after,
            Some(score),
            "the new channel must be purely additive; Space {space_id} moved"
        );
    }

    let semantic_association = fused
        .associations
        .iter()
        .find(|association| association.space_id == fixture.silent_space)
        .expect("the semantically matched Space is now associated");
    let explanation: Value = serde_json::from_str(
        semantic_association
            .reasons
            .first()
            .expect("every association leads with its typed fusion explanation"),
    )
    .expect("a typed fusion explanation");
    let channels = explanation["channels"].as_array().expect("channel list");
    let semantic_channel = channels
        .iter()
        .find(|channel| channel["channel"] == "semantic_similarity")
        .expect("the semantic channel is reported in the fusion explanation");

    assert_eq!(semantic_channel["rank"], 1);
    assert_eq!(
        semantic_channel["exact_match_strength"], 7_100,
        "the explanation carries the similarity that produced the rank"
    );
    // RRF at rank 1 is 1_000_000 / (RRF_K + 1), weighted by the channel's own weight of 3.
    assert_eq!(
        semantic_channel["reciprocal_rank_micros"],
        (1_000_000_u32 / 61) * 3,
        "the channel fuses at the hint-channel weight ADR-0004 fixed it to"
    );
}

#[test]
fn channel_ordering_stays_stable_so_explanations_are_comparable_across_runs() {
    // `TaskAssociationChannel` is `Ord`-derived and the fusion explanation sorts by it, so a
    // variant inserted anywhere but the end would silently reorder every existing explanation.
    assert!(TaskAssociationChannel::ExactScope < TaskAssociationChannel::SemanticSimilarity);
    assert!(TaskAssociationChannel::SpaceIntentBm25 < TaskAssociationChannel::SemanticSimilarity);
    assert!(
        TaskAssociationChannel::ResolvedArtifactExact < TaskAssociationChannel::SemanticSimilarity
    );
}

#[test]
fn explicit_search_is_untouched_by_the_channel() {
    let fixture = Fixture::new();
    fixture.accept(
        fixture.lexical_space,
        &format!("{LEXICAL_NEEDLE} routing is decided here"),
    );
    let (_, silent_revision) = fixture.accept(
        fixture.silent_space,
        &format!("{SILENT_TOPIC} shapes the outbox drain"),
    );
    let engine = fixture.engine();

    let (channel, queries) = ScriptedChannel::hits(vec![SemanticHit {
        revision_id: silent_revision,
        similarity_basis_points: 9_000,
    }]);
    let semantic = engine.clone().with_semantic_channel(channel);

    let mut request =
        TaskContextRequest::automatic(TaskId::new(), working_intent(), Vec::new(), 8_000);
    request.mode = ContextPackMode::Explicit;
    let explicit = semantic.task_context_pack(&request).unwrap();

    assert_eq!(
        queries.load(Ordering::SeqCst),
        0,
        "ADR-0004 scopes the channel to automatic injection; an explicit, paged query must not \
         rank against a cache the backfill is still filling"
    );
    let serialized = serde_json::to_string(&explicit).unwrap();
    assert!(!serialized.contains("semantic_similarity"));
    assert!(!serialized.contains("embedding_unavailable"));
}
