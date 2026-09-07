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
//! different export. `SEMANTIC_SIMILARITY_FLOOR_BASIS_POINTS` was calibrated against bge-m3's
//! score distribution on this very fixture, and the noise-ceiling assertion below is a statement
//! about *that* distribution: pointed at another export this test would be comparing a floor to
//! scores it was never derived from, and would prove nothing either way. Recalibrating the floor
//! for the new default is its own errand, and it starts by rerunning this file's measurements.
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
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use association_probe_harness::{Harness, build_harness, reset_context_usage, top_index};
use sctx_domain::{TaskId, WorkingIntentSnapshot};
use sctx_engineering_graph::EngineeringProjectionStore;
use sctx_index::{ProjectionIndex, SEARCH_RANKING_VERSION};
use sctx_search::{
    ContextPackMode, EmbeddingSemanticChannel, QueryVectorCache, SEMANTIC_QUERY_CACHE_CAPACITY,
    SEMANTIC_SIMILARITY_FLOOR_BASIS_POINTS, SearchEngine, SemanticCacheKey, SemanticChannel,
    SemanticOutcome, SemanticVectorCache, TaskContextRequest, model_fingerprint,
};
use serde_json::Value;

const PROBE_FIXTURE: &str = include_str!("../../../fixtures/association/probe-ext-v1.json");

/// The `task_intent_update` defaults the real automatic path uses (`crates/mcp/src/lib.rs`).
const AUTOMATIC_TOKEN_BUDGET: usize = 8_000;
const AUTOMATIC_MAX_SPACES: usize = 8;

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

/// The probe category whose queries are the length a real automatic retrieval submits.
const LONG_INTENT_CATEGORY: &str = "long_intent";

fn model_paths() -> Option<(PathBuf, PathBuf)> {
    let model = std::env::var_os("SCTX_PROBE_EMBEDDING_MODEL")?;
    let runtime = std::env::var_os("SCTX_PROBE_EMBEDDING_RUNTIME")?;
    Some((PathBuf::from(model), PathBuf::from(runtime)))
}

fn installation_root(harness: &Harness) -> PathBuf {
    harness.home.join(".shared-context")
}

fn engine(root: &Path) -> SearchEngine {
    let index = ProjectionIndex::new(root.join("repository"), root.join("state"));
    index.synchronize().unwrap();
    match EngineeringProjectionStore::initialize(root) {
        Ok(graph) => SearchEngine::with_engineering_graph(index, graph),
        Err(_) => SearchEngine::new(index),
    }
}

/// One automatic retrieval, shaped exactly like the one `task_intent_update` performs.
fn automatic_top1(engine: &SearchEngine, query: &str) -> (Option<String>, usize, Duration) {
    let mut request = TaskContextRequest::automatic(
        TaskId::new(),
        WorkingIntentSnapshot {
            goal: query.to_owned(),
            current_direction: None,
            in_scope: Vec::new(),
            out_of_scope: Vec::new(),
            domains: Vec::new(),
            platforms: Vec::new(),
            constraints: Vec::new(),
            acceptance_conditions: Vec::new(),
            artifact_hints: Vec::new(),
            interface_hints: Vec::new(),
            open_questions: Vec::new(),
        },
        Vec::new(),
        AUTOMATIC_TOKEN_BUDGET,
    );
    request.mode = ContextPackMode::AutomaticInjection;
    request.max_spaces = AUTOMATIC_MAX_SPACES;
    let started = Instant::now();
    let pack = engine.task_context_pack(&request).unwrap();
    let elapsed = started.elapsed();
    (
        pack.items
            .first()
            .map(|item| item.context.context_id.to_string()),
        pack.items.len(),
        elapsed,
    )
}

struct Run {
    hits: usize,
    by_category: BTreeMap<String, (usize, usize)>,
    latencies: Vec<Duration>,
    /// Latencies of the `long_intent` probes alone. The suite average is dominated by 15--40
    /// character probes that no automatic retrieval ever submits, and a p95 taken over it is the
    /// measurement that let a 200 ms encode budget look adequate.
    long_latencies: Vec<Duration>,
    noise_leaks: usize,
}

fn run_suite(harness: &Harness, engine: &SearchEngine, fixture: &Value) -> Run {
    let mut run = Run {
        hits: 0,
        by_category: BTreeMap::new(),
        latencies: Vec::new(),
        long_latencies: Vec::new(),
        noise_leaks: 0,
    };
    for probe in fixture["probes"].as_array().unwrap() {
        reset_context_usage(&harness.home);
        let query = probe["query"].as_str().unwrap();
        let category = probe["category"].as_str().unwrap().to_owned();
        let expected = probe["expected"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_u64().unwrap())
            .collect::<Vec<_>>();

        let (top1, count, elapsed) = automatic_top1(engine, query);
        run.latencies.push(elapsed);
        if category == LONG_INTENT_CATEGORY {
            run.long_latencies.push(elapsed);
        }
        let index = top_index(harness, top1.as_deref());
        let hit = if expected.is_empty() {
            if count > 0 {
                run.noise_leaks += 1;
            }
            count == 0
        } else {
            index.is_some_and(|index| expected.contains(&index))
        };
        run.hits += usize::from(hit);
        let entry = run.by_category.entry(category).or_default();
        entry.0 += 1;
        entry.1 += usize::from(hit);
    }
    run
}

fn percentile(mut samples: Vec<Duration>, percent: usize) -> Duration {
    assert!(!samples.is_empty());
    samples.sort_unstable();
    let index = (samples.len() * percent).div_ceil(100).saturating_sub(1);
    samples[index.min(samples.len() - 1)]
}

/// Prints the raw separation the similarity floor has to cut, and the two numbers that decide it.
///
/// ADR-0004 set a provisional 0.50 from a prototype on the two older fixtures and deferred the
/// final value to this set. Printing the distribution means the next person to question the floor
/// re-reads a table instead of re-deriving one.
fn print_similarity_separation(fixture: &Value, channel: &EmbeddingSemanticChannel) {
    println!("\n--- top similarity per probe (basis points) ---");
    let mut worst_positive = u16::MAX;
    let mut best_noise = 0_u16;
    for probe in fixture["probes"].as_array().unwrap() {
        let query = probe["query"].as_str().unwrap();
        let is_noise = probe["expected"].as_array().unwrap().is_empty();
        let top = match channel.similar_revisions(query) {
            SemanticOutcome::Hits(hits) => {
                hits.first().map_or(0, |hit| hit.similarity_basis_points)
            }
            SemanticOutcome::Unavailable => 0,
        };
        if is_noise {
            best_noise = best_noise.max(top);
        } else if top > 0 {
            worst_positive = worst_positive.min(top);
        }
        println!(
            "{:<10} {:<16} {top:>6}",
            probe["id"].as_str().unwrap(),
            probe["category"].as_str().unwrap()
        );
    }
    println!("worst scoring positive {worst_positive}, best scoring noise {best_noise}");
    assert!(
        best_noise < SEMANTIC_SIMILARITY_FLOOR_BASIS_POINTS,
        "the floor must sit above every noise query on this set: best noise {best_noise}, floor \
         {SEMANTIC_SIMILARITY_FLOOR_BASIS_POINTS}"
    );
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
    let load_elapsed = load_started.elapsed();
    let key = SemanticCacheKey::new(model_fingerprint(&model).unwrap(), SEARCH_RANKING_VERSION);
    let cache = SemanticVectorCache::open_at_root(&root).unwrap();
    let embeddable = engine.embeddable_revisions().unwrap();
    let backfill_started = Instant::now();
    for (revision_id, text) in &embeddable {
        let vector = provider.encode(text).expect("every corpus text encodes");
        cache.store(&key, *revision_id, &vector).unwrap();
    }
    let backfill_elapsed = backfill_started.elapsed();
    println!(
        "model load {load_elapsed:?}, embedded {} revision(s) in {backfill_elapsed:?}",
        embeddable.len()
    );

    let channel = Arc::new(
        EmbeddingSemanticChannel::from_cache(Arc::clone(&provider), &cache, &key).unwrap(),
    );
    assert_eq!(channel.corpus_size(), embeddable.len());

    print_similarity_separation(&fixture, channel.as_ref());

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

    println!("\n--- ADR-0004 per-category comparison (task_intent_update) ---");
    println!(
        "{:<16} {:>6} {:>10} {:>10} {:>8}",
        "category", "count", "lexical", "fused", "delta"
    );
    for (category, (count, lexical_hits)) in &lexical.by_category {
        let fused_hits = fused.by_category.get(category).map_or(0, |entry| entry.1);
        let delta = i64::try_from(fused_hits).unwrap() - i64::try_from(*lexical_hits).unwrap();
        println!("{category:<16} {count:>6} {lexical_hits:>10} {fused_hits:>10} {delta:>+8}");
    }

    let category_hits =
        |run: &Run, category: &str| run.by_category.get(category).map_or(0, |entry| entry.1);

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
