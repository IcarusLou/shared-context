//! Measures what one real encode actually costs, by text length, for each model family.
//!
//! It used to hold the encode budget honest. T5b set that budget to 200 ms on the strength of
//! ADR-0004's "30--85 ms p95", a number produced by a torch prototype and never re-measured
//! against the `ort` stack this workspace ships, and validated with probe queries 15--40
//! characters long; a real Working Intent query was 283 characters and every automatic retrieval
//! built from one reported `embedding_unavailable`. ADR-0007 then retired the query path
//! altogether, so there is no budget left to assert against and these two ladders assert nothing:
//! they are the measurement, printed.
//!
//! What they are still for is the two questions an operator and a backfill actually ask. How long
//! does one corpus encode take on this machine, which is what decides how long a first backfill
//! runs; and how much memory and load time does the export cost, which is what
//! `docs/user-guide.md` quotes to someone deciding whether to install it at all.
//!
//! ## Which entry point the ladders time, and why that changed on 2026-09-12
//!
//! The ladders are `encode_bulk` -- the Document role, the call the corpus backfill makes for every
//! revision. They used to be `encode`, the Query role, which was the right choice while the query
//! path existed and the wrong one the moment the module's own purpose became the backfill's cost:
//! for a Qwen3-family export the two are not the same encode at all, because the query role
//! prepends an instruction and therefore tokens the backfill never pays for. Timing `encode` while
//! claiming to answer "how long does a first backfill run" overstated the answer by whatever that
//! prefix costs.
//!
//! One interactive reading is kept beside each ladder, at the 283-character length every earlier
//! number in `docs/user-guide.md` is quoted at. `EmbeddingProvider::encode` still has exactly one
//! production caller -- `sctx embedding status --verify` -- and ADR-0007's amendment keeps it
//! deliberately, so what it costs is still worth one line.
//!
//! Both tests are `#[ignore]`d because they need a real model export:
//!
//! ```text
//! SCTX_PROBE_EMBEDDING_MODEL=~/.shared-context/embedding/model \
//! SCTX_PROBE_EMBEDDING_RUNTIME=~/.shared-context/embedding/runtime/libonnxruntime.dylib \
//!   cargo test --release -p sctx-search --test embedding_encode_latency -- --ignored --nocapture
//!
//! SCTX_PROBE_F2LLM_MODEL=~/.shared-context/embedding/model \
//! SCTX_PROBE_EMBEDDING_RUNTIME=~/.shared-context/embedding/runtime/libonnxruntime.dylib \
//!   cargo test --release -p sctx-search --test embedding_encode_latency -- --ignored --nocapture
//! ```
//!
//! Two ladders over the same lengths: one for whatever export `SCTX_PROBE_EMBEDDING_MODEL` names,
//! which is how an operator measures the model they have installed, and one for an F2LLM snapshot
//! reached through `SCTX_PROBE_F2LLM_MODEL`, which additionally reports the model load, the token
//! ladder and the process's resident size. Pointing both variables at one directory runs both over
//! one export, which is a cross-check rather than a comparison; two different exports is the
//! comparison, and the encoders do not cost the same.

#![cfg(feature = "embedding-onnx")]

#[cfg(unix)]
mod f2llm_snapshot;

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use sctx_search::{EmbeddingProvider, load_onnx_provider};
#[cfg(unix)]
use sctx_search::{embedding::SEMANTIC_MAX_TOKENS, embedding::onnx::TextRole};

/// Samples per length. Enough that a p95 is a measurement rather than the single worst of three.
const SAMPLES: usize = 24;

/// Text lengths, in characters, spanning the probe suite through a real Intent to the truncation
/// ceiling. 283 is the length measured on Codex session 01a06646, kept because every earlier
/// reading in `docs/user-guide.md` is quoted at it.
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

/// Which of the provider's two entry points is being timed.
///
/// Not a `TextRole`: the role is the provider's internal business, and what a caller picks is a
/// method. Keeping the distinction at the method is what makes this a measurement of a production
/// path rather than of a parameter -- `encode_bulk` is what the backfill calls, prefix or no
/// prefix, and a family that stops distinguishing the two would make these two rows agree by
/// itself.
#[derive(Clone, Copy)]
enum Entry {
    /// `encode_bulk`, the Document role: every revision the corpus backfill writes.
    Backfill,
    /// `encode`, the Query role: `sctx embedding status --verify` and the calibration suites.
    Interactive,
}

impl Entry {
    fn encode(self, provider: &dyn EmbeddingProvider, text: &str) {
        match self {
            Self::Backfill => provider.encode_bulk(text).expect("corpus encode"),
            Self::Interactive => provider.encode(text).expect("interactive encode"),
        };
    }
}

/// Encodes one text `SAMPLES` times after a short per-length warm-up, returning p50, p95 and max.
///
/// The warm-up is per length rather than per run: the graph is re-shaped for each sequence length,
/// so the first encode at a new length pays a cost no steady-state encode at that length pays.
fn measure(
    provider: &dyn EmbeddingProvider,
    text: &str,
    entry: Entry,
) -> (Duration, Duration, Duration) {
    for _ in 0..2 {
        entry.encode(provider, text);
    }
    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let started = Instant::now();
        entry.encode(provider, text);
        samples.push(started.elapsed());
    }
    samples.sort_unstable();
    (
        percentile(&samples, 0.50),
        percentile(&samples, 0.95),
        *samples.last().expect("SAMPLES is non-zero"),
    )
}

/// Runs the backfill ladder against one loaded provider and returns the p95 of the real-Intent
/// length, having also printed what one interactive encode of that same text costs.
fn ladder(provider: &dyn EmbeddingProvider) -> Duration {
    // Warm-up. The first encode of a session pays one-time ORT graph and allocator costs that no
    // steady-state encode pays, and folding it into the sample would inflate every percentile.
    let cold = Instant::now();
    Entry::Backfill.encode(provider, &intent_query(283));
    let cold = cold.elapsed();
    println!("\ncold first encode (283 chars): {} ms", cold.as_millis());
    println!();
    println!(
        "{:>7}  {:>8}  {:>8}  {:>8}",
        "chars", "p50 ms", "p95 ms", "max ms"
    );

    let mut worst_real_intent = Duration::ZERO;
    for &length in LENGTHS {
        let text = intent_query(length);
        let (p50, p95, max) = measure(provider, &text, Entry::Backfill);
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

    // The one surviving interactive caller, at the one length every earlier reading is quoted at.
    // Printed beside the ladder rather than as a ladder of its own: what it is for is telling an
    // operator whether `status --verify` is instant, and the difference between the two rows is
    // what the query instruction costs on this family.
    let (p50, p95, max) = measure(provider, &intent_query(283), Entry::Interactive);
    println!(
        "\ninteractive encode (283 chars, `status --verify`): p50 {} ms, p95 {} ms, max {} ms",
        p50.as_millis(),
        p95.as_millis(),
        max.as_millis()
    );
    println!();
    worst_real_intent
}

/// The ladder for whatever export `SCTX_PROBE_EMBEDDING_MODEL` names, which is how an operator
/// measures the model they have actually installed.
#[test]
#[ignore = "needs a real model export; see the module docs"]
fn warm_encode_latency_by_text_length() {
    let Some((model, runtime)) = provider_paths() else {
        panic!("set SCTX_PROBE_EMBEDDING_MODEL and SCTX_PROBE_EMBEDDING_RUNTIME; see module docs");
    };
    let provider =
        load_onnx_provider(&model, &runtime).expect("load the export named by the environment");
    println!(
        "export {} -- {} dimensions",
        model.display(),
        provider.dimensions()
    );

    let worst_real_intent = ladder(provider.as_ref());
    println!("283-character p95: {} ms", worst_real_intent.as_millis());
}

/// The same ladder for the export `sctx embedding install` now installs by default, plus the load
/// time and resident size `docs/user-guide.md` quotes.
#[cfg(unix)]
#[test]
#[ignore = "needs a real F2LLM-v2-0.6B snapshot; see the module docs"]
fn warm_f2llm_encode_latency_by_text_length() {
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

    // Where the ladder's lengths actually land in tokens, because [`SEMANTIC_MAX_TOKENS`] is a
    // token count and the conversion is not the same for two tokenizers. bge-m3's XLM-R vocabulary
    // reaches the cap inside 1400 characters of this text; a byte-level BPE over the same
    // characters need not, and a ladder that stops short of the cap would report a "ceiling" that
    // is really the end of the table. Both roles, because on this family they differ by the
    // instruction prefix and the ladder below pays only the document one.
    println!(
        "\n{:>7}  {:>10}  {:>10}",
        "chars", "doc tokens", "qry tokens"
    );
    for &length in LENGTHS {
        let text = intent_query(length);
        let document = provider
            .input_ids(&text, TextRole::Document)
            .expect("tokenize a ladder text as a document");
        let query = provider
            .input_ids(&text, TextRole::Query)
            .expect("tokenize a ladder text as a query");
        println!("{length:>7}  {:>10}  {:>10}", document.len(), query.len());
    }

    let worst_real_intent = ladder(provider.as_ref());

    // The real ceiling: a text long enough that truncation is what decides the sequence length, so
    // this is the most expensive encode the backfill can ever be asked for on this machine.
    let capped = intent_query(6_000);
    let ids = provider
        .input_ids(&capped, TextRole::Document)
        .expect("tokenize a text past the cap");
    assert_eq!(
        ids.len(),
        SEMANTIC_MAX_TOKENS,
        "the ceiling sample must actually be truncated, or it is measuring something shorter"
    );
    let (p50, p95, max) = measure(provider.as_ref(), &capped, Entry::Backfill);
    println!(
        "\ntruncation ceiling ({SEMANTIC_MAX_TOKENS} tokens): p50 {} ms, p95 {} ms, max {} ms",
        p50.as_millis(),
        p95.as_millis(),
        max.as_millis()
    );

    if let Some(resident) = resident_kilobytes() {
        println!("process RSS after the ladder: {} MB", resident / 1024);
    }
    println!("283-character p95: {} ms", worst_real_intent.as_millis());
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
