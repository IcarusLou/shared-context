//! Measures what one real encode actually costs, by query length, for each model family.
//!
//! T5b set [`sctx_search::SEMANTIC_ENCODE_BUDGET`] to 200 ms on the strength of ADR-0004's
//! "30--85 ms p95", a number produced by a torch prototype and never re-measured against the `ort`
//! stack this workspace ships. It was then validated with probe queries 15--40 characters long. A
//! real Working Intent query is `goal + current_direction + in_scope` concatenated: the Codex
//! session that exposed the defect built a 283-character query, and every one of its automatic
//! retrievals reported `embedding_unavailable`.
//!
//! These tests exist so that constant is never again chosen without a measurement behind it. Both
//! are `#[ignore]`d because they need the operator's own model export:
//!
//! ```text
//! SCTX_PROBE_EMBEDDING_MODEL=~/.shared-context/embedding/model \
//! SCTX_PROBE_EMBEDDING_RUNTIME=~/.shared-context/embedding/runtime/libonnxruntime.dylib \
//!   cargo test --release -p sctx-search --test embedding_encode_latency -- --ignored --nocapture
//!
//! SCTX_PROBE_F2LLM_MODEL=~/.cache/huggingface/hub/models--codefuse-ai--F2LLM-v2-0.6B/snapshots/<sha> \
//! SCTX_PROBE_EMBEDDING_RUNTIME=~/.shared-context/embedding/runtime/libonnxruntime.dylib \
//!   cargo test --release -p sctx-search --test embedding_encode_latency -- --ignored --nocapture
//! ```
//!
//! One ladder per family, over the same lengths, because a budget shared by two encoders has to
//! hold for the slower one and there is no way to know which that is without measuring both. The
//! F2LLM ladder additionally reports the model load and the process's resident size, which is what
//! `docs/user-guide.md` quotes to an operator deciding whether to install it at all.
//!
//! They assert nothing about absolute timings -- those are a property of the machine, not the code
//! -- and print a table instead. The one thing they do assert is the property the budget has to
//! satisfy: a warm encode of a real-length Intent query must fit inside the configured budget.

#![cfg(feature = "embedding-onnx")]

#[cfg(unix)]
mod f2llm_snapshot;

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use sctx_search::{EmbeddingProvider, SEMANTIC_ENCODE_BUDGET, load_onnx_provider};
#[cfg(unix)]
use sctx_search::{embedding::SEMANTIC_MAX_TOKENS, embedding::onnx::TextRole};

/// Samples per length. Enough that a p95 is a measurement rather than the single worst of three.
const SAMPLES: usize = 24;

/// Query lengths, in characters, spanning the probe suite through a real Intent to the truncation
/// ceiling. 283 is the length measured on Codex session 01a06646.
const LENGTHS: &[usize] = &[20, 40, 100, 200, 283, 400, 700, 1400];

fn provider_paths() -> Option<(PathBuf, PathBuf)> {
    let model = std::env::var_os("SCTX_PROBE_EMBEDDING_MODEL")?;
    let runtime = std::env::var_os("SCTX_PROBE_EMBEDDING_RUNTIME")?;
    Some((PathBuf::from(model), PathBuf::from(runtime)))
}

/// Builds a query of roughly `length` characters in the shape retrieval actually submits: a goal,
/// a current direction and an in-scope list, joined the way the Working Intent query builder joins
/// them. Random text would measure the tokenizer on noise; this measures it on our own corpus.
fn intent_query(length: usize) -> String {
    const PHRASES: &[&str] = &[
        "把嵌入通道的编码预算按真实 Intent 长度重新标定",
        "current direction: 先测量再决定改法, 不要用短探针标定预算",
        "in scope: crates/search/src/embedding.rs, semantic.sqlite, sctx doctor",
        "排除掉与召回无关的重构, 关闭态必须与无此通道逐字节一致",
        "goal: make the semantic recall channel usable in a real session",
    ];
    let mut text = String::new();
    let mut index = 0;
    while text.chars().count() < length {
        if !text.is_empty() {
            text.push_str(" / ");
        }
        text.push_str(PHRASES[index % PHRASES.len()]);
        index += 1;
    }
    text.chars().take(length).collect()
}

fn percentile(sorted: &[Duration], fraction: f64) -> Duration {
    assert!(!sorted.is_empty(), "percentile of an empty sample");
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let rank = ((sorted.len() as f64 - 1.0) * fraction).round() as usize;
    sorted[rank.min(sorted.len() - 1)]
}

/// Encodes one text `SAMPLES` times after a short per-length warm-up, returning p50, p95 and max.
///
/// The warm-up is per length rather than per run: the graph is re-shaped for each sequence length,
/// so the first encode at a new length pays a cost no steady-state query at that length pays.
fn measure(provider: &dyn EmbeddingProvider, text: &str) -> (Duration, Duration, Duration) {
    for _ in 0..2 {
        provider.encode(text).expect("per-length warm-up encode");
    }
    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let started = Instant::now();
        provider.encode(text).expect("measured encode");
        samples.push(started.elapsed());
    }
    samples.sort_unstable();
    (
        percentile(&samples, 0.50),
        percentile(&samples, 0.95),
        *samples.last().expect("SAMPLES is non-zero"),
    )
}

/// Runs the ladder against one loaded provider and returns the p95 of the real-Intent length.
fn ladder(provider: &dyn EmbeddingProvider) -> Duration {
    // Warm-up. The first encode of a session pays one-time ORT graph and allocator costs that no
    // steady-state query pays, and folding it into the sample would inflate every percentile.
    let cold = Instant::now();
    provider.encode(&intent_query(283)).expect("warm-up encode");
    let cold = cold.elapsed();
    println!("\ncold first encode (283 chars): {} ms", cold.as_millis());
    println!(
        "budget under test: {} ms\n",
        SEMANTIC_ENCODE_BUDGET.as_millis()
    );
    println!(
        "{:>7}  {:>8}  {:>8}  {:>8}",
        "chars", "p50 ms", "p95 ms", "max ms"
    );

    let mut worst_real_intent = Duration::ZERO;
    for &length in LENGTHS {
        let text = intent_query(length);
        let (p50, p95, max) = measure(provider, &text);
        println!(
            "{length:>7}  {:>8}  {:>8}  {:>8}",
            p50.as_millis(),
            p95.as_millis(),
            max.as_millis()
        );
        if length == 283 {
            worst_real_intent = p95;
        }
    }
    println!();
    worst_real_intent
}

#[test]
#[ignore = "needs a real bge-m3 export; see the module docs"]
fn warm_encode_latency_by_query_length() {
    let Some((model, runtime)) = provider_paths() else {
        panic!("set SCTX_PROBE_EMBEDDING_MODEL and SCTX_PROBE_EMBEDDING_RUNTIME; see module docs");
    };
    let provider = load_onnx_provider(&model, &runtime).expect("load the configured bge-m3 export");

    let worst_real_intent = ladder(provider.as_ref());

    assert!(
        worst_real_intent <= SEMANTIC_ENCODE_BUDGET,
        "a warm 283-character Intent query took {} ms at p95, over the {} ms budget: the budget is \
         calibrated on the wrong query length and every automatic retrieval degrades to \
         embedding_unavailable",
        worst_real_intent.as_millis(),
        SEMANTIC_ENCODE_BUDGET.as_millis()
    );
}

/// The same ladder for the export `sctx embedding install` now installs by default.
///
/// A 0.6B decoder is a different order of model from bge-m3, and the encode budget is shared by
/// both: whichever family an operator has configured, [`SEMANTIC_ENCODE_BUDGET`] is what decides
/// whether their query is answered or silently dropped. So the budget's own assertion below is the
/// same one, unweakened -- if this family cannot answer a real-length Intent inside it on a quiet
/// machine, the number that has already been re-calibrated twice needs a third look rather than an
/// exception.
#[cfg(unix)]
#[test]
#[ignore = "needs a real F2LLM-v2-0.6B snapshot; see the module docs"]
fn warm_f2llm_encode_latency_by_query_length() {
    let before = resident_kilobytes();
    let loading = Instant::now();
    let provider = f2llm_snapshot::provider();
    let loading = loading.elapsed();
    let after = resident_kilobytes();
    println!(
        "\nload (flatten + runtime init + session): {} ms",
        loading.as_millis()
    );
    match (before, after) {
        (Some(before), Some(after)) => println!(
            "process RSS: {} MB before load, {} MB after ({} MB of weights and arenas)",
            before / 1024,
            after / 1024,
            after.saturating_sub(before) / 1024
        ),
        _ => println!("process RSS: unavailable on this platform"),
    }

    // Where the ladder's lengths actually land in tokens, because the budget's ceiling is a token
    // count and the two are not the same conversion for two tokenizers. bge-m3's XLM-R vocabulary
    // reaches [`SEMANTIC_MAX_TOKENS`] inside 1400 characters of this text; a byte-level BPE over
    // the same characters need not, and a ladder that stops short of the cap would report a
    // "ceiling" the constant can still be asked to beat.
    println!("\n{:>7}  {:>8}", "chars", "tokens");
    for &length in LENGTHS {
        let ids = provider
            .input_ids(&intent_query(length), TextRole::Query)
            .expect("tokenize a ladder query");
        println!("{length:>7}  {:>8}", ids.len());
    }

    let worst_real_intent = ladder(provider.as_ref());

    // The real ceiling: a query long enough that truncation is what decides the sequence length, so
    // this is the most expensive encode the budget can ever be asked for on this machine.
    let capped = intent_query(6_000);
    let ids = provider
        .input_ids(&capped, TextRole::Query)
        .expect("tokenize a query past the cap");
    assert_eq!(
        ids.len(),
        SEMANTIC_MAX_TOKENS,
        "the ceiling sample must actually be truncated, or it is measuring something shorter"
    );
    let (p50, p95, max) = measure(provider.as_ref(), &capped);
    println!(
        "\ntruncation ceiling ({SEMANTIC_MAX_TOKENS} tokens): p50 {} ms, p95 {} ms, max {} ms",
        p50.as_millis(),
        p95.as_millis(),
        max.as_millis()
    );

    if let Some(resident) = resident_kilobytes() {
        println!("process RSS after the ladder: {} MB", resident / 1024);
    }

    assert!(
        worst_real_intent <= SEMANTIC_ENCODE_BUDGET,
        "a warm 283-character Intent query took {} ms at p95, over the {} ms budget: the budget is \
         calibrated on the wrong model family and every automatic retrieval degrades to \
         embedding_unavailable",
        worst_real_intent.as_millis(),
        SEMANTIC_ENCODE_BUDGET.as_millis()
    );
}

/// This process's resident set, in kibibytes, or `None` when `ps` cannot say.
///
/// Read out of `ps` rather than the kernel directly because every in-process way to ask on macOS
/// goes through `libc` and `unsafe`, which this workspace forbids. It is a coarse number and is
/// reported as one: the point is the order of magnitude an operator has to have free, which is the
/// question `docs/user-guide.md` answers.
#[cfg(unix)]
fn resident_kilobytes() -> Option<u64> {
    let output = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()?;
    String::from_utf8(output.stdout)
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
}
