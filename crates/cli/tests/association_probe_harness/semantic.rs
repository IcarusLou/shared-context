//! The half of the embedding acceptance runs that is the same for every model family.
//!
//! Two suites use it: `association_probe_ext_semantic` (an export named by the operator, the arm a
//! model swap is compared on) and `association_probe_ext_semantic_f2llm` (the export `sctx
//! embedding install` actually installs, the arm that carries the ratchet). What they share is
//! everything except the model: the same corpus, built by the same public confirmation chain, the
//! same document-role encode, the same separation arithmetic. Keeping that in one place is what
//! makes the two runs comparable at all -- a per-category number measured by one runner against a
//! number produced by a slightly different one would be a comparison of runners.
//!
//! What stays in each suite is what genuinely differs: how the model is loaded, and which numbers
//! that model's space is ratcheted at.
//!
//! ## What this module stopped measuring on 2026-09-12
//!
//! It used to run the 39 probes twice -- once lexically, once with a semantic channel fused in --
//! and read ADR-0004's per-category acceptance off the difference. There is no such difference to
//! read any more, and the reason is architectural rather than numerical:
//!
//! * ADR-0007's amendment retired the query path. `SemanticChannel` has no query method; nothing
//!   in production encodes a Working Intent, so "how many probes does the embedding channel lift"
//!   names a mechanism that does not exist.
//! * The dual-lane rebuild (`docs/dual-lane/plan.md`, S2-4) made automatic injection Lane A plus
//!   Lane B and nothing else. `probe-ext-v1`'s 39 probes carry no file footprint, so Lane A seeds
//!   nothing, so Lane B never runs, so every automatic Pack is empty with or without a model
//!   attached. The `run_suite`/`automatic_top1`/`print_category_comparison` machinery measured
//!   5/39 on both arms -- the five noise probes, scored for correctly returning nothing -- and the
//!   ADR-0004 delta assertions could no longer be satisfied by any model.
//!
//! The three lexical probe suites hold the surviving hit-rate numbers: `context_search` at 30/39
//! (`association_probe_ext_workflow`) and the empty-Pack statement beside it, with
//! `association_probe_lane_workflow` as automatic injection's one positive ratchet. This module
//! does not re-measure any of them. What it measures instead is the one thing a real model is still
//! needed for here: whether this encoder separates this corpus from unrelated text *in the space
//! production writes vectors in*.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use sctx_engineering_graph::EngineeringProjectionStore;
use sctx_index::{ProjectionIndex, SEARCH_RANKING_VERSION};
use sctx_search::{
    EmbeddingProvider, SearchEngine, SemanticCacheKey, SemanticVectorCache,
    embedding::cosine_similarity,
};
use serde_json::Value;

use super::Harness;

/// The category whose probes have no answer, and whose scores are therefore the noise ceiling.
pub const NOISE_CATEGORY: &str = "noise";

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
        // Corpus texts are documents: `encode_bulk` takes the Document role, matching what the
        // production backfill (`fill_missing_vectors`) and the hop-2 calibration encode. Until
        // 2026-09-12 this called `encode`, whose F2LLM path prepends the query instruction -- so
        // the ADR-0004 F2LLM arm was accepted in a query x query space production never uses. The
        // signal-pollution audit confirmed the mismatch; every fused number measured before this
        // line changed belongs to that other space.
        let vector = provider
            .encode_bulk(text)
            .expect("every corpus text encodes");
        cache.store(&key, *revision_id, &vector).unwrap();
    }
    println!(
        "embedded {} revision(s) in {:?}",
        embeddable.len(),
        started.elapsed()
    );
    (cache, key, embeddable.len())
}

/// The span of top-1 scores one probe category reached, in basis points.
pub struct CategorySpread {
    pub count: usize,
    /// The category's worst probe. For a positive category this is what a floor would have to
    /// clear from below; for `noise` it says nothing and is printed for completeness.
    pub lowest: u16,
    /// The category's best probe. For `noise` this is the number that matters.
    pub highest: u16,
}

/// One encoder's separation on one fixture, measured entirely in the document space.
pub struct Separation {
    /// Lowest top-1 score any probe with an answer reached, and which probe reached it.
    pub worst_positive: u16,
    pub worst_positive_probe: String,
    /// Highest top-1 score any probe with no answer reached, and which probe reached it.
    pub best_noise: u16,
    pub best_noise_probe: String,
    pub by_category: BTreeMap<String, CategorySpread>,
}

impl Separation {
    /// The gap the two distributions leave between them. Negative gaps are the interesting ones,
    /// which is why this is signed rather than a `saturating_sub`.
    pub fn margin(&self) -> i32 {
        i32::from(self.worst_positive) - i32::from(self.best_noise)
    }

    pub fn lowest_in(&self, category: &str) -> u16 {
        self.by_category
            .get(category)
            .map_or(0, |spread| spread.lowest)
    }
}

/// Scores every probe text against the cached corpus **with both sides in the document role**, and
/// returns the two numbers that bound the result.
///
/// ## Why both sides are documents
///
/// The corpus half is not a choice: `embed_corpus` writes exactly what the production backfill
/// writes, and that is `encode_bulk`, the Document role. The probe half is the change ADR-0007's
/// amendment forces. A cosine is only meaningful between two vectors of one population, and the
/// population production compares in is document against document -- Lane B's second hop takes a
/// seed Context's stored vector and a candidate Context's stored vector, and no query is encoded
/// anywhere on the path. Encoding the probe text as a query would measure the space that was
/// retired, which for F2LLM is a genuinely different space: the query role prepends an instruction
/// this family was trained to see in front of questions and never in front of corpus text.
///
/// ## What it therefore does and does not claim
///
/// A probe text encoded as a document is a *proxy* for a stored Context, not one: it is 15--356
/// characters of paraphrase rather than a `statement` and `rationale` written by a Checkpoint. So
/// this is a measurement of the **space** -- does this encoder put text about the dedupe window
/// near the Context about the dedupe window, and unrelated text nowhere near it -- and not of the
/// lane. The lane itself is measured where it lives, against Contexts on both sides:
/// `embedding_hop2_admission_calibration` and `lanes::hop2_ratchet` in `sctx-search`.
///
/// Top-1 over the whole corpus rather than the score against the probe's own expected Context, for
/// the same reason the measurement it replaced used top-1: a separation is a statement about which
/// scores the two populations can reach at all, and scoring only the intended pair would hide the
/// unrelated Context that outscored it.
pub fn document_space_separation(
    fixture: &Value,
    provider: &Arc<dyn EmbeddingProvider>,
    corpus: &[(sctx_domain::RevisionId, Vec<f32>)],
) -> Separation {
    assert!(!corpus.is_empty(), "an empty corpus separates nothing");
    println!("\n--- document-space top-1 per probe (basis points) ---");
    let mut separation = Separation {
        worst_positive: u16::MAX,
        worst_positive_probe: String::new(),
        best_noise: 0,
        best_noise_probe: String::new(),
        by_category: BTreeMap::new(),
    };
    for probe in fixture["probes"].as_array().unwrap() {
        let id = probe["id"].as_str().unwrap();
        let category = probe["category"].as_str().unwrap();
        let query = probe["query"].as_str().unwrap();
        let is_noise = probe["expected"].as_array().unwrap().is_empty();
        assert_eq!(
            is_noise,
            category == NOISE_CATEGORY,
            "probe {id} is tagged {category} but its expected set says otherwise; the two \
             populations below would be built from the wrong rows"
        );
        let vector = provider
            .encode_bulk(query)
            .expect("every probe text encodes");
        let top = corpus
            .iter()
            .map(|(_, candidate)| basis_points(cosine_similarity(&vector, candidate)))
            .max()
            .expect("the corpus is not empty");
        if is_noise {
            if top > separation.best_noise || separation.best_noise_probe.is_empty() {
                separation.best_noise = top;
                id.clone_into(&mut separation.best_noise_probe);
            }
        } else if top < separation.worst_positive {
            separation.worst_positive = top;
            id.clone_into(&mut separation.worst_positive_probe);
        }
        let spread = separation
            .by_category
            .entry(category.to_owned())
            .or_insert(CategorySpread {
                count: 0,
                lowest: u16::MAX,
                highest: 0,
            });
        spread.count += 1;
        spread.lowest = spread.lowest.min(top);
        spread.highest = spread.highest.max(top);
        println!("{id:<10} {category:<16} {top:>6}");
    }
    assert!(
        separation.worst_positive != u16::MAX,
        "the fixture holds no probe with an answer; there is no positive population to separate"
    );

    println!("\n--- document-space spread per category (basis points) ---");
    println!(
        "{:<16} {:>6} {:>10} {:>10}",
        "category", "count", "lowest", "highest"
    );
    for (category, spread) in &separation.by_category {
        println!(
            "{category:<16} {:>6} {:>10} {:>10}",
            spread.count, spread.lowest, spread.highest
        );
    }
    println!(
        "\nworst positive {} ({}), best noise {} ({}), margin {}",
        separation.worst_positive,
        separation.worst_positive_probe,
        separation.best_noise,
        separation.best_noise_probe,
        separation.margin()
    );
    separation
}

/// Cosine to basis points, clamped, by the same rounding every recorded score uses.
fn basis_points(similarity: f32) -> u16 {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let scaled = (similarity.clamp(0.0, 1.0) * 10_000.0).round() as u16;
    scaled
}
