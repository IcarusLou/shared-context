//! Chinese-first association probe acceptance harness (WP-L7, WP-L8).
//!
//! The knowledge base defaults to Chinese prose (statement / rationale /
//! evidence summary / Task intent), with identifiers and paths kept verbatim.
//! `probe-zh-v1` stores eight such Contexts -- including one near-duplicate
//! distractor that shares the "comment input bar" surface but has an unrelated
//! cause -- and asks them back with 24 probes: 16 Chinese rewrites that avoid the
//! stored wording, 4 identifier probes, 2 English probes and 2 noise probes.
//!
//! `association_probe_zh_baseline` locks in what must never regress (identifier
//! probes hit, noise probes return nothing). `association_probe_zh_target`
//! holds the association goal, >= 19/24 on both entry points with zero noise
//! hits, and is blocking since WP-L8.
//!
//! WP-L8 moved the two retrieval-side causes and left the third:
//!
//! * The ranked coverage denominator now counts only the query tokens the Tree
//!   can answer at all, so a Han bigram that names nothing in the corpus no
//!   longer holds a rewritten Chinese question under the truncation floor
//!   (`zh-04`, `zh-09`, `zh-13` recovered; explicit search 17 -> 20/24).
//! * Identifier-derived query tokens count several times over in the automatic
//!   text-coverage gate, which is the only channel an English question has
//!   against Chinese prose (`en-01`, `en-02` recovered), and the injection
//!   ranking now scales each Space score by how much of the query the Context
//!   itself answered, so a short near-duplicate no longer outranks the Context
//!   that covers the question (`zh-08` recovered; automatic 16 -> 19/24).
//! * Rewrites that share no token at all with the stored text (`zh-01`,
//!   `zh-02`, `zh-06`, `zh-07`) remain out of reach for a lexical channel: the
//!   surviving misses land on a Context that really does share the query's
//!   only shared tokens. Closing them needs an embedding channel (P2.5), not
//!   another lexical threshold -- adding `jieba-rs` word-level tokens on top of
//!   the Han bigrams (both `cut` and `cut_for_search`) reproduced the previous
//!   numbers probe for probe.

mod association_probe_harness;

use association_probe_harness::{ProbeOutcome, assert_no_noise, build_harness, emit, run_probes};
use serde_json::Value;

const PROBE_FIXTURE: &str = include_str!("../../../fixtures/association/probe-zh-v1.json");
const REPORT_FILE_NAME: &str = "association-probe-zh-report.json";
const TARGET_HITS: usize = 19;

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
fn association_probe_zh_baseline() {
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
fn association_probe_zh_target() {
    let (_fixture, outcomes, search_hits, intent_hits) = probe("target");

    let total = outcomes.len();
    assert!(
        search_hits >= TARGET_HITS,
        "explicit search associated only {search_hits}/{total} probes"
    );
    assert!(
        intent_hits >= TARGET_HITS,
        "task_intent_update associated only {intent_hits}/{total} probes"
    );
    assert_no_noise(&outcomes);
}
