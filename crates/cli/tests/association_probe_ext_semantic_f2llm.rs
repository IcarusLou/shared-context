//! What the default export's embedding space does to the extended probe corpus.
//!
//! The model is F2LLM-v2-0.6B, the one `sctx embedding install` installs, and the corpus is built
//! by the real harness -- the same Git store, the same public MCP confirmation chain, the same
//! projection -- so the text encoded here is the text the production backfill encodes. What is
//! measured is one number pair: the lowest score any probe with an answer reaches against that
//! corpus, and the highest any probe without one reaches. Everything except the model and those
//! numbers is shared with `association_probe_ext_semantic` through
//! `association_probe_harness::semantic`.
//!
//! `#[ignore]`d because it needs roughly 2.4 GB of weights this repository deliberately does not
//! ship. Point it at an unmodified Hugging Face snapshot -- or at the installed model directory,
//! whose layout is the same flat one -- and at the ONNX Runtime library:
//!
//! ```text
//! SCTX_PROBE_F2LLM_MODEL=~/.shared-context/embedding/model \
//! SCTX_PROBE_EMBEDDING_RUNTIME=~/.shared-context/embedding/runtime/libonnxruntime.dylib \
//!   cargo test --release --locked -p sctx-cli --test association_probe_ext_semantic_f2llm -- \
//!     --ignored --nocapture
//! ```
//!
//! ## What this suite stopped asserting on 2026-09-12, and where each assertion went
//!
//! It was ADR-0004's acceptance: run the 39 probes lexically, run them again with a semantic
//! channel fused in, and require `paraphrase` and `cross_lingual` to gain while `identifier` and
//! `noise` did not regress. Every clause of that has lost its subject, and the replacement for each
//! is named here so the next reader does not have to reconstruct it.
//!
//! | retired assertion | why | what covers it now |
//! |---|---|---|
//! | in-process lexical control at 27/39, `long_intent` at 3/3 | the automatic Pack it measured is Lane A plus Lane B (S2-4), and this fixture's probes carry no file footprint, so the control now measures 5/39 -- the five noise probes returning nothing | `association_probe_ext_workflow`'s `context_search` ratchet at 30/39, through the binary, which is the entry point that still ranks lexically |
//! | `paraphrase`/`cross_lingual` net gain, `identifier` no regression, fused total at 32/39 | there is no fused Pack. ADR-0007's amendment retired the query path; `SemanticChannel` has no query method, and Lane B never runs without a Lane A seed | the per-category *spread* below, in the document space, which is the only form of the claim a document-to-document architecture can make |
//! | `noise_leaks == 0` on both arms | every automatic Pack in this fixture is empty, so there is nothing to leak | `assert_every_automatic_pack_is_empty` in the three lexical suites, and `association_probe_lane_workflow`'s noise count on a fixture that does have footprints |
//! | `long_intent` p95 increment, first encode against cached encode | nothing on the retrieval path encodes any more, so the increment is zero by construction rather than by measurement | `embedding_encode_latency`, which measures the backfill's own encode cost by length |
//!
//! What survives is the half of the old run that was always a property of the encoder rather than
//! of the retrieval code: `print_similarity_separation`, which scored the provider directly against
//! the cached corpus. It is now `document_space_separation` and encodes the probe side through the
//! Document role too, because that is the population production compares in. See its doc comment
//! for what a probe-as-document does and does not claim.
//!
//! ## Why the numbers below are not the numbers this file used to hold
//!
//! They are readings of a different space. The corpus side moved on 2026-09-12, when `embed_corpus`
//! stopped calling `encode` -- whose F2LLM path prepends the query instruction -- and started
//! calling `encode_bulk` like the backfill does; the probe side moved with it. Every number the
//! fused era recorded (a 4553 worst positive against a noise ceiling of 0, a 2800 "noise ceiling"
//! constant whose doc described a distribution belonging to the deleted
//! `embedding_qwen3_floor_calibration`) is a reading of a query x query space this repository never
//! ran in production. They are not comparable to these and are not carried forward.

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
    build_harness,
    semantic::{document_space_separation, embed_corpus, engine, installation_root},
};
use sctx_search::{
    EmbeddingProvider, EmbeddingSemanticChannel, SemanticChannel,
    embedding::onnx::OnnxEmbeddingProvider,
};
use serde_json::Value;

const PROBE_FIXTURE: &str = include_str!("../../../fixtures/association/probe-ext-v1.json");

/// Accepted revisions the harness leaves behind for the backfill to embed.
///
/// Eleven, not the fixture's twelve Contexts: one of them is superseded by another -- the rounding
/// fix supersedes the original drift, which is what the `multi_hop` probes are built on -- and
/// `embeddable_revisions` returns current accepted revisions only, so the superseded one is not in
/// the corpus production would compare against either.
///
/// Pinned for the same reason the hop-2 calibration pins its Context count: the separation below is
/// a statement about a top-1 score over *this* corpus, and a corpus that grew a Context is a corpus
/// the ratchets were not measured on.
const CORPUS_REVISIONS: usize = 11;

/// Lowest document-space top-1 score any probe with an answer reached, in basis points.
///
/// A ratchet, not a target: it is what three release runs measured on 2026-09-12, and its job is to
/// make a silent regression noisy. Lowering it deliberately means saying why in the same commit.
///
/// The run: 34 probes with an answer span 4081--8388, the 5 noise probes 503--1980. The floor is
/// `id-02`, a bare dotted config key, and the ceiling of the noise side is `noise-05`. Rounded
/// outward to the nearest ten, following the hop-2 calibration's convention -- pinning a
/// float-derived basis point exactly would make a last-bit difference in one cosine a failure, and
/// this measurement is not that sharp.
const F2LLM_WORST_POSITIVE_BASIS_POINTS: u16 = 4_080;

/// Highest document-space top-1 score any probe with no answer reached, in basis points.
///
/// Ratcheted from above, and the more load-bearing of the two: a noise probe that climbs into the
/// positives' band is the failure mode a document-to-document admission decision cannot survive.
/// Measured 1980; see the constant above for the rounding.
///
/// Worth reading beside `SEMANTIC_HOP2_ADMISSION_FLOOR_BASIS_POINTS` (5200), which governs the one
/// production comparison in this space: unrelated text tops out more than three thousand basis
/// points below that floor here, while the positives straddle it. That is the first time a probe
/// suite's numbers have been in the same neighbourhood as the constant production actually uses --
/// the query-space readings this file used to hold never were, which is the whole of ADR-0007's
/// diagnosis in one line.
const F2LLM_BEST_NOISE_BASIS_POINTS: u16 = 1_990;

/// The two categories ADR-0004 added the channel for, held at their measured floor.
///
/// This is the ADR's per-category clause in the only form the document space can state it. The old
/// form compared a fused hit count against a lexical one; there is no fused Pack to count. What is
/// still true and still worth guarding is that an English query about a Chinese Context, and a
/// paraphrase that shares no wording with one, land near that Context in this space -- the property
/// the lexical channel cannot have and the reason an encoder is installed at all.
///
/// Measured: `cross_lingual` 5213--6964 over 5 probes, `paraphrase` 4091--7834 over 9. Both floors
/// sit above the noise ceiling by more than three thousand basis points, and `cross_lingual`'s
/// whole span sits above the hop-2 admission floor -- the category the lexical channel resolves
/// 1/5 of is the one this space places highest.
const F2LLM_CROSS_LINGUAL_LOWEST_BASIS_POINTS: u16 = 5_210;
const F2LLM_PARAPHRASE_LOWEST_BASIS_POINTS: u16 = 4_090;

#[test]
#[ignore = "needs a real F2LLM-v2-0.6B snapshot; see the module docs"]
fn the_default_export_separates_this_corpus_in_the_document_space() {
    let fixture = serde_json::from_str::<Value>(PROBE_FIXTURE).unwrap();
    let harness = build_harness(&fixture);
    let root = installation_root(&harness);
    let engine = engine(&root);

    // Load the model once. The load covers reshaping the snapshot and initialising the runtime as
    // well as the session, which is what an operator waits for on the first `sctx mcp serve` too.
    let load_started = Instant::now();
    let provider = f2llm_snapshot::provider();
    println!(
        "model load (runtime init + session) {:?}",
        load_started.elapsed()
    );
    let provider = Arc::<OnnxEmbeddingProvider>::clone(provider) as Arc<dyn EmbeddingProvider>;
    // A literal fingerprint rather than `model_fingerprint` of the directory: this suite loads the
    // model out of a directory of symlinks into the Hugging Face cache, and `model_fingerprint`
    // reads `DirEntry::metadata`, which on Unix does not follow a symlink -- so it sees a directory
    // holding no files and refuses. Nothing about that is load-bearing here: the fingerprint only
    // has to be stable for the length of one run, because the cache lives in a temporary home.
    let (cache, key, embeddable) = embed_corpus(&engine, &provider, &root, "f2llm-probe-snapshot");
    assert_eq!(
        embeddable, CORPUS_REVISIONS,
        "the harness left {embeddable} embeddable revision(s) rather than {CORPUS_REVISIONS}; the \
         fixture changed shape and the ratchets below are no longer comparable"
    );

    let corpus = cache.load(&key).unwrap();
    // The channel is built and read through the face Lane B reads, rather than just asserted to
    // hold the right count: `document_vectors` is the one method the trait still has, and a channel
    // that published nothing would make the second hop silently unavailable in production while
    // this suite's own arithmetic went on working off `cache.load`.
    let channel = Arc::new(EmbeddingSemanticChannel::from_cache(&cache, &key).unwrap());
    assert_eq!(channel.corpus_size(), embeddable);
    let published = (Arc::clone(&channel) as Arc<dyn SemanticChannel>)
        .document_vectors()
        .expect("a channel over a filled cache publishes its document vectors");
    assert_eq!(
        published.len(),
        embeddable,
        "the snapshot Lane B would read holds {} of {embeddable} corpus vector(s)",
        published.len()
    );

    let separation = document_space_separation(&fixture, &provider, &corpus);

    // The structural claim first, because it is the one that is true or false rather than high or
    // low: the two populations do not overlap at all on this fixture. A model swap that inverted
    // them would fail here even if both ratchets below had been re-measured around it.
    assert!(
        separation.margin() > 0,
        "the populations overlap: worst positive {} ({}) is at or below best noise {} ({})",
        separation.worst_positive,
        separation.worst_positive_probe,
        separation.best_noise,
        separation.best_noise_probe
    );
    assert!(
        separation.worst_positive >= F2LLM_WORST_POSITIVE_BASIS_POINTS,
        "the weakest positive fell to {} ({}) against the measured \
         {F2LLM_WORST_POSITIVE_BASIS_POINTS}",
        separation.worst_positive,
        separation.worst_positive_probe
    );
    assert!(
        separation.best_noise <= F2LLM_BEST_NOISE_BASIS_POINTS,
        "unrelated text climbed to {} ({}) against the measured {F2LLM_BEST_NOISE_BASIS_POINTS}",
        separation.best_noise,
        separation.best_noise_probe
    );
    assert!(
        separation.lowest_in("cross_lingual") >= F2LLM_CROSS_LINGUAL_LOWEST_BASIS_POINTS,
        "the weakest cross-lingual probe fell to {} against the measured \
         {F2LLM_CROSS_LINGUAL_LOWEST_BASIS_POINTS}",
        separation.lowest_in("cross_lingual")
    );
    assert!(
        separation.lowest_in("paraphrase") >= F2LLM_PARAPHRASE_LOWEST_BASIS_POINTS,
        "the weakest paraphrase probe fell to {} against the measured \
         {F2LLM_PARAPHRASE_LOWEST_BASIS_POINTS}",
        separation.lowest_in("paraphrase")
    );
}
