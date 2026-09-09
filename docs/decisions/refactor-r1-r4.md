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
