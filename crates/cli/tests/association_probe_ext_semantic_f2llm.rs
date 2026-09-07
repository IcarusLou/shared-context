//! ADR-0004 acceptance for the default export: the extended probe set fused with F2LLM-v2-0.6B.
//!
//! `association_probe_ext_semantic` is the same acceptance over bge-m3, and it stays that way:
//! `SEMANTIC_SIMILARITY_FLOOR_BASIS_POINTS` is a statement about bge-m3's score distribution on
//! this fixture and cannot be pointed at another export. This file is the recalibrated twin, over
//! [`QWEN3_SEMANTIC_SIMILARITY_FLOOR_BASIS_POINTS`] and the model `sctx embedding install` now
//! installs by default. Everything except the model, the floor and the ratchets is shared through
//! `association_probe_harness::semantic`, so the per-category numbers below are comparable to the
//! bge-m3 run rather than to a second runner's idea of the same measurement.
//!
//! `#[ignore]`d because it needs roughly 2.4 GB of weights this repository deliberately does not
//! ship. Point it at an unmodified Hugging Face snapshot and the ONNX Runtime library:
//!
//! ```text
//! SCTX_PROBE_F2LLM_MODEL=~/.cache/huggingface/hub/models--codefuse-ai--F2LLM-v2-0.6B/snapshots/<sha> \
//! SCTX_PROBE_EMBEDDING_RUNTIME=~/.shared-context/embedding/runtime/libonnxruntime.dylib \
//!   cargo test --release --locked -p sctx-cli --test association_probe_ext_semantic_f2llm -- \
//!     --ignored --nocapture
//! ```
//!
//! ## What is ratcheted, and against what
//!
//! ADR-0004's acceptance is a set of *relative* clauses -- `paraphrase` and `cross_lingual` must
//! gain, `identifier` and `noise` must not regress -- and every one of them is asserted here
//! against this run's own lexical control, exactly as the bge-m3 suite asserts them against its.
//! On top of that, the two categories the channel exists for are held to the numbers the *bge-m3*
//! arm reaches, because a default model swap that quietly costs cross-lingual recall is the failure
//! this file is here to catch and no self-referential comparison would see it.
//!
//! The total is pinned too. It is a ratchet, not a target: it is what this fixture measured on the
//! day the floor was calibrated, and its job is to make a silent regression noisy. Moving it down
//! deliberately means saying why in the same commit.

#![cfg(unix)]

mod association_probe_harness;

// The snapshot flattening is one contract with the Hub's layout, and `crates/search` already states
// it for the encoder contract suite. A second copy here would be a second thing to keep true: the
// day the export grows a file, one of the two copies gets it. Reaching across the workspace for a
// test-only module is unusual enough to be worth the sentence, and cheaper than the drift.
#[path = "../../search/tests/f2llm_snapshot/mod.rs"]
mod f2llm_snapshot;

use std::{sync::Arc, time::Instant};

use association_probe_harness::{
    Harness, build_harness,
    semantic::{
        Run, category_hits, embed_corpus, engine, installation_root, percentile,
        print_category_comparison, print_similarity_separation, run_suite,
    },
};
use sctx_search::{
    EmbeddingProvider, EmbeddingSemanticChannel, QWEN3_SEMANTIC_SIMILARITY_FLOOR_BASIS_POINTS,
    QueryVectorCache, SEMANTIC_QUERY_CACHE_CAPACITY, SearchEngine, SemanticChannel,
    embedding::onnx::OnnxEmbeddingProvider,
};
use serde_json::Value;

const PROBE_FIXTURE: &str = include_str!("../../../fixtures/association/probe-ext-v1.json");

/// What the blocking ext suite measures for `task_intent_update` through the real binary, and how
/// far the in-process control may sit below it. Both are the bge-m3 suite's, unchanged: they
/// describe the lexical channels, which no embedding model touches.
const LEXICAL_INTENT_HITS: usize = 27;
const LEXICAL_CONTROL_TOLERANCE: usize = 1;

/// Fused hits this fixture reaches with F2LLM at the calibrated floor.
///
/// Measured 2026-09-07 on the run recorded in
/// [`QWEN3_SEMANTIC_SIMILARITY_FLOOR_BASIS_POINTS`]'s doc comment.
const FUSED_HITS: usize = 32;

/// What the bge-m3 arm of this acceptance reaches, per category, on the same fixture through the
/// same in-process runner (measured 2026-09-07, release, `association_probe_ext_semantic`).
///
/// Only the two categories ADR-0004 requires a *gain* in are held to these. The rest are compared
/// against this run's own lexical control, because a model may legitimately trade a near-duplicate
/// tie for a paraphrase and the ADR says which of those it cares about.
const BGE_CROSS_LINGUAL_HITS: usize = 2;
const BGE_PARAPHRASE_HITS: usize = 8;

#[test]
#[ignore = "needs a real F2LLM-v2-0.6B snapshot; see the module docs"]
fn the_f2llm_channel_holds_cross_lingual_and_paraphrase_at_the_calibrated_floor() {
    let fixture = serde_json::from_str::<Value>(PROBE_FIXTURE).unwrap();
    let harness = build_harness(&fixture);
    let root = installation_root(&harness);
    let engine = engine(&root);

    // 1. The lexical baseline, through the same in-process path the fused run will use.
    let lexical = run_suite(&harness, &engine, &fixture);
    let total = fixture["probes"].as_array().unwrap().len();
    println!(
        "\n--- lexical control (in-process) --- {}/{total} (binary measures \
         {LEXICAL_INTENT_HITS}/{total})",
        lexical.hits
    );
    assert!(
        lexical.hits + LEXICAL_CONTROL_TOLERANCE >= LEXICAL_INTENT_HITS,
        "the in-process control drifted to {}/{total}, more than {LEXICAL_CONTROL_TOLERANCE} \
         probe(s) below the {LEXICAL_INTENT_HITS}/{total} the blocking suite measures; its \
         embedding numbers would not be comparable",
        lexical.hits
    );
    assert_eq!(
        lexical.noise_leaks, 0,
        "the control must reject every noise probe, or the fused run has nothing to hold to"
    );

    // 2. Load the model once and embed the whole accepted corpus. The load covers reshaping the
    //    snapshot and initialising the runtime as well as the session, which is what an operator
    //    waits for on the first `sctx mcp serve` too.
    let load_started = Instant::now();
    let provider = f2llm_snapshot::provider();
    println!(
        "model load (runtime init + session) {:?}",
        load_started.elapsed()
    );
    let provider = Arc::<OnnxEmbeddingProvider>::clone(provider) as Arc<dyn EmbeddingProvider>;
    assert_eq!(
        provider.similarity_floor_basis_points(),
        QWEN3_SEMANTIC_SIMILARITY_FLOOR_BASIS_POINTS,
        "this suite grades the Qwen3 floor; a provider reporting another one means the wrong \
         export is loaded"
    );
    // A literal fingerprint rather than `model_fingerprint` of the directory: this suite loads the
    // model out of a directory of symlinks into the Hugging Face cache, and `model_fingerprint`
    // reads `DirEntry::metadata`, which on Unix does not follow a symlink -- so it sees a directory
    // holding no files and refuses. Nothing about that is load-bearing here: the fingerprint only
    // has to be stable for the length of one run, because the cache lives in a temporary home.
    let (cache, key, embeddable) = embed_corpus(&engine, &provider, &root, "f2llm-probe-snapshot");

    let channel = Arc::new(
        EmbeddingSemanticChannel::from_cache(Arc::clone(&provider), &cache, &key).unwrap(),
    );
    assert_eq!(channel.corpus_size(), embeddable);

    let (_worst_positive, best_noise) = print_similarity_separation(&fixture, channel.as_ref());
    assert!(
        best_noise < QWEN3_SEMANTIC_SIMILARITY_FLOOR_BASIS_POINTS,
        "the floor must sit above every noise query on this set: best noise {best_noise}, floor \
         {QWEN3_SEMANTIC_SIMILARITY_FLOOR_BASIS_POINTS}"
    );

    // The separation pass just encoded every probe query, so the fused run gets a fresh query
    // cache: otherwise its latency numbers would all be cache hits.
    let channel = Arc::new(
        EmbeddingSemanticChannel::from_cache(Arc::clone(&provider), &cache, &key)
            .unwrap()
            .with_query_cache(Arc::new(QueryVectorCache::with_capacity(
                SEMANTIC_QUERY_CACHE_CAPACITY,
            ))),
    );
    let semantic_engine = engine
        .clone()
        .with_semantic_channel(Arc::clone(&channel) as Arc<dyn SemanticChannel>);

    // 3. The same 39 probes, now with the channel fused in.
    let fused = run_suite(&harness, &semantic_engine, &fixture);
    println!("--- with embedding channel --- {}/{total}", fused.hits);
    print_category_comparison(&lexical, &fused);

    // ADR-0004's acceptance, against this run's own control.
    let paraphrase_delta = i64::try_from(category_hits(&fused, "paraphrase")).unwrap()
        - i64::try_from(category_hits(&lexical, "paraphrase")).unwrap();
    let cross_lingual_delta = i64::try_from(category_hits(&fused, "cross_lingual")).unwrap()
        - i64::try_from(category_hits(&lexical, "cross_lingual")).unwrap();
    assert!(
        paraphrase_delta + cross_lingual_delta > 0,
        "ADR-0004 requires a net gain across paraphrase and cross_lingual, got \
         {paraphrase_delta:+} and {cross_lingual_delta:+}"
    );
    assert!(
        category_hits(&fused, "identifier") >= category_hits(&lexical, "identifier"),
        "identifier recall must not regress"
    );
    assert_eq!(
        fused.noise_leaks, 0,
        "the channel must not leak a single noise probe"
    );

    // And against the model it replaced as the default.
    assert!(
        category_hits(&fused, "cross_lingual") >= BGE_CROSS_LINGUAL_HITS,
        "cross_lingual fell to {} against bge-m3's {BGE_CROSS_LINGUAL_HITS} on this fixture; the \
         default export must not cost the category the channel exists for",
        category_hits(&fused, "cross_lingual")
    );
    assert!(
        category_hits(&fused, "paraphrase") >= BGE_PARAPHRASE_HITS,
        "paraphrase fell to {} against bge-m3's {BGE_PARAPHRASE_HITS} on this fixture",
        category_hits(&fused, "paraphrase")
    );
    assert!(
        fused.hits >= FUSED_HITS,
        "fused recall fell to {}/{total} against the {FUSED_HITS}/{total} this fixture measured \
         at the calibrated floor",
        fused.hits
    );

    // 4. Latency, model already loaded.
    report_latency(&harness, &semantic_engine, &fixture, &lexical, &fused);
}

/// Reports what the channel costs a `long_intent` retrieval, and asserts the one part of that which
/// is a property of this code.
///
/// Nothing here is graded against the bge-m3 suite's budgets: those are that model's measured cost
/// plus headroom, and this family's own ladder is measured in
/// `crates/search/tests/embedding_encode_latency.rs`. The query vector cache is different -- it is
/// the same code whichever model is loaded, and a cache that changed what the channel retrieves
/// would be a defect rather than a slower model.
fn report_latency(
    harness: &Harness,
    semantic_engine: &SearchEngine,
    fixture: &Value,
    lexical: &Run,
    fused: &Run,
) {
    let lexical_long_p95 = percentile(lexical.long_latencies.clone(), 95);
    let first_long_p95 = percentile(fused.long_latencies.clone(), 95);
    println!(
        "\nlong_intent p95: lexical {lexical_long_p95:?}, first encode {first_long_p95:?}, \
         increment {:?}",
        first_long_p95.saturating_sub(lexical_long_p95)
    );
    let repeated = run_suite(harness, semantic_engine, fixture);
    let repeated_long_p95 = percentile(repeated.long_latencies.clone(), 95);
    println!(
        "long_intent p95 (cached encode): {repeated_long_p95:?}, increment {:?}",
        repeated_long_p95.saturating_sub(lexical_long_p95)
    );
    assert_eq!(
        repeated.hits, fused.hits,
        "the query vector cache must not change what the channel retrieves"
    );
}
