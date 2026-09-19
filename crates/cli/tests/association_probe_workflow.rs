//! Association probe acceptance harness (WP-T), mixed Chinese/English corpus.
//!
//! `association_probe_baseline` locks in the identifier probes plus the noise
//! probe. `association_probe_target` encodes the association goal on the entry
//! point this fixture can still measure.
//!
//! Re-baselined for ADR-0007 (three release runs, bit-identical): explicit
//! search 20/22, unchanged from the fused stack -- `context_search` was not
//! touched and this is the assertion that proves it. Automatic retrieval reads
//! 0 lane A hits, 0 lane B hits and 0 noise across all 22 probes, because the
//! fixture drives `task_intent_update` with a goal string and nothing else: no
//! Workspace Signal, no resolved Focus, no path-shaped hint, therefore no
//! anchor, therefore no seed, therefore no second hop. The old `intent_hits >=
//! 18` floor counted hits on the intent-text channel ADR-0007 retired, so it is
//! replaced by the two things still true of this corpus -- every automatic Pack
//! is empty, and empty means empty rather than noisy.

mod association_probe_harness;

use association_probe_harness::{
    ProbeOutcome, assert_every_automatic_pack_is_empty, assert_no_noise, build_harness, emit,
    run_probes,
};
use serde_json::Value;

const PROBE_FIXTURE: &str = include_str!("../../../fixtures/association/probe-v1.json");
const REPORT_FILE_NAME: &str = "association-probe-report.json";

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

#[test]
fn association_probe_baseline() {
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
}

#[test]
fn association_probe_target() {
    let (_fixture, outcomes, search_hits, _intent_hits) = probe("target");

    let total = outcomes.len();
    assert!(
        search_hits >= 18,
        "explicit search associated only {search_hits}/{total} probes"
    );
    assert_every_automatic_pack_is_empty(&outcomes);
    assert_no_noise(&outcomes);
}
