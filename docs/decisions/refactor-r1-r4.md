# Implementation decisions — R1–R4 refactor

Authoritative design: [approved plan](../refactor-r1-r4/approved-plan.md), supplied and approved 2026-09-09. Tracking: [execution record](../refactor-r1-r4/execution.md).

KD1–KD7 are approved design choices, not agent-made decisions. B1–B5, ADR-0005 revision, other deferred defects, destructive migrations, fixture profile changes and threshold recalibration remain out of scope.

## D-001 — Durable execution evidence
Unspecified question / design reference: the supplied plan lives in a temporary scratchpad; the skill requires durable decisions and tracker evidence.
Chosen approach: preserve a text copy of the approved plan and upstream audit/noise wording in docs/refactor-r1-r4; use existing Mew workspace as system of record, with local issue mapping and reviews.
Alternatives considered: temporary-only records; introduce another tracker.
Rationale and assumptions: Mew is the repository's existing tracker; local source copies survive temporary-file cleanup.
Tradeoffs / consequences: documentation copies are historical authority, not a second evolving design.
Affected issue and code: all milestones; documentation only.
Validation evidence: clean initial HEAD 0efcc1f; source hash recorded in approved-plan.md; Mew connectivity verified.
Status: agent-selected
Supersedes / superseded by: none.

## D-002 — Preserve model diagnostics at the event boundary
Unspecified question / design reference: R1-1/KD3 specifies optional Common.model but the committed diagnostic shape validator also requires model for every event; SessionEnd null and supplied-empty behavior is unspecified.
Chosen approach: require a nonempty model for the five model-bearing events in both shape and typed validation. SessionEnd accepts missing/null model (serde Option None), retains nonempty validation when a string is supplied, and rejects non-string values. Change only model validation; preserve the committed reason relaxation and closed diagnostics.
Alternatives considered: remove shape validation (would degrade field diagnostics); split per-event Common (explicitly deferred by KD3); accept blank supplied SessionEnd model (unneeded expansion of the prior contract).
Rationale and assumptions: the #34 fingerprint omits model entirely; both validation layers must allow that shape. Optional null follows the existing optional-string boundary without weakening required events.
Tradeoffs / consequences: a supplied blank SessionEnd model remains invalid; no fixture profile bump. Append a sanitized copy of the observed key fingerprint, retaining legacy fixture indexes.
Affected issue and code: R1-1; adapter-codex, append-only Codex fixture, CLI real-binary regression, deferred issue #34 and fixture-version discrepancy.
Validation evidence: `cargo test -p sctx-adapter-codex --locked` passed 25 payload contracts (unit/doc suites contain 0 tests); `cargo test -p sctx-cli --locked --test hook_fail_open --test hook_session_activation` passed 12 + 14 tests, including combined model-less SessionEnd success telemetry/lease cleanup and the existing supplied-model reason cleanup. Scoped rustfmt check and git diff --check passed.
Status: agent-selected
Supersedes / superseded by: none.

## D-003 — Preserve the graph caller's existing encounter order
Unspecified question / design reference: R1-2 requires callers to retain relevance order; inspection found `add_graph_channel` independently sorts/deduplicates its per-root BFS output before the shared helper. The plan does not define a new global ordering across artifact roots.
Chosen approach: remove the graph caller's redundant sort/dedup and let the shared helper retain the first occurrence from the existing traversal. Use a membership set without sorting the output; explicit references retain their existing sort/dedup.
Alternatives considered: leave the graph sort (would violate the caller-order invariant); globally reorder all roots by BFS depth (would introduce ranking policy outside R1-2).
Rationale and assumptions: each root already emits itself before its relation hops; preserving that sequence fixes the observed loss without changing traversal, safety checks, channel weights, relation rules, or thresholds.
Tradeoffs / consequences: rank follows first encounter across artifact/reference iteration, not minimum global graph depth; duplicated values do not consume a rank. Tests use fixed IDs opposing relevance order so random identity ordering cannot hide regression.
Affected issue and code: R1-2; `crates/search/src/candidate.rs`, `crates/search/tests/candidate_analysis.rs`.
Validation evidence: `cargo test --locked -p sctx-search` passed 130 tests with 8 explicitly ignored manual/model tests and 0 doc tests; includes adverse-order duplicate, real BM25, and graph root/hop guards. Scoped rustfmt and `cargo clippy --locked -p sctx-search --all-targets --all-features -- -D warnings` passed. Immutable 34-candidate replay (same corpus, budget 4096, top_k 16): exact_duplicate 17→17, unresolved_related 291→309, supports 11→11, revises 3→3, potential_contradiction 5→5, novel 1→1; total assessments 328→346. Analysis states changed from 33 complete / 1 failed (`analysis_invalid_input`) to 34 complete. These are observations, not a threshold or causal claim about the prior failure; supports/contradiction counts did not increase. Authoritative replay JSONL and summary: `/private/tmp/sctx-refactor-replay/after-r1-2-verified-candidates.jsonl`, `after-r1-2-verified-candidates-summary.json`; all three pristine database hashes still match the manifest. The earlier run launched before copy completion was discarded.
Status: agent-selected
Supersedes / superseded by: none.

## D-004 — Keep high-DF filtering at its existing point in token selection
Unspecified question / design reference: R1-3/KD2 changes the sort key before truncation; the existing high-DF filter between sorting and truncation also uses position to protect eight tokens.
Chosen approach: change only the key to DF-positive first, then ascending DF and the existing length/lexical tie-breakers. Keep high-DF filtering and its retained-prefix/universal rules in place. Add full-index regression coverage for budget survival, short-query DF-zero retention, and the high-DF prefix interaction.
Alternatives considered: reorder only after high-DF filtering (would preserve DF-zero displacement of the retained floor and introduce a second sort); remove DF-zero tokens (explicitly deferred by KD2).
Rationale and assumptions: the approved key change naturally lets positive-DF tokens occupy the protected prefix; no second algorithm or threshold change is needed. The filter still removes frequent terms beyond the prefix and universal terms independently of it.
Tradeoffs / consequences: when fewer than eight positive-DF tokens precede an ordinary high-DF term, it can now survive the retained floor despite many absent query tokens. DF-zero tokens remain selected when budget permits and continue to contribute to the selected denominator.
Ratchet disposition: three release runs retain ordinary search/intent 20/22 and fused 32/39, so those existing bounds stay unchanged. The semantic suite lexical control improves 27→29 solely through long_intent 1→3/3; add a narrow lexical long_intent 3/3 ratchet. Keep its LEXICAL_INTENT_HITS=27 binary-reference constant because this run did not remeasure that separate binary suite.
Affected issue and code: R1-3; search token selection, task_association regressions, and the F2LLM lexical long-intent acceptance ratchet.
Validation evidence: full `cargo test --locked -p sctx-search` passed 133 tests with 8 explicitly ignored manual/model tests; all three new token-selection regressions passed. Search all-target/all-feature warning-free clippy and scoped rustfmt/diff checks passed. Three sequential release `association_probe_workflow` runs each passed both tests at search 20/22 and intent 20/22 (zero noise). Three sequential release F2LLM runs with explicit `--ignored` each passed at lexical 29/39, fused 32/39 (zero noise); one further run passed after adding the long-intent ratchet. Logs: `/private/tmp/sctx-r1-3-probe-runs.json`, `/private/tmp/sctx-r1-3-search-full.log`, `/private/tmp/sctx-r1-3-ratchet-validation.log`. Immutable 36-intent replay changed mathematical ratio-insufficient requests 8→1 and actual `low_answerable_ratio` omissions 1→0; 23 intent summaries changed. This is an intent-only direct SearchEngine comparison with empty signals/no resolved focus and no semantic/usage-prior provider, not an exact original live-request replay. All three pristine database hashes match baseline. Frozen after binary and evidence: `/private/tmp/sctx-refactor-replay/after-r1-3-binary`, `after-r1-3-evidence.json`, `after-intents.jsonl`, `after-intents-summary.json`. The unrelated R1-1 adapter doc lint found through the CLI dependency check is assigned to separate M1-F1; final workspace lint remains a milestone gate.
Status: agent-selected
Supersedes / superseded by: none.
