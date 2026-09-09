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
