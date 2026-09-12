//! Proves an interactive encode is not starved by the corpus backfill that runs beside it, and
//! that the backfill is not starved in return.
//!
//! Every encode in the process is serialised through one ONNX session, and the corpus backfill is
//! a loop of the most expensive encodes there are: on this machine a full-length corpus encode is
//! about 1060 ms at p95. So this is not a fairness problem that yielding between corpus texts can
//! solve -- an interactive encode that queued behind one would pay all of that -- and the repair
//! aborts the corpus run in flight when an interactive encode arrives. `SessionGate` is what
//! implements it and this is what measures it.
//!
//! What arrives interactively is smaller than it was. ADR-0007 retired the query path, so the
//! interactive side is no longer a Pack asking a question every second; it is `sctx embedding
//! status --verify`, the calibration suites, and anything else that calls
//! `EmbeddingProvider::encode` while a backfill is running. The property is unchanged and the
//! numbers are still the numbers, but there is no budget left for it to be measured against --
//! these ladders print what the contention costs rather than asserting a constant that no longer
//! exists.
//!
//! Two phases, because a fix for either half alone is easy and wrong:
//!
//! * **Contended** -- an interactive encode every 150 ms, far past any real arrival rate.
//!   Measures what a real 283-character text costs while the backfill runs. A fix that made
//!   interactive encodes fast by never backfilling would pass this and leave the corpus
//!   permanently unembedded.
//! * **Realistic** -- one every 1.5 s. Measures whether the backfill still gets through corpus
//!   texts. A fix that made the backfill fast by ignoring interactive encodes would pass this and
//!   starve the other side.
//!
//! `#[ignore]`d because it needs the operator's own model export:
//!
//! ```text
//! SCTX_PROBE_EMBEDDING_MODEL=~/.shared-context/embedding/model \
//! SCTX_PROBE_EMBEDDING_RUNTIME=~/.shared-context/embedding/runtime/libonnxruntime.dylib \
//!   cargo test -p sctx-search --test embedding_backfill_contention --release -- --ignored --nocapture
//! ```

#![cfg(feature = "embedding-onnx")]

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use sctx_search::{EmbeddingProvider, load_onnx_provider};

/// Characters in each corpus text the backfill encodes. Past the truncation ceiling, so it is the
/// most expensive encode the backfill can issue and the worst case a query could wait behind.
const CORPUS_CHARS: usize = 1400;

/// Characters in each measured interactive encode. The length of the real Working Intent query
/// that exposed the budget defect T5d repaired, kept so this run is comparable with that one.
const QUERY_CHARS: usize = 283;

/// Queries in the contended phase. Enough that a p95 is a measurement rather than the worst of
/// three.
const CONTENDED_QUERIES: usize = 16;

/// Gap between arrivals in the contended phase: three queries a second, well past any real rate.
const CONTENDED_INTERVAL: Duration = Duration::from_millis(150);

/// Queries in the realistic phase.
const REALISTIC_QUERIES: usize = 5;

/// Gap between arrivals in the realistic phase, about the fastest an agent session issues
/// automatic Packs.
const REALISTIC_INTERVAL: Duration = Duration::from_millis(1_500);

/// Corpus texts the backfill must get through in the realistic phase.
///
/// One per query interval would be four; two is the floor that still distinguishes "the backfill
/// is making progress" from "the backfill only ever escapes through its starvation deadline".
const REALISTIC_CORPUS_FLOOR: usize = 2;

fn provider_paths() -> Option<(PathBuf, PathBuf)> {
    let model = std::env::var_os("SCTX_PROBE_EMBEDDING_MODEL")?;
    let runtime = std::env::var_os("SCTX_PROBE_EMBEDDING_RUNTIME")?;
    Some((PathBuf::from(model), PathBuf::from(runtime)))
}

/// Builds text of roughly `length` characters in the shape retrieval actually submits, salted with
/// `salt` so no two queries in one run are the same string. The salt matters: the channel's query
/// vector cache would otherwise serve every query after the first for free and measure nothing.
fn intent_text(length: usize, salt: usize) -> String {
    const PHRASES: &[&str] = &[
        "把嵌入通道的编码预算按真实 Intent 长度重新标定",
        "current direction: 先测量再决定改法, 不要用短探针标定预算",
        "in scope: crates/search/src/embedding.rs, semantic.sqlite, sctx doctor",
        "排除掉与召回无关的重构, 关闭态必须与无此通道逐字节一致",
        "goal: make the semantic recall channel usable in a real session",
    ];
    let mut text = format!("run {salt}: ");
    let mut index = salt;
    while text.chars().count() < length {
        text.push_str(" / ");
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

/// What one phase of interactive-encodes-against-a-backfill cost, on both sides.
struct Phase {
    p50: Duration,
    p95: Duration,
    max: Duration,
    failed: usize,
    queries: usize,
    corpus_encodes: usize,
    samples: Vec<Duration>,
}

/// Runs `queries` queries `interval` apart while a backfill loop encodes corpus texts, and reports
/// what both sides got.
fn run_phase(
    provider: &Arc<dyn EmbeddingProvider>,
    queries: usize,
    interval: Duration,
    salt_base: usize,
) -> Phase {
    let stop = Arc::new(AtomicBool::new(false));
    let encoded = Arc::new(AtomicUsize::new(0));
    let backfill = {
        let provider = Arc::clone(provider);
        let stop = Arc::clone(&stop);
        let encoded = Arc::clone(&encoded);
        std::thread::Builder::new()
            .name("contention-backfill".to_owned())
            .spawn(move || {
                let mut salt = salt_base + 10_000;
                while !stop.load(Ordering::Relaxed) {
                    let text = intent_text(CORPUS_CHARS, salt);
                    // The same call `load_and_backfill` makes. Measuring `encode` here would
                    // measure a backfill this workspace does not run.
                    if provider.encode_bulk(&text).is_ok() {
                        encoded.fetch_add(1, Ordering::Relaxed);
                    }
                    salt += 1;
                }
            })
            .expect("spawn the backfill thread")
    };

    let mut samples = Vec::with_capacity(queries);
    let mut failed = 0_usize;
    for salt in 0..queries {
        std::thread::sleep(interval);
        let query = intent_text(QUERY_CHARS, salt_base + salt);
        let started = Instant::now();
        let outcome = provider.encode(&query);
        samples.push(started.elapsed());
        if outcome.is_err() {
            failed += 1;
        }
    }
    stop.store(true, Ordering::Relaxed);
    backfill.join().expect("join the backfill thread");

    let mut sorted = samples.clone();
    sorted.sort_unstable();
    Phase {
        p50: percentile(&sorted, 0.50),
        p95: percentile(&sorted, 0.95),
        max: *sorted.last().expect("a phase runs at least one query"),
        failed,
        queries,
        corpus_encodes: encoded.load(Ordering::Relaxed),
        samples,
    }
}

fn report(name: &str, phase: &Phase) {
    println!(
        "\n{name}: {} queries of {QUERY_CHARS} chars against a backfill of {CORPUS_CHARS}-char \
         corpus texts",
        phase.queries
    );
    println!(
        "corpus texts the backfill completed: {}",
        phase.corpus_encodes
    );
    println!(
        "{:>8}  {:>8}  {:>8}  {:>11}",
        "p50 ms", "p95 ms", "max ms", "failed"
    );
    println!(
        "{:>8}  {:>8}  {:>8}  {:>8}/{}",
        phase.p50.as_millis(),
        phase.p95.as_millis(),
        phase.max.as_millis(),
        phase.failed,
        phase.queries
    );
    println!(
        "per-query ms: {:?}",
        phase
            .samples
            .iter()
            .map(Duration::as_millis)
            .collect::<Vec<_>>()
    );
}

#[test]
#[ignore = "needs a real bge-m3 export; see the module docs"]
fn a_backfill_and_a_query_each_get_what_they_need() {
    let Some((model, runtime)) = provider_paths() else {
        panic!("set SCTX_PROBE_EMBEDDING_MODEL and SCTX_PROBE_EMBEDDING_RUNTIME; see module docs");
    };
    let provider = load_onnx_provider(&model, &runtime).expect("load the configured bge-m3 export");

    // Warm-up: the first encode of a session and the first encode at each sequence length pay
    // one-time ORT costs no steady-state query pays.
    for length in [QUERY_CHARS, CORPUS_CHARS] {
        for salt in 0..2 {
            provider
                .encode(&intent_text(length, salt))
                .expect("warm-up encode");
        }
    }

    let contended = run_phase(&provider, CONTENDED_QUERIES, CONTENDED_INTERVAL, 0);
    report("contended (150 ms apart)", &contended);

    let realistic = run_phase(&provider, REALISTIC_QUERIES, REALISTIC_INTERVAL, 1_000);
    report("realistic (1.5 s apart)", &realistic);
    println!();

    // No budget to compare against any more: ADR-0007 retired the path that had one. What is
    // still a property rather than a reading is that every interactive encode *completed* --
    // preemption aborts the corpus run, and an implementation that reported the abort to the
    // interactive caller instead would show up here.
    assert_eq!(
        contended.failed, 0,
        "{} of {CONTENDED_QUERIES} interactive encodes failed while the corpus backfilled: \
         preemption is being charged to the wrong side",
        contended.failed
    );
    // The other half of the property. Starving the backfill would make every assertion above pass
    // and leave the corpus permanently unembedded, which is a worse failure than the transient one
    // being repaired.
    assert!(
        realistic.corpus_encodes >= REALISTIC_CORPUS_FLOOR,
        "the backfill completed only {} corpus texts against {REALISTIC_QUERIES} queries \
         {REALISTIC_INTERVAL:?} apart: yielding has become starving the backfill, and \
         the corpus would take days to embed on a session that is merely in use",
        realistic.corpus_encodes
    );
}
