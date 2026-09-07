//! ADR-0004 acceptance: the extended probe set with a real embedding model attached.
//!
//! This test is `#[ignore]`d because it needs roughly 2.1 GB of model weights and an ONNX Runtime
//! shared library that this repository deliberately does not ship. Point it at both and run it:
//!
//! ```text
//! SCTX_PROBE_EMBEDDING_MODEL=/path/to/bge-m3-onnx \
//! SCTX_PROBE_EMBEDDING_RUNTIME=/path/to/libonnxruntime.dylib \
//!   cargo test --locked -p sctx-cli --test association_probe_ext_semantic -- --ignored --nocapture
//! ```
//!
//! The model must be bge-m3 specifically, even though `sctx embedding install` now defaults to a
//! different export. [`SEMANTIC_SIMILARITY_FLOOR_BASIS_POINTS`] was calibrated against bge-m3's
//! score distribution on this very fixture, and the noise-ceiling assertion below is a statement
//! about *that* distribution: pointed at another export this test would be comparing a floor to
//! scores it was never derived from, and would prove nothing either way. The new default has its
//! own suite -- `association_probe_ext_semantic_f2llm`, over its own floor -- and the two share
//! everything but the model through `association_probe_harness::semantic`, so their per-category
//! numbers are comparable.
//!
//! ## Why this runs in-process instead of through the probe binary
//!
//! [`association_probe_harness::run_probes`] spawns a fresh `sctx mcp serve` for every single
//! call. That is the right shape for the lexical suites and the wrong shape for this one: the
//! embedding channel loads its model on a background thread precisely so a session never waits
//! 9--12 seconds for it, and a process that answers one request and exits is killed long before
//! that thread publishes anything. Measured through the probe binary the channel would report
//! `embedding_unavailable` on all 39 probes and prove nothing.
//!
//! So the corpus is still built by the real harness -- the same Git store, the same public MCP
//! confirmation chain, the same projection -- and only the *query* half runs in-process against
//! one loaded model. Both arms of the comparison run through that same in-process path, so the
//! per-category deltas below are measured against their own control and never against a number
//! produced by a different runner.
//!
//! The in-process control lands one probe below the 27/39 the blocking suite measures through the
//! binary, and the difference is known rather than mysterious: `Runtime` attaches a
//! `RuntimeUsagePrior` read from `runtime.sqlite`, which this runner has no public way to build.
//! The harness clears `context_usage` before every probe, so the prior is near-neutral, but
//! "near-neutral" still breaks one tie differently. The test pins that gap to at most one probe:
//! if the in-process runner ever drifts further from the real one, it fails before reporting a
//! single embedding number.

mod association_probe_harness;

use std::{
    fs,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use association_probe_harness::{
    Harness, build_harness,
    semantic::{
        Run, category_hits, embed_corpus, engine, installation_root, percentile,
        print_category_comparison, print_similarity_separation, run_suite,
    },
};
use sctx_search::{
    EmbeddingProvider, EmbeddingSemanticChannel, QueryVectorCache, SEMANTIC_QUERY_CACHE_CAPACITY,
    SEMANTIC_SIMILARITY_FLOOR_BASIS_POINTS, SearchEngine, SemanticChannel,
};
use serde_json::Value;

const PROBE_FIXTURE: &str = include_str!("../../../fixtures/association/probe-ext-v1.json");

/// What the blocking ext suite measures for `task_intent_update` through the real binary.
/// Re-measured 2026-09-03 over the 39-probe fixture (`long_intent` added by T5d).
const LEXICAL_INTENT_HITS: usize = 27;
/// How far the in-process control may sit below it before the substitution stops being honest.
/// One probe, for the usage prior this runner cannot build; see the module docs.
const LEXICAL_CONTROL_TOLERANCE: usize = 1;

/// ADR-0004's latency budget, which now applies to the case it can actually hold for: a query
/// whose vector the process has already encoded once.
///
/// The original clause budgeted a 100 ms p95 increment with no qualifier, and it was measured
/// against probe queries 15--40 characters long. That is not what an automatic retrieval submits.
/// A real Working Intent flattens to a few hundred characters and its *first* encode costs
/// hundreds of milliseconds on current hardware, which is the whole reason the 200 ms encode
/// budget silently disabled the channel in every real session. See the 2026-09-03 revision in
/// `docs/adr/0004-embedding-retrieval-channel.md`.
const REPEATED_P95_INCREMENT_BUDGET: Duration = Duration::from_millis(100);

/// What one *first* encode of a real-length Working Intent may add to `task_context` p95.
///
/// Measured at 235 ms p95 for a 283-character query (`crates/search/tests/embedding_encode_latency.rs`);
/// 400 ms is that plus fusion and headroom. This number is the honest cost of the channel's first
/// look at a new Intent, and stating it is the point: the previous budget hid it by never paying
/// it at all.
const FIRST_P95_INCREMENT_BUDGET: Duration = Duration::from_millis(400);

fn model_paths() -> Option<(PathBuf, PathBuf)> {
    let model = std::env::var_os("SCTX_PROBE_EMBEDDING_MODEL")?;
    let runtime = std::env::var_os("SCTX_PROBE_EMBEDDING_RUNTIME")?;
    Some((PathBuf::from(model), PathBuf::from(runtime)))
}

#[test]
#[ignore = "requires a locally downloaded bge-m3 ONNX export and an ONNX Runtime library"]
fn the_embedding_channel_lifts_paraphrase_and_cross_lingual_without_costing_identifiers() {
    let Some((model, runtime)) = model_paths() else {
        panic!(
            "set SCTX_PROBE_EMBEDDING_MODEL and SCTX_PROBE_EMBEDDING_RUNTIME; see the module docs"
        );
    };
    let fixture = serde_json::from_str::<Value>(PROBE_FIXTURE).unwrap();
    let harness = build_harness(&fixture);
    let root = installation_root(&harness);
    let engine = engine(&root);

    // 1. The lexical baseline, through the same in-process path the embedding run will use. This
    //    is the control that makes every number below comparable to the blocking suite.
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

    // 2. Load the model once and embed the whole accepted corpus.
    let load_started = Instant::now();
    let provider = sctx_search::load_onnx_provider(&model, &runtime)
        .expect("the configured model and runtime must load");
    println!("model load {:?}", load_started.elapsed());
    let provider = provider as Arc<dyn EmbeddingProvider>;
    let fingerprint = sctx_search::model_fingerprint(&model).unwrap();
    let (cache, key, embeddable) = embed_corpus(&engine, &provider, &root, &fingerprint);

    let channel = Arc::new(
        EmbeddingSemanticChannel::from_cache(Arc::clone(&provider), &cache, &key).unwrap(),
    );
    assert_eq!(channel.corpus_size(), embeddable);

    let (_worst_positive, best_noise) = print_similarity_separation(&fixture, channel.as_ref());
    assert!(
        best_noise < SEMANTIC_SIMILARITY_FLOOR_BASIS_POINTS,
        "the floor must sit above every noise query on this set: best noise {best_noise}, floor \
         {SEMANTIC_SIMILARITY_FLOOR_BASIS_POINTS}"
    );

    // `print_similarity_separation` has now encoded every probe query once, so the shared query
    // cache holds them all. Handing the fused channel an empty cache of its own is what makes the
    // "first encode" measurement below actually a first encode -- otherwise the run would report
    // the cached cost for every probe and quietly lose the number this test exists to pin.
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

    // ADR-0004's acceptance, verbatim.
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

    // 4. Latency, with the model already loaded.
    assert_latency_budgets(
        &harness,
        &semantic_engine,
        &fixture,
        &lexical,
        &fused,
        total,
    );
}

/// Grades the two latency clauses ADR-0004 was split into on 2026-09-03.
///
/// Both are measured on the `long_intent` probes alone. The whole-suite p95 is printed for
/// continuity with the T5b baseline and asserted on nothing: it is dominated by 15--40 character
/// probes, and taking an acceptance number from it is precisely the mistake that let a 200 ms
/// encode budget ship while making the channel unusable in every real session.
fn assert_latency_budgets(
    harness: &Harness,
    semantic_engine: &SearchEngine,
    fixture: &Value,
    lexical: &Run,
    fused: &Run,
    total: usize,
) {
    let lexical_p95 = percentile(lexical.latencies.clone(), 95);
    let fused_p95 = percentile(fused.latencies.clone(), 95);
    println!(
        "\ntask_context p95 over all {total} probes: lexical {lexical_p95:?}, fused {fused_p95:?} \
         (short-probe dominated, not an acceptance number)"
    );

    let lexical_long_p95 = percentile(lexical.long_latencies.clone(), 95);
    let first_long_p95 = percentile(fused.long_latencies.clone(), 95);
    let first_increment = first_long_p95.saturating_sub(lexical_long_p95);
    println!(
        "long_intent p95 (first encode): lexical {lexical_long_p95:?}, fused {first_long_p95:?}, \
         increment {first_increment:?}"
    );
    assert!(
        first_increment <= FIRST_P95_INCREMENT_BUDGET,
        "a first encode of a real-length Working Intent may add {FIRST_P95_INCREMENT_BUDGET:?} to \
         task_context p95, measured {first_increment:?}"
    );

    // The same long queries again. Their vectors are in the query cache now, so this is what every
    // repeat read of one Intent costs -- and it is the case ADR-0004's original 100 ms clause
    // survives on.
    let repeated = run_suite(harness, semantic_engine, fixture);
    let repeated_long_p95 = percentile(repeated.long_latencies.clone(), 95);
    let repeated_increment = repeated_long_p95.saturating_sub(lexical_long_p95);
    println!(
        "long_intent p95 (cached encode): fused {repeated_long_p95:?}, increment \
         {repeated_increment:?}"
    );
    assert!(
        repeated_increment <= REPEATED_P95_INCREMENT_BUDGET,
        "a repeated retrieval over one Working Intent must stay inside ADR-0004's \
         {REPEATED_P95_INCREMENT_BUDGET:?} p95 increment, measured {repeated_increment:?}"
    );
    assert_eq!(
        repeated.hits, fused.hits,
        "the query vector cache must not change what the channel retrieves"
    );
}

/// Keeps the fixture path honest even when the ignored test never runs.
#[test]
fn the_extended_probe_fixture_is_readable() {
    let fixture = serde_json::from_str::<Value>(PROBE_FIXTURE).unwrap();
    assert_eq!(fixture["probes"].as_array().unwrap().len(), 39);
    let _ = fs::metadata("../../fixtures/association/probe-ext-v1.json");
}
