//! Extended association probe set (T5a), report-only + ratchet blocking.
//!
//! `probe-ext-v1` adds 12 corpus Contexts across six unrelated domains (push
//! notification dedupe/token refresh, image upload cache/retry, cron
//! scheduler timezone/misfire, English-language settlement reconciliation,
//! English-language feature-flag cache, storage quota) and 36 probes, each
//! tagged with a `category`:
//!
//! * `paraphrase` (9) -- same-language synonym rewrites that avoid the
//!   stored wording.
//! * `cross_lingual` (5) -- English queries against Chinese Contexts and
//!   Chinese queries against the two English Contexts.
//! * `near_duplicate` (5) -- two corpus pairs share a surface symptom
//!   ("重复推送" / "上传失败") with different root causes; the probes ask for
//!   one member specifically (`dup-05` is genuinely ambiguous between the
//!   pair and lists both as an allowed set).
//! * `negation` (4) -- polarity-flipped phrasing ("如果不...", "就算没有...")
//!   that must still resolve to the same Context.
//! * `identifier` (5) -- literal class names / config keys / dotted paths.
//! * `multi_hop` (3) -- the answer requires either following a
//!   `ContextRelation` (`depends_on` from the metric-spike discovery to its
//!   root cause, `supersedes` from the rounding fix to the original drift)
//!   or a shared identifier across two Contexts that never share a relation
//!   edge (`TaskQueueExecutorPool` in both the misfire discovery and the
//!   starvation issue). Each lists both plausible Contexts as an allowed
//!   set, since only the current lexical channel is under test.
//! * `noise` (5) -- topics with zero lexical overlap with any of the three
//!   probe fixtures; `assert_no_noise` requires these to return nothing on
//!   both entry points.
//!
//! This is deliberately **not** a pass/fair-target gate like the two
//! existing probe tests. WP-T's own measurements (see `docs/` acceptance
//! notes referenced from the association-repair branch) show the current
//! lexical channel cannot resolve `paraphrase` (no shared tokens),
//! `near_duplicate` (term-frequency favors the wrong pair member) or
//! `negation` (polarity is invisible to a bag-of-tokens match) -- those are
//! exactly the gaps ADR-0004's embedding channel exists to close. Setting a
//! fixed target here would either be trivially true (too low to catch a
//! regression) or immediately fail (too high for what lexical search can
//! do). Instead this test ratchets: it only guards against *regressing*
//! below the worst of three independent measurements taken while writing
//! this fixture, so future channel changes can be graded against the
//! per-category table this test prints on every run.
//!
//! Measured on 2026-09-02, three consecutive runs against
//! `feat/association-repair @ 5b422c1` (lexical channel only, no embedding
//! work landed). The three runs were bit-for-bit identical -- this retrieval
//! path is deterministic, no flakiness was observed:
//!
//! | run | search hits / 36 | `task_intent_update` hits / 36 |
//! |-----|-------------------|---------------------------------|
//! |  1  | 27                | 26                              |
//! |  2  | 27                | 26                              |
//! |  3  | 27                | 26                              |
//!
//! Worst (= only) observed: search 27/36, `task_intent_update` 26/36. The
//! blocking thresholds below are pinned to those values -- they only catch a
//! regression, they do not encode a progress target. Do not raise them
//! without re-measuring; do not lower them without recording why.
//!
//! Per-category hit counts (both entry points; identical across all three
//! runs):
//!
//! | category          | count | search hits | intent hits |
//! |--------------------|-------|--------------|--------------|
//! | `paraphrase`       | 9     | 8            | 8            |
//! | `cross_lingual`    | 5     | 1            | 1            |
//! | `near_duplicate`   | 5     | 5            | 5            |
//! | `negation`         | 4     | 1            | 1            |
//! | `identifier`       | 5     | 5            | 4            |
//! | `multi_hop`        | 3     | 2            | 2            |
//! | `noise`            | 5     | 5 (0 leaks)  | 5 (0 leaks)  |
//!
//! This matches the qualitative failure modes WP-T already knew about:
//! `negation` and `cross_lingual` are almost entirely unresolved by a
//! lexical channel (1/4 and 1/5), `paraphrase` gets partial credit only
//! where the rewrite still shares some tokens with the stored text (8/9),
//! and `near_duplicate` -- despite the shared-surface distractor design --
//! scores perfectly here because the disambiguating tokens in each probe
//! happen to be more specific to one pair member than the other; that is
//! not guaranteed to hold once an embedding channel changes the scoring.
//! `identifier` is the one category expected to stay near-ceiling under any
//! channel, since a class name or config key is close to a literal-match
//! query. These are exactly the per-category deltas the embedding channel
//! (ADR-0004) should move once it lands.

mod association_probe_harness;

use std::collections::BTreeMap;

use association_probe_harness::{ProbeOutcome, assert_no_noise, build_harness, emit, run_probes};
use serde_json::Value;

const PROBE_FIXTURE: &str = include_str!("../../../fixtures/association/probe-ext-v1.json");
const REPORT_FILE_NAME: &str = "association-probe-ext-report.json";

/// Ratcheted floor: the worst (here, only, since all three runs matched) of
/// three measured runs. Regression-only, not a progress target -- see the
/// module doc comment for the measured numbers.
const TARGET_SEARCH_HITS: usize = 27;
const TARGET_INTENT_HITS: usize = 26;

fn fixture() -> Value {
    serde_json::from_str::<Value>(PROBE_FIXTURE).unwrap()
}

fn probe(mode: &str) -> (Value, Vec<ProbeOutcome>, usize, usize) {
    let fixture = fixture();
    let mut harness = build_harness(&fixture);
    let outcomes = run_probes(&mut harness, &fixture);
    let (search_hits, intent_hits) = emit(REPORT_FILE_NAME, mode, &fixture, &outcomes);
    (fixture, outcomes, search_hits, intent_hits)
}

/// Prints one row per category: how many probes carry it and how many of
/// those hit on each entry point. This is the table the embedding channel
/// (ADR-0004) should be graded against once it lands -- a category-level
/// regression or improvement should show up here even while the blocking
/// thresholds above stay pinned to the lexical-only baseline.
fn print_category_table(outcomes: &[ProbeOutcome]) {
    let mut by_category: BTreeMap<&str, (usize, usize, usize)> = BTreeMap::new();
    for outcome in outcomes {
        let entry = by_category.entry(outcome.category.as_str()).or_default();
        entry.0 += 1;
        entry.1 += usize::from(outcome.search_hit);
        entry.2 += usize::from(outcome.intent_hit);
    }
    println!("\n--- association-probe-ext: per-category hits ---");
    println!(
        "{:<16} {:>6} {:>13} {:>13}",
        "category", "count", "search_hits", "intent_hits"
    );
    for (category, (count, search_hits, intent_hits)) in &by_category {
        println!("{category:<16} {count:>6} {search_hits:>13} {intent_hits:>13}");
    }
}

#[test]
fn association_probe_ext_baseline() {
    let (_fixture, outcomes, _search_hits, _intent_hits) = probe("baseline");

    for outcome in &outcomes {
        assert!(
            outcome.category != "identifier" || outcome.search_hit,
            "identifier probe {} ({}) lost its explicit-search hit: top1={:?} expected={:?}",
            outcome.id,
            outcome.query,
            outcome.search_top1,
            outcome.expected
        );
    }
    assert_no_noise(&outcomes);
    print_category_table(&outcomes);
}

#[test]
fn association_probe_ext_ratchet() {
    let (_fixture, outcomes, search_hits, intent_hits) = probe("target");

    let total = outcomes.len();
    print_category_table(&outcomes);

    assert!(
        search_hits >= TARGET_SEARCH_HITS,
        "explicit search associated only {search_hits}/{total} probes, \
         below the ratcheted floor of {TARGET_SEARCH_HITS}"
    );
    assert!(
        intent_hits >= TARGET_INTENT_HITS,
        "task_intent_update associated only {intent_hits}/{total} probes, \
         below the ratcheted floor of {TARGET_INTENT_HITS}"
    );
    assert_no_noise(&outcomes);
}
