//! The same document-space separation as the default export's suite, over whichever export the
//! operator names. This is the arm a model swap is argued on.
//!
//! It was bge-m3's arm of ADR-0004's acceptance, back when the acceptance was a fused hit count and
//! bge-m3 was the default. Both of those are gone -- see
//! `association_probe_ext_semantic_f2llm`'s module docs for the assertion-by-assertion disposition,
//! which applies here unchanged -- and what is left is the one question a second encoder is still
//! asked: *on the corpus production actually writes vectors for, does this export separate what
//! belongs together from what does not, and how does that compare with the installed one?*
//!
//! ```text
//! SCTX_PROBE_EMBEDDING_MODEL=/path/to/some-onnx-export \
//! SCTX_PROBE_EMBEDDING_RUNTIME=~/.shared-context/embedding/runtime/libonnxruntime.dylib \
//!   cargo test --release --locked -p sctx-cli --test association_probe_ext_semantic -- \
//!     --ignored --nocapture
//! ```
//!
//! ## Why this arm carries no ratchet
//!
//! The default export's suite pins four numbers because those are readings of the encoder this
//! repository installs, taken on the machine that installs it. This one pins none, deliberately.
//!
//! Its old constant (`SEMANTIC_SIMILARITY_FLOOR_BASIS_POINTS`, then a local 5200 with a 5095 noise
//! ceiling and a 5640 weakest positive beside it) is a reading of bge-m3 in a corpus space that no
//! longer exists: `SEMANTIC_CORPUS_VERSION` went from `"1"` to `"2"` on 2026-09-11, which stopped
//! the corpus text being `normalize_search_text` shrapnel and made it the revision fields as
//! written -- for a Chinese fixture, a different text space rather than a lightly different string.
//! Nobody has re-run bge-m3 since, and nobody can: there are no bge-m3 weights on the machine that
//! installs F2LLM by default. Carrying those numbers forward as a ratchet would be pinning a
//! measurement of a space the suite no longer runs in, which is exactly the defect this pass
//! exists to clear.
//!
//! So what it asserts is the structural claim, which is model-independent and still sharp: the
//! positives and the noise do not overlap. What it *reports* is the whole table, which is the
//! comparison. Pointing it at an export and reading the printed spread against the constants in
//! the F2LLM suite is the model-selection procedure; see `docs/dual-lane/plan.md`, the real-model
//! manual checklist, for the exact commands and which three suites a selection has to agree on.
//!
//! One caution about reading the two tables side by side: the family decides whether the document
//! role is even distinguishable. F2LLM (Qwen3) prepends an instruction to queries and nothing to
//! documents, so its query and document spaces differ; bge-m3 and most encoder-style exports use
//! one text path for both, so for them `encode_bulk` and `encode` return the same vector and this
//! suite's move to the document role changes nothing about their numbers. A cosine is still not
//! portable between two exports' spaces -- the margin is, which is why the margin is what the
//! comparison is read off.

mod association_probe_harness;

use std::{fs, path::PathBuf};

use association_probe_harness::{
    build_harness,
    semantic::{document_space_separation, embed_corpus, engine, installation_root},
};
use sctx_search::EmbeddingSemanticChannel;
use serde_json::Value;

const PROBE_FIXTURE: &str = include_str!("../../../fixtures/association/probe-ext-v1.json");

/// Accepted revisions the harness leaves behind for the backfill to embed. Eleven rather than the
/// fixture's twelve Contexts, because one is superseded and `embeddable_revisions` returns current
/// accepted revisions only. Pinned so a fixture that grew a Context is not read as an export that
/// separates differently.
const CORPUS_REVISIONS: usize = 11;

fn model_paths() -> Option<(PathBuf, PathBuf)> {
    let model = std::env::var_os("SCTX_PROBE_EMBEDDING_MODEL")?;
    let runtime = std::env::var_os("SCTX_PROBE_EMBEDDING_RUNTIME")?;
    Some((PathBuf::from(model), PathBuf::from(runtime)))
}

#[test]
#[ignore = "requires a locally downloaded ONNX export and an ONNX Runtime library"]
fn a_named_export_separates_this_corpus_in_the_document_space() {
    let Some((model, runtime)) = model_paths() else {
        panic!(
            "set SCTX_PROBE_EMBEDDING_MODEL and SCTX_PROBE_EMBEDDING_RUNTIME; see the module docs"
        );
    };
    let fixture = serde_json::from_str::<Value>(PROBE_FIXTURE).unwrap();
    let harness = build_harness(&fixture);
    let root = installation_root(&harness);
    let engine = engine(&root);

    let provider = sctx_search::load_onnx_provider(&model, &runtime)
        .expect("the configured model and runtime must load");
    // Named in the output rather than asserted: the whole point of this arm is that the export is
    // the operator's choice, and a comparison table that does not say which encoder produced it is
    // not a comparison. The width comes off the provider's own probe, so it is what the session
    // returns rather than what the directory claims.
    println!(
        "export {} -- {} dimensions",
        model.display(),
        provider.dimensions()
    );
    let fingerprint = sctx_search::model_fingerprint(&model).unwrap();
    let (cache, key, embeddable) = embed_corpus(&engine, &provider, &root, &fingerprint);
    assert_eq!(
        embeddable, CORPUS_REVISIONS,
        "the harness left {embeddable} embeddable revision(s) rather than {CORPUS_REVISIONS}; the \
         fixture changed shape and this table is not comparable with the other arm's"
    );

    let corpus = cache.load(&key).unwrap();
    let channel = EmbeddingSemanticChannel::from_cache(&cache, &key).unwrap();
    assert_eq!(channel.corpus_size(), embeddable);

    let separation = document_space_separation(&fixture, &provider, &corpus);
    assert!(
        separation.margin() > 0,
        "this export does not separate the fixture at all: worst positive {} ({}) is at or below \
         best noise {} ({}). Whatever else the table says, an admission decision cannot be built \
         on it",
        separation.worst_positive,
        separation.worst_positive_probe,
        separation.best_noise,
        separation.best_noise_probe
    );
}

/// Keeps the fixture path honest even when the ignored test never runs.
#[test]
fn the_extended_probe_fixture_is_readable() {
    let fixture = serde_json::from_str::<Value>(PROBE_FIXTURE).unwrap();
    assert_eq!(fixture["probes"].as_array().unwrap().len(), 39);
    let _ = fs::metadata("../../fixtures/association/probe-ext-v1.json");
}
