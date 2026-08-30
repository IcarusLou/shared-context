//! Association probe acceptance harness (WP-T), mixed Chinese/English corpus.
//!
//! `association_probe_baseline` locks in the identifier probes plus the noise
//! probe. `association_probe_target` encodes the association goal (>= 18/22 on
//! both the explicit search and the automatic Task retrieval entry, with zero
//! noise hits) and is a blocking gate now that the retrieval work has landed.

mod association_probe_harness;

use association_probe_harness::{ProbeOutcome, assert_no_noise, build_harness, emit, run_probes};
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
    let (_fixture, outcomes, search_hits, intent_hits) = probe("target");

    let total = outcomes.len();
    assert!(
        search_hits >= 18,
        "explicit search associated only {search_hits}/{total} probes"
    );
    assert!(
        intent_hits >= 18,
        "task_intent_update associated only {intent_hits}/{total} probes"
    );
    assert_no_noise(&outcomes);
}
