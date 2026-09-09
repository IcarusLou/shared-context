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
