//! Calibrates [`QWEN3_SEMANTIC_SIMILARITY_FLOOR_BASIS_POINTS`] on the stack that ships.
//!
//! ADR-0004's T5a procedure produced bge-m3's floor by reading two numbers off a real run over the
//! probe fixtures: the highest similarity any *noise* query reaches, and the lowest any positive
//! reaches. The F2LLM floor was seeded from a torch/Python benchmark instead -- different fixtures,
//! a different runtime, no cross-lingual positive read off the same run -- and its doc comment said
//! so, because a cosine floor is a property of one embedding space measured under one stack. This
//! file is the T5a procedure repeated for that family here, and its output is what the constant's
//! doc comment records.
//!
//! Twice now this repository has set a retrieval constant from numbers produced somewhere other than
//! where it runs, and twice the number was wrong in a way nothing failed on (ADR-0004's two encode
//! budget revisions). So the measurement lives in the test suite rather than in a notebook: the next
//! person to question the floor reruns this and reads a table, instead of deriving one again.
//!
//! `#[ignore]`d because it needs roughly 2.4 GB of weights this repository deliberately does not
//! ship. Point it at a Hugging Face snapshot and the ONNX Runtime library:
//!
//! ```text
//! SCTX_PROBE_F2LLM_MODEL=~/.cache/huggingface/hub/models--codefuse-ai--F2LLM-v2-0.6B/snapshots/<sha> \
//! SCTX_PROBE_EMBEDDING_RUNTIME=~/.shared-context/embedding/runtime/libonnxruntime.dylib \
//!   cargo test -p sctx-search --test embedding_qwen3_floor_calibration -- --ignored --nocapture
//! ```
//!
//! ## What is measured
//!
//! The corpus is every Context in the three association fixtures, encoded through the *document*
//! path as `statement` and `rationale` joined with a newline -- the text
//! [`sctx_search::SearchEngine::embeddable_revisions`] builds, on the fields these fixtures fill.
//! The queries are every probe in those fixtures, encoded through the *query* path, so the
//! instruction prefix is applied by the provider exactly as it is in production and never by this
//! file. Each fixture is its own corpus: a probe's `expected` names indices into the fixture it came
//! from, and pooling the three would let a query match a Context its own fixture never contained.
//!
//! A positive probe is scored by its best *expected* Context, which is the similarity the floor has
//! to admit for that probe to be rescuable. A noise probe is scored by its best Context overall,
//! which is the similarity the floor has to refuse. Those two columns are the whole calibration.

#![cfg(all(feature = "embedding-onnx", unix))]

mod f2llm_snapshot;

use std::collections::BTreeMap;

use f2llm_snapshot::provider;
use sctx_search::{
    EmbeddingProvider, QWEN3_SEMANTIC_SIMILARITY_FLOOR_BASIS_POINTS, embedding::cosine_similarity,
};
use serde_json::Value;

/// The three probe fixtures, by the name the report prints them under.
const FIXTURES: &[(&str, &str)] = &[
    (
        "probe-v1",
        include_str!("../../../fixtures/association/probe-v1.json"),
    ),
    (
        "probe-zh-v1",
        include_str!("../../../fixtures/association/probe-zh-v1.json"),
    ),
    (
        "probe-ext-v1",
        include_str!("../../../fixtures/association/probe-ext-v1.json"),
    ),
];

/// Positives this calibration keeps at the floor the constant now holds.
///
/// A ratchet, not a target: it is the count measured by the run recorded in the constant's doc
/// comment. Recalibrating deliberately means moving this line with the constant and saying why in
/// the same commit; drifting into a lower count without noticing is what it prevents.
const RETAINED_POSITIVES: usize = 75;

/// Positives the fixtures hold in total, so a change in the corpus is not read as a recall change.
const TOTAL_POSITIVES: usize = 77;

/// One probe's measurement.
struct Scored {
    fixture: &'static str,
    id: String,
    category: String,
    /// Empty `expected`: the probe must retrieve nothing at all.
    noise: bool,
    /// A positive's best expected Context, or a noise probe's best Context overall, in basis
    /// points.
    score: u16,
    /// A positive's best Context overall, in basis points. Equal to `score` when the top-scoring
    /// Context is an expected one, and above it when a distractor outscores the answer -- which the
    /// floor cannot fix and fusion has to.
    top: u16,
    /// This query against every Context in its fixture, in basis points.
    ///
    /// The floor decides how many of these enter the channel, and that count matters as much as
    /// which probe is rescued: fusion ranks *within* a channel, so a floor low enough to admit a
    /// query's whole corpus hands out ranks to neighbours that mean nothing and lets them outvote a
    /// strong lexical match by sheer number. [`sctx_search::SEMANTIC_CHANNEL_LIMIT`] caps the
    /// damage; it does not make an indiscriminate channel informative.
    against_corpus: Vec<u16>,
}

/// Cosine in basis points, by the same rounding the channel applies before comparing to the floor.
fn basis_points(similarity: f32) -> u16 {
    if !similarity.is_finite() || similarity <= 0.0 {
        return 0;
    }
    let scaled = (similarity * 10_000.0).round();
    if scaled >= 10_000.0 {
        return 10_000;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    {
        scaled as u16
    }
}

/// The text a fixture Context contributes to the embedding index.
///
/// `statement` and `rationale` joined with a newline: `embeddable_revisions` joins `problem_view`
/// too, and no fixture Context carries one, so this is that function's output on this corpus rather
/// than an approximation of it.
fn corpus_text(context: &Value) -> String {
    ["statement", "rationale"]
        .into_iter()
        .filter_map(|field| context.get(field).and_then(Value::as_str))
        .map(str::trim)
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Encodes one fixture's corpus and scores every one of its probes against it.
fn score_fixture(name: &'static str, raw: &str, provider: &dyn EmbeddingProvider) -> Vec<Scored> {
    let fixture: Value = serde_json::from_str(raw).expect("the probe fixture is valid JSON");
    let contexts = fixture["contexts"]
        .as_array()
        .expect("the fixture holds a context array");

    let corpus = contexts
        .iter()
        .map(|context| {
            let index = context["index"]
                .as_u64()
                .expect("a context carries an index");
            let vector = provider
                .encode_bulk(&corpus_text(context))
                .expect("every corpus text encodes");
            (index, vector)
        })
        .collect::<Vec<_>>();

    fixture["probes"]
        .as_array()
        .expect("the fixture holds a probe array")
        .iter()
        .map(|probe| {
            let query = probe["query"].as_str().expect("a probe carries a query");
            let expected = probe["expected"]
                .as_array()
                .expect("a probe carries an expected array")
                .iter()
                .map(|value| value.as_u64().expect("expected holds indices"))
                .collect::<Vec<_>>();
            let vector = provider.encode(query).expect("every probe query encodes");

            let mut best_expected = 0_u16;
            let mut top = 0_u16;
            let mut against_corpus = Vec::with_capacity(corpus.len());
            for (index, corpus_vector) in &corpus {
                let score = basis_points(cosine_similarity(&vector, corpus_vector));
                against_corpus.push(score);
                top = top.max(score);
                if expected.contains(index) {
                    best_expected = best_expected.max(score);
                }
            }

            Scored {
                fixture: name,
                id: probe["id"]
                    .as_str()
                    .expect("a probe carries an id")
                    .to_owned(),
                category: probe["category"]
                    .as_str()
                    .expect("a probe carries a category")
                    .to_owned(),
                noise: expected.is_empty(),
                score: if expected.is_empty() {
                    top
                } else {
                    best_expected
                },
                top,
                against_corpus,
            }
        })
        .collect()
}

/// Prints how many positives each candidate floor keeps, per category.
///
/// The sweep is what makes the chosen value auditable as a choice rather than a reading: it shows
/// what the next hundred basis points would have cost, and therefore whether the value sits on a
/// plateau or on the edge of a cliff that one fixture happens to define.
fn print_sweep(scored: &[Scored], noise_ceiling: u16) {
    let mut categories = scored
        .iter()
        .filter(|probe| !probe.noise)
        .map(|probe| probe.category.clone())
        .collect::<Vec<_>>();
    categories.sort_unstable();
    categories.dedup();

    println!("\n--- positives kept by candidate floor ---");
    print!("{:>6}  {:>7}  {:>9}", "floor", "kept", "admitted");
    for category in &categories {
        print!("  {category:>12}");
    }
    println!();

    let first = noise_ceiling.next_multiple_of(100);
    for step in 0..10 {
        let floor = first + step * 100;
        let kept = scored
            .iter()
            .filter(|probe| !probe.noise && probe.score >= floor)
            .count();
        // How much corpus this floor lets into the channel, per query, out of how much it could.
        let admitted = scored
            .iter()
            .map(|probe| {
                probe
                    .against_corpus
                    .iter()
                    .filter(|score| **score >= floor)
                    .count()
            })
            .sum::<usize>();
        let corpus = scored
            .iter()
            .map(|probe| probe.against_corpus.len())
            .sum::<usize>();
        #[allow(clippy::cast_precision_loss)]
        let admitted = admitted as f64 / scored.len() as f64;
        #[allow(clippy::cast_precision_loss)]
        let corpus = corpus as f64 / scored.len() as f64;
        print!(
            "{floor:>6}  {kept:>7}  {:>9}",
            format!("{admitted:.1}/{corpus:.1}")
        );
        for category in &categories {
            let total = scored
                .iter()
                .filter(|probe| !probe.noise && &probe.category == category)
                .count();
            let kept = scored
                .iter()
                .filter(|probe| !probe.noise && &probe.category == category && probe.score >= floor)
                .count();
            print!("  {:>12}", format!("{kept}/{total}"));
        }
        println!();
    }
}

#[test]
#[ignore = "needs a real F2LLM-v2-0.6B snapshot; see the module docs"]
fn the_qwen3_floor_separates_every_noise_query_from_the_positives_it_keeps() {
    let provider = provider().as_ref();

    let scored = FIXTURES
        .iter()
        .flat_map(|(name, raw)| score_fixture(name, raw, provider))
        .collect::<Vec<_>>();

    println!("\n--- per-probe similarity (basis points) ---");
    println!(
        "{:<13} {:<10} {:<24} {:>7} {:>7}",
        "fixture", "probe", "category", "score", "top"
    );
    for probe in &scored {
        println!(
            "{:<13} {:<10} {:<24} {:>7} {:>7}",
            probe.fixture,
            probe.id,
            if probe.noise {
                format!("{} (noise)", probe.category)
            } else {
                probe.category.clone()
            },
            probe.score,
            probe.top
        );
    }

    let noise_ceiling = scored
        .iter()
        .filter(|probe| probe.noise)
        .map(|probe| probe.score)
        .max()
        .expect("the fixtures hold noise probes");
    let positive_floor = scored
        .iter()
        .filter(|probe| !probe.noise)
        .map(|probe| probe.score)
        .min()
        .expect("the fixtures hold positive probes");
    let positives = scored.iter().filter(|probe| !probe.noise).count();
    assert_eq!(
        positives, TOTAL_POSITIVES,
        "the fixtures changed shape; the retained-positive ratchet below is no longer comparable"
    );

    println!(
        "\nnoise ceiling {noise_ceiling}, lowest positive {positive_floor}, floor in the source \
         {QWEN3_SEMANTIC_SIMILARITY_FLOOR_BASIS_POINTS}"
    );

    print_sweep(&scored, noise_ceiling);

    // The hard constraint. A semantic hit is its own injection eligibility path, so one noise query
    // above the floor is enough to put a Context in front of an Agent who asked about something
    // else -- which is worse than the positives a higher floor gives up.
    assert!(
        QWEN3_SEMANTIC_SIMILARITY_FLOOR_BASIS_POINTS > noise_ceiling,
        "the floor must sit above every noise query on these fixtures: ceiling {noise_ceiling}, \
         floor {QWEN3_SEMANTIC_SIMILARITY_FLOOR_BASIS_POINTS}"
    );

    let retained = scored
        .iter()
        .filter(|probe| !probe.noise && probe.score >= QWEN3_SEMANTIC_SIMILARITY_FLOOR_BASIS_POINTS)
        .count();
    println!("positives at or above the floor: {retained}/{positives}");

    let mut sacrificed: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for probe in scored
        .iter()
        .filter(|probe| !probe.noise && probe.score < QWEN3_SEMANTIC_SIMILARITY_FLOOR_BASIS_POINTS)
    {
        sacrificed
            .entry(probe.category.as_str())
            .or_default()
            .push(probe.id.as_str());
    }
    println!("--- positives the floor gives up ---");
    for (category, ids) in &sacrificed {
        println!("{category:<24} {}", ids.join(", "));
    }

    assert!(
        retained >= RETAINED_POSITIVES,
        "the floor kept {retained}/{positives} positives, below the {RETAINED_POSITIVES} this \
         calibration measured: either the floor moved without this ratchet, or the encoder changed"
    );
}
