//! Measures what one real bge-m3 encode actually costs, by query length.
//!
//! T5b set [`sctx_search::SEMANTIC_ENCODE_BUDGET`] to 200 ms on the strength of ADR-0004's
//! "30--85 ms p95", a number produced by a torch prototype and never re-measured against the `ort`
//! stack this workspace ships. It was then validated with probe queries 15--40 characters long. A
//! real Working Intent query is `goal + current_direction + in_scope` concatenated: the Codex
//! session that exposed the defect built a 283-character query, and every one of its automatic
//! retrievals reported `embedding_unavailable`.
//!
//! This test exists so that constant is never again chosen without a measurement behind it. It is
//! `#[ignore]`d because it needs the operator's own model export:
//!
//! ```text
//! SCTX_PROBE_EMBEDDING_MODEL=~/.shared-context/embedding/model \
//! SCTX_PROBE_EMBEDDING_RUNTIME=~/.shared-context/embedding/runtime/libonnxruntime.dylib \
//!   cargo test -p sctx-search --test embedding_encode_latency -- --ignored --nocapture
//! ```
//!
//! It asserts nothing about absolute timings -- those are a property of the machine, not the code
//! -- and prints a table instead. The one thing it does assert is the property the budget has to
//! satisfy: a warm encode of a real-length Intent query must fit inside the configured budget.

#![cfg(feature = "embedding-onnx")]

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use sctx_search::{SEMANTIC_ENCODE_BUDGET, load_onnx_provider};

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

#[test]
#[ignore = "needs a real bge-m3 export; see the module docs"]
fn warm_encode_latency_by_query_length() {
    let Some((model, runtime)) = provider_paths() else {
        panic!("set SCTX_PROBE_EMBEDDING_MODEL and SCTX_PROBE_EMBEDDING_RUNTIME; see module docs");
    };
    let provider = load_onnx_provider(&model, &runtime).expect("load the configured bge-m3 export");

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
        // Two more encodes at this length before measuring: the graph is re-shaped per sequence
        // length, so each length gets its own small warm-up.
        for _ in 0..2 {
            provider.encode(&text).expect("per-length warm-up encode");
        }
        let mut samples = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            let started = Instant::now();
            provider.encode(&text).expect("measured encode");
            samples.push(started.elapsed());
        }
        samples.sort_unstable();
        let p50 = percentile(&samples, 0.50);
        let p95 = percentile(&samples, 0.95);
        let max = *samples.last().expect("SAMPLES is non-zero");
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

    assert!(
        worst_real_intent <= SEMANTIC_ENCODE_BUDGET,
        "a warm 283-character Intent query took {} ms at p95, over the {} ms budget: the budget is \
         calibrated on the wrong query length and every automatic retrieval degrades to \
         embedding_unavailable",
        worst_real_intent.as_millis(),
        SEMANTIC_ENCODE_BUDGET.as_millis()
    );
}
