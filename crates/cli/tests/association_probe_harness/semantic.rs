//! The half of the embedding acceptance runs that is the same for every model family.
//!
//! Two suites use it: `association_probe_ext_semantic` (bge-m3, whose floor the extended fixture
//! calibrated) and `association_probe_ext_semantic_f2llm` (the F2LLM default). What they share is
//! everything except the model: the same corpus, the same lexical control, the same in-process
//! retrieval path, the same percentile arithmetic. Keeping that in one place is what makes the two
//! runs comparable at all -- a per-category delta measured by one runner against a number produced
//! by a slightly different one would be a comparison of runners, which is the mistake the module
//! docs of the bge-m3 suite spend three paragraphs guarding against.
//!
//! What stays in each suite is what genuinely differs: how the model is loaded, which floor its
//! space is calibrated to, and which numbers are ratcheted.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use sctx_domain::{TaskId, WorkingIntentSnapshot};
use sctx_engineering_graph::EngineeringProjectionStore;
use sctx_index::{ProjectionIndex, SEARCH_RANKING_VERSION};
use sctx_search::{
    ContextPackMode, EmbeddingProvider, EmbeddingSemanticChannel, SearchEngine, SemanticCacheKey,
    SemanticChannel as _, SemanticOutcome, SemanticVectorCache, TaskContextRequest,
};
use serde_json::Value;

use super::{Harness, reset_context_usage, top_index};

/// The `task_intent_update` defaults the real automatic path uses (`crates/mcp/src/lib.rs`).
pub const AUTOMATIC_TOKEN_BUDGET: usize = 8_000;
pub const AUTOMATIC_MAX_SPACES: usize = 8;

/// The probe category whose queries are the length a real automatic retrieval submits.
pub const LONG_INTENT_CATEGORY: &str = "long_intent";

pub fn installation_root(harness: &Harness) -> PathBuf {
    harness.home.join(".shared-context")
}

pub fn engine(root: &Path) -> SearchEngine {
    let index = ProjectionIndex::new(root.join("repository"), root.join("state"));
    index.synchronize().unwrap();
    match EngineeringProjectionStore::initialize(root) {
        Ok(graph) => SearchEngine::with_engineering_graph(index, graph),
        Err(_) => SearchEngine::new(index),
    }
}

/// One automatic retrieval, shaped exactly like the one `task_intent_update` performs.
pub fn automatic_top1(engine: &SearchEngine, query: &str) -> (Option<String>, usize, Duration) {
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

pub struct Run {
    pub hits: usize,
    pub by_category: BTreeMap<String, (usize, usize)>,
    pub latencies: Vec<Duration>,
    /// Latencies of the `long_intent` probes alone. The suite average is dominated by 15--40
    /// character probes that no automatic retrieval ever submits, and a p95 taken over it is the
    /// measurement that let a 200 ms encode budget look adequate.
    pub long_latencies: Vec<Duration>,
    pub noise_leaks: usize,
}

pub fn run_suite(harness: &Harness, engine: &SearchEngine, fixture: &Value) -> Run {
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

pub fn percentile(mut samples: Vec<Duration>, percent: usize) -> Duration {
    assert!(!samples.is_empty());
    samples.sort_unstable();
    let index = (samples.len() * percent).div_ceil(100).saturating_sub(1);
    samples[index.min(samples.len() - 1)]
}

pub fn category_hits(run: &Run, category: &str) -> usize {
    run.by_category.get(category).map_or(0, |entry| entry.1)
}

/// Prints the per-category lexical/fused comparison ADR-0004's acceptance is read off.
pub fn print_category_comparison(lexical: &Run, fused: &Run) {
    println!("\n--- ADR-0004 per-category comparison (task_intent_update) ---");
    println!(
        "{:<16} {:>6} {:>10} {:>10} {:>8}",
        "category", "count", "lexical", "fused", "delta"
    );
    for (category, (count, lexical_hits)) in &lexical.by_category {
        let fused_hits = category_hits(fused, category);
        let delta = i64::try_from(fused_hits).unwrap() - i64::try_from(*lexical_hits).unwrap();
        println!("{category:<16} {count:>6} {lexical_hits:>10} {fused_hits:>10} {delta:>+8}");
    }
}

/// Embeds every accepted revision through the provider's corpus path and returns the filled cache.
///
/// `fingerprint` is what keys the cache -- two model directories must never share cached vectors --
/// and a caller with a real installation directory passes `sctx_search::model_fingerprint` of it,
/// exactly as the serve path does.
pub fn embed_corpus(
    engine: &SearchEngine,
    provider: &Arc<dyn EmbeddingProvider>,
    root: &Path,
    fingerprint: &str,
) -> (SemanticVectorCache, SemanticCacheKey, usize) {
    let key = SemanticCacheKey::new(fingerprint, SEARCH_RANKING_VERSION);
    let cache = SemanticVectorCache::open_at_root(root).unwrap();
    let embeddable = engine.embeddable_revisions().unwrap();
    let started = Instant::now();
    for (revision_id, text) in &embeddable {
        let vector = provider.encode(text).expect("every corpus text encodes");
        cache.store(&key, *revision_id, &vector).unwrap();
    }
    println!(
        "embedded {} revision(s) in {:?}",
        embeddable.len(),
        started.elapsed()
    );
    (cache, key, embeddable.len())
}

/// Prints the raw separation the similarity floor has to cut, and returns the two numbers that
/// decide it: the lowest-scoring positive and the highest-scoring noise query.
///
/// ADR-0004 set a provisional 0.50 from a prototype on the two older fixtures and deferred the
/// final value to this set. Printing the distribution means the next person to question a floor
/// re-reads a table instead of re-deriving one.
pub fn print_similarity_separation(
    fixture: &Value,
    channel: &EmbeddingSemanticChannel,
) -> (u16, u16) {
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
    (worst_positive, best_noise)
}
