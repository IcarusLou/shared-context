//! Lane hit-rate ratchet for ADR-0007's two lanes.
//!
//! The three probe suites beside this one measure the entry point an Agent types into: an explicit
//! `context_search`, and -- until ADR-0007 -- an automatic Pack reached by matching the Working
//! Intent text. The second half of that measurement went away with the mechanism, and what was left
//! was a negative ratchet: every automatic Pack in those fixtures is empty, and empty is not noisy.
//! True, and not enough. A retrieval system needs at least one test that says it *finds* something.
//!
//! So this file measures what the lanes can actually be measured on: ten probes from the extended
//! corpus, each one saying which files its Session opened. The corpus is the same
//! `probe-ext-v1.json`, because a second copy of twelve Contexts is a second thing to keep in step;
//! the footprints live in a `lane_probes` block the other suites do not read, which is what keeps
//! their own numbers -- and their own empty-Pack assertion -- exactly where they were.
//!
//! ## What is ratcheted, and why it is three numbers rather than one
//!
//! Lane A admits a Context because a file the Session opened is a file that Context is recorded
//! against. The seed expansion admits one because it restates the problem of a Context Lane A
//! already admitted. Lane B admits one because its document vector is close enough to a seed's.
//! Those are three different claims with three different failure modes, and the old single
//! `intent_hits` number could not tell which of them had moved. Each is counted on its own here,
//! and the noise count -- Contexts admitted that the probe did not ask for -- is the one that may
//! never rise above zero.
//!
//! ## Measured baseline (three release runs, bit-identical)
//!
//! `cargo test --release --locked -p sctx-cli --test association_probe_lane_workflow -- --nocapture`
//!
//! See the constants below for the numbers and the paragraph above each for how they were derived.
//! `associated` is 0 for a reason that is a property of the harness rather than of Lane B: these
//! suites run with `[retrieval]` unconfigured, so no embedding channel is attached, so the second
//! hop has no corpus of document vectors to compare against and never runs. Lane B's own end-to-end
//! ratchet is `lanes::hop2_ratchet` in `sctx-search`, which does have a model.

mod association_probe_harness;

use association_probe_harness::{LaneOutcome, build_harness, emit_lane_table, run_lane_probes};
use serde_json::Value;

const PROBE_FIXTURE: &str = include_str!("../../../fixtures/association/probe-ext-v1.json");

/// Contexts Lane A admitted across the ten probes, summed.
///
/// Eight of the ten probes touch a file some Context is recorded against, and one of those files --
/// `CronDispatcher.kt` -- is named by two Claims, the timezone drift and the disabled catch-up. So
/// eight probes produce nine admissions, and the two noise probes touch files no Context references
/// and correctly produce nothing. That a single file hands back both Contexts written about it is
/// the design rather than an overshoot: Lane A anchors at file granularity, and a Session that
/// opened the dispatcher has opened the file both findings are about.
const TARGET_ANCHORED: usize = 9;

/// Contexts the seed expansion admitted off an anchored one, summed.
///
/// Every Context in this corpus was checkpointed from its own Task, so no two share a
/// `problem_view` and the expansion has no edge to follow. That is a property of the fixture, not a
/// bound on the mechanism: `zero_anchor_knowledge_becomes_reachable_through_a_shared_problem_view`
/// in `sctx-search` is where the edge itself is proven. It is ratcheted at zero here so that a
/// change which starts pulling siblings into these Packs has to be looked at rather than absorbed.
const TARGET_EXPANDED: usize = 0;

fn fixture() -> Value {
    serde_json::from_str::<Value>(PROBE_FIXTURE).unwrap()
}

fn lane_probe() -> Vec<LaneOutcome> {
    let fixture = fixture();
    let mut harness = build_harness(&fixture);
    run_lane_probes(&mut harness, &fixture)
}

#[test]
fn lane_probe_ratchet() {
    let outcomes = lane_probe();
    let (anchored, expanded, associated, noise) = emit_lane_table(&outcomes);

    assert_eq!(
        noise,
        0,
        "a Context nobody asked for is worse than no Context: {:#?}",
        outcomes
            .iter()
            .filter(|outcome| !outcome.noise.is_empty())
            .map(|outcome| (&outcome.id, &outcome.noise))
            .collect::<Vec<_>>()
    );
    assert!(
        anchored >= TARGET_ANCHORED,
        "Lane A admitted {anchored} Contexts, below the ratcheted {TARGET_ANCHORED}"
    );
    assert_eq!(
        expanded, TARGET_EXPANDED,
        "the seed expansion admitted {expanded} Contexts in a corpus whose Contexts share no \
         problem: either the corpus grew a shared `problem_view`, or the edge widened"
    );
    assert_eq!(
        associated, 0,
        "no embedding channel is configured in these suites, so the second hop cannot run"
    );
}

#[test]
fn every_probe_that_touches_an_anchored_file_gets_exactly_what_it_asked_for() {
    for outcome in lane_probe() {
        let admitted = outcome.admitted();
        if outcome.category == "noise" {
            assert!(
                admitted.is_empty(),
                "noise probe {} touched {:?} and was handed {admitted:?}",
                outcome.id,
                outcome.touched
            );
            continue;
        }
        assert!(
            !admitted.is_empty(),
            "probe {} touched {:?} and got nothing back",
            outcome.id,
            outcome.touched
        );
        assert!(
            outcome.noise.is_empty(),
            "probe {} was handed {:?}, which it did not ask for",
            outcome.id,
            outcome.noise
        );
    }
}
