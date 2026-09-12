//! The optional embedding recall channel (ADR-0004), end to end without an ONNX model.
//!
//! The suite is split the way the code is. Everything about *how vectors behave* -- determinism,
//! the discardable cache and its key, the second hop's admission record -- runs against
//! [`HashProvider`], a deterministic character-n-gram projection that satisfies the weakest
//! property a real encoder must satisfy: identical text embeds identically and similar text embeds
//! nearby. Everything about *how retrieval uses the channel* -- the `embedding_unavailable`
//! degradation and the promise that an unconfigured installation is byte-identical -- runs against
//! [`ScriptedChannel`], which states the channel's answer outright so the assertion is about the
//! wiring and nothing else.
//!
//! There is no query-side section any more. ADR-0007 retired the path that encoded a caller's text
//! and ranked the corpus against it, so the floor, the channel cap, the encode budget, the query
//! vector cache and the encode telemetry are gone, and so are the tests that measured them. What
//! is left is the corpus and the hop that reads it.
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
    ContextPackMode, DocumentVectorSnapshot, EmbeddingProvider, EmbeddingSemanticChannel, Error,
    Hop2AdmissionSample, SEMANTIC_CORPUS_VERSION, SEMANTIC_HOP2_ADMISSION_FLOOR_BASIS_POINTS,
    SEMANTIC_HOP2_SAMPLE_HISTORY, SearchEngine, SemanticCacheKey, SemanticChannel,
    SemanticChannelHandle, SemanticVectorCache, TaskContextPack, TaskContextRequest,
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
}

impl HashProvider {
    fn new() -> Self {
        Self {
            dimensions: 64,
            encodes: AtomicUsize::new(0),
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

    fn encode(&self, text: &str) -> Result<Vec<f32>, Error> {
        self.encodes.fetch_add(1, Ordering::SeqCst);
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

/// A channel that answers whatever the test told it to answer.
///
/// The only question a channel is asked now is for its document vectors, so that is the only
/// thing this scripts: `Some` of a snapshot for a loaded channel, `None` for one that cannot
/// serve the hop at all.
struct ScriptedChannel {
    vectors: Option<DocumentVectorSnapshot>,
    reads: Arc<AtomicUsize>,
}

impl ScriptedChannel {
    fn unavailable() -> Arc<Self> {
        Arc::new(Self {
            vectors: None,
            reads: Arc::new(AtomicUsize::new(0)),
        })
    }

    fn loaded(vectors: Vec<(RevisionId, Vec<f32>)>) -> Self {
        Self {
            vectors: Some(Arc::new(vectors)),
            reads: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl SemanticChannel for ScriptedChannel {
    fn document_vectors(&self) -> Option<DocumentVectorSnapshot> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.vectors.clone()
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
        self.accept_draft(space_id, draft(statement))
    }

    fn accept_draft(
        &self,
        space_id: SpaceId,
        revision_draft: ContextRevisionDraft,
    ) -> (ContextId, RevisionId) {
        let added = Event::context_revision_added(space_id, revision_draft, None).unwrap();
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

    // A SEMANTIC_CORPUS_VERSION bump is the signal that the *corpus text rule* changed shape, and
    // it has to invalidate on its own: the 2026-09-11 join fix changed what every cached vector
    // means while leaving SEARCH_RANKING_VERSION -- and therefore BM25 -- byte-identical.
    assert_eq!(
        original.ranking_version,
        format!("6+corpus{SEMANTIC_CORPUS_VERSION}"),
        "the key must fold the corpus version in, or a corpus change cannot invalidate anything"
    );
    assert_ne!(
        original.ranking_version, "6",
        "the ranking version alone must not be the cache generation"
    );

    // Pruning against the new key must reclaim the superseded generation, not the current one.
    cache.store(&other_model, revision, &vector).unwrap();
    let pruned = cache.prune_superseded(&other_model).unwrap();
    assert_eq!(pruned, 1, "exactly the superseded generation is dropped");
    assert!(cache.load(&original).unwrap().is_empty());
    assert_eq!(cache.load(&other_model).unwrap().len(), 1);
}

#[test]
fn a_vector_whose_revision_left_the_accepted_set_is_reclaimed() {
    // `prune_superseded` only ever looks at the model and corpus generation, so a revision that is
    // superseded or un-accepted keeps its vector under an unchanged key forever. Measured at 5 of
    // 28 rows on one real installation. The cost is recall, not correctness: the hit is discarded
    // by the safety predicate afterwards, but only after it has taken a channel slot.
    let temporary = tempfile::tempdir().unwrap();
    let cache = SemanticVectorCache::open(&temporary.path().join("state/semantic.sqlite")).unwrap();
    let provider = HashProvider::new();
    let key = SemanticCacheKey::new("fingerprint", "6");
    let live = RevisionId::new();
    let retired = RevisionId::new();
    let vector = provider.encode("a decision worth caching").unwrap();
    cache.store(&key, live, &vector).unwrap();
    cache.store(&key, retired, &vector).unwrap();

    let other_generation = SemanticCacheKey::new("fingerprint", "5");
    cache.store(&other_generation, live, &vector).unwrap();

    let keep = std::collections::BTreeSet::from([live]);
    assert_eq!(cache.retain_revisions(&key, &keep).unwrap(), 1);
    assert_eq!(
        cache.cached_revisions(&key).unwrap(),
        keep,
        "exactly the revisions still embeddable survive"
    );
    assert_eq!(
        cache.retain_revisions(&key, &keep).unwrap(),
        0,
        "a second pass over an already-clean generation removes nothing"
    );
    assert_eq!(
        cache.load(&other_generation).unwrap().len(),
        1,
        "reclaiming dead revisions is scoped to one generation; prune_superseded owns the rest"
    );
}

#[test]
fn the_embedding_corpus_is_the_revision_text_and_never_the_normalized_index_text() {
    // The defect this pins: `embeddable_revisions` read `statement`/`rationale`/`problem_view` out
    // of `context_fts`, where every column has been through `normalize_search_text`. For CJK that
    // is overlapping bigrams -- "在包 包含 含真 真实" -- so the entire semantic corpus was tokenizer
    // shrapnel while `semantic_query_text` encoded ordinary prose on the query side. Cosine between
    // two different text spaces is not a weak signal, it is a different question.
    let fixture = Fixture::new();
    let statement = "检索准入必须先判定语料是否与查询处于同一文本空间";
    let rationale = "否则余弦相似度比较的是两套不同的文本,排序结果无从解释".to_owned();
    let problem_view = "召回通道对中文语料给出的分数长期低于英文语料".to_owned();
    let mut revision_draft = draft(statement);
    revision_draft.rationale = rationale.clone();
    revision_draft.problem_view = Some(problem_view.clone());
    let (_, revision_id) = fixture.accept_draft(fixture.lexical_space, revision_draft);

    let embeddable = fixture.engine().embeddable_revisions().unwrap();
    let text = embeddable
        .iter()
        .find(|(candidate, _)| *candidate == revision_id)
        .map(|(_, text)| text.clone())
        .expect("the accepted revision is offered to the backfill");

    assert_eq!(
        text,
        format!("{statement}\n{rationale}\n{problem_view}"),
        "the corpus text is the three revision fields as written, joined with a newline"
    );
    // The other direction, so the test fails if the fields are ever routed back through the
    // tokenizer rather than merely joined differently.
    assert_ne!(
        text,
        sctx_index::normalize_search_text(&text),
        "this fixture must be text the normalizer actually rewrites, or it proves nothing"
    );
    assert!(
        text.contains(statement),
        "the statement survives as one contiguous string, not as bigram fragments"
    );
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
// The second hop's admission record (ADR-0007's pre-registered recalibration)
// ---------------------------------------------------------------------------------------------

/// One decision, spelled the way the second hop produces it.
fn hop2_sample(seed: RevisionId, candidate: RevisionId, score: u16) -> Hop2AdmissionSample {
    Hop2AdmissionSample {
        seed_revision_id: seed,
        candidate_revision_id: candidate,
        score_basis_points: score,
        admitted: score >= SEMANTIC_HOP2_ADMISSION_FLOOR_BASIS_POINTS,
    }
}

#[test]
fn second_hop_decisions_land_where_the_recalibration_can_read_them() {
    let directory = TempDir::new().unwrap();
    let cache = SemanticVectorCache::open(&directory.path().join("semantic.sqlite")).unwrap();

    assert!(
        cache.recent_hop2_admissions(8).unwrap().is_empty(),
        "an installation whose second hop has never run reports nothing rather than a zero"
    );

    let seed = RevisionId::new();
    let admitted = RevisionId::new();
    let refused = RevisionId::new();
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    cache.record_hop2_admissions(&[
        hop2_sample(seed, admitted, 7_412),
        hop2_sample(seed, refused, 2_563),
    ]);

    let recorded = cache.recent_hop2_admissions(8).unwrap();
    assert_eq!(
        recorded
            .iter()
            .map(|row| (
                row.sample.seed_revision_id,
                row.sample.candidate_revision_id,
                row.sample.score_basis_points,
                row.sample.admitted
            ))
            .collect::<Vec<_>>(),
        [(seed, refused, 2_563, false), (seed, admitted, 7_412, true),],
        "newest first, and the refusal is kept: the floor is derived from the highest scoring \
         negative, so a record of admissions alone could never re-derive it"
    );
    assert!(
        recorded
            .iter()
            .all(|row| row.recorded_at_unix_seconds >= i64::try_from(before).unwrap()),
        "the writer stamps the clock, so one history cannot be ordered two ways"
    );
}

#[test]
fn the_admission_record_is_a_window_and_never_a_storage_decision() {
    let directory = TempDir::new().unwrap();
    let cache = SemanticVectorCache::open(&directory.path().join("semantic.sqlite")).unwrap();
    let seed = RevisionId::new();

    let overflowing = (0..SEMANTIC_HOP2_SAMPLE_HISTORY + 64)
        .map(|index| {
            hop2_sample(
                seed,
                RevisionId::new(),
                u16::try_from(index % 10_000).unwrap(),
            )
        })
        .collect::<Vec<_>>();
    cache.record_hop2_admissions(&overflowing);

    assert_eq!(
        cache
            .recent_hop2_admissions(SEMANTIC_HOP2_SAMPLE_HISTORY * 2)
            .unwrap()
            .len(),
        SEMANTIC_HOP2_SAMPLE_HISTORY,
        "the record trims to its window, the way the encode history does"
    );
    let newest = cache.recent_hop2_admissions(1).unwrap();
    assert_eq!(
        newest[0].sample.candidate_revision_id,
        overflowing.last().unwrap().candidate_revision_id,
        "the trim drops the oldest decisions, not the newest"
    );
}

#[test]
fn a_record_that_cannot_be_written_costs_the_recalibration_and_never_the_retrieval() {
    // The cache is discardable by design and an operator may delete or truncate it at any moment,
    // and an older build's file has no such table at all. Every one of those has to read as a
    // dropped batch: a Pack that failed to assemble because a diagnostic could not be written
    // would be a far worse trade than a hole in a distribution.
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("semantic.sqlite");
    let cache = SemanticVectorCache::open(&path).unwrap();
    let key = SemanticCacheKey::new("fingerprint", "6");
    let revision = RevisionId::new();
    cache
        .store(&key, revision, &[0.5_f32, 0.5, 0.5, 0.5])
        .unwrap();

    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch("DROP TABLE hop2_admission_sample")
        .unwrap();

    cache.record_hop2_admissions(&[hop2_sample(revision, RevisionId::new(), 5_400)]);

    assert_eq!(
        cache.load(&key).unwrap().len(),
        1,
        "the vectors retrieval actually depends on are untouched by a failed diagnostic write"
    );
    assert!(
        cache.recent_hop2_admissions(8).is_err(),
        "reading a table that is gone is an error the caller may handle; writing to it is not"
    );
}

/// An unfilled handle cannot serve the second hop, and can the moment it is published.
///
/// The 9--12 second model load must never be something a retrieval waits for, and the difference
/// between "no vectors yet" and "a corpus the backfill has not reached" is what decides whether
/// the Pack owes an omission line.
#[test]
fn an_unfilled_handle_serves_no_vectors_and_does_the_moment_it_is_published() {
    let handle = SemanticChannelHandle::new();
    assert!(!handle.is_ready());
    assert!(
        handle.document_vectors().is_none(),
        "an unloaded channel has nothing for the hop to compare against"
    );

    let provider = HashProvider::new();
    let revision = RevisionId::new();
    let vector = provider
        .encode("a decision about retry deduplication windows")
        .unwrap();
    handle.publish(Arc::new(EmbeddingSemanticChannel::new(vec![(
        revision,
        vector.clone(),
    )])));
    assert!(handle.is_ready());
    let published = handle
        .document_vectors()
        .expect("a published handle serves its snapshot");
    assert_eq!(published.as_slice(), &[(revision, vector)]);
}

/// The handle is also the queue of revisions still owed a vector, and it blocks the filler.
///
/// The corpus is filled once per process at model-load time, so the knowledge a session writes
/// arrives after the only pass that would have embedded it. The handle carries the ask because it
/// is the one object the loader thread and the request path already share; it must park rather
/// than spin, and a request that arrives while a fill is running must wake the next pass instead
/// of being swallowed by the current one.
#[test]
fn the_handle_carries_what_the_corpus_still_owes_and_parks_until_something_owes_it() {
    let handle = SemanticChannelHandle::new();
    assert!(handle.pending_backfill().is_empty());

    let first = RevisionId::new();
    let second = RevisionId::new();
    handle.request_backfill([first]);
    handle.request_backfill([first, second]);
    assert_eq!(
        handle.pending_backfill(),
        std::collections::BTreeSet::from([first, second]),
        "asking twice for the same revision owes it once"
    );
    assert_eq!(
        handle.take_backfill_requests(),
        std::collections::BTreeSet::from([first, second])
    );
    assert!(handle.pending_backfill().is_empty());

    // A filler with nothing to do waits, and is woken by the ask rather than by a poll.
    let filler = handle.clone();
    let waiting = std::thread::spawn(move || filler.take_backfill_requests());
    let third = RevisionId::new();
    // The wait is a condvar, so the request may land before or after the thread parks; both must
    // deliver it.
    loop {
        handle.request_backfill([third]);
        if waiting.is_finished() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(
        waiting.join().unwrap(),
        std::collections::BTreeSet::from([third])
    );
}

/// Reading the corpus for the hop costs no model call at all.
///
/// Both sides of every comparison ADR-0007 makes are vectors the backfill already wrote, so the
/// retrieval path cannot acquire an encode -- which is the property that makes it safe to hand a
/// half-loaded channel to every request.
#[test]
fn serving_the_hop_costs_no_model_call() {
    let provider = HashProvider::new();
    let corpus = (0..8)
        .map(|index| {
            (
                RevisionId::new(),
                provider.encode(&format!("corpus entry {index}")).unwrap(),
            )
        })
        .collect::<Vec<_>>();
    let baseline = provider.encode_count();
    let channel = EmbeddingSemanticChannel::new(corpus);

    let vectors = channel
        .document_vectors()
        .expect("a channel over a corpus serves it");
    assert_eq!(vectors.len(), 8);
    assert_eq!(channel.corpus_size(), 8);
    assert_eq!(
        provider.encode_count(),
        baseline,
        "the hop reads the cache; nothing on this path encodes anything"
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
    // What the Pack contains is Lane A's answer and only Lane A's answer, which on this fixture --
    // Contexts with no Engineering Reference and a Session that touched no file -- is nothing.
    // ADR-0007 makes that the correct answer rather than a degradation, and the point of this test
    // is that attaching a channel does not change it.
    assert!(pack.items.is_empty() && pack.compact_items.is_empty());

    // The one and only difference an attached-but-unavailable channel may make is the omission.
    let degraded = engine
        .clone()
        .with_semantic_channel(ScriptedChannel::unavailable());
    let degraded_pack = automatic_pack_for(&degraded, task_id);
    assert_eq!(
        omission_reasons(&degraded_pack),
        vec![
            "no_lane_evidence".to_owned(),
            "embedding_unavailable".to_owned()
        ],
        "a configured channel that could not run says so, once, beside the line that says the \
         lanes found no route -- two different facts, and a reader needs both"
    );
    assert_eq!(
        strip_omissions(&pack),
        strip_omissions(&degraded_pack),
        "an unavailable channel changes nothing but the omission it reports"
    );
    assert_eq!(
        omission_reasons(&pack),
        vec!["no_lane_evidence".to_owned()],
        "an installation with no channel has no channel to report on, and still owes the reader \
         the reason its Pack is empty"
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

/// The three degradations ADR-0004 named collapse to one, because two of them were about encoding.
///
/// A model that will not load, a model that cannot encode this query, and an encode that overran
/// its budget were three ways for the query-side channel to fail. Nothing encodes anything on a
/// retrieval path any more: ADR-0007's second hop compares two vectors the corpus backfill already
/// wrote, and the encode budget and the query cache are gone with the path that had them. The only
/// degradation left is a channel with no published corpus snapshot to compare against -- which is
/// what a model still loading is.
#[test]
fn the_only_degradation_left_is_a_channel_with_no_published_corpus() {
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
        vec![
            "no_lane_evidence".to_owned(),
            "embedding_unavailable".to_owned()
        ],
        "a handle nobody has published has no corpus to compare against, which is the one \
         degradation the second hop can still suffer"
    );

    // 2. A published corpus of vectors nothing in this projection refers to. The channel is not
    //    degraded -- it answered, with vectors -- and there is no encoder left in it to fail.
    let published = EmbeddingSemanticChannel::new(vec![(RevisionId::new(), vec![0.5_f32; 64])]);
    let loaded = engine.clone().with_semantic_channel(Arc::new(published));
    assert_eq!(
        omission_reasons(&automatic_pack(&loaded)),
        vec!["no_lane_evidence".to_owned()],
        "a channel that served its snapshot is not a degraded second hop, whatever the snapshot \
         turns out to hold"
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

    let provider = HashProvider::new();
    let channel = Arc::new(ScriptedChannel::loaded(vec![(
        silent_revision,
        provider.encode(SILENT_TOPIC).unwrap(),
    )]));
    let reads = Arc::clone(&channel.reads);
    let semantic = engine.clone().with_semantic_channel(channel);

    let mut request =
        TaskContextRequest::automatic(TaskId::new(), working_intent(), Vec::new(), 8_000);
    request.mode = ContextPackMode::Explicit;
    let explicit = semantic.task_context_pack(&request).unwrap();

    assert_eq!(
        reads.load(Ordering::SeqCst),
        0,
        "ADR-0004 scopes the channel to automatic injection; an explicit, paged query must not \
         read a corpus snapshot the backfill is still filling"
    );
    let serialized = serde_json::to_string(&explicit).unwrap();
    assert!(!serialized.contains("semantic_similarity"));
    assert!(!serialized.contains("embedding_unavailable"));
}
