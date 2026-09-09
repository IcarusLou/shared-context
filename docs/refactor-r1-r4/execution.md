# R1–R4 execution record

Authority: [approved plan](approved-plan.md). Decisions: [decision log](../decisions/refactor-r1-r4.md). Initial HEAD: `0efcc1f`; branch `feat/association-repair`; initial worktree clean. No applicable AGENTS.md found. Mew is the system of record; IDs are in tracker.json.

| Milestone | Outcome / invariant | Dependencies / forbidden early work | Issues in order | Status |
|---|---|---|---|---|
| M1 | SessionEnd cleanup works; relevance rank survives; DF>0 tokens survive budget | None; no M2–M4 implementation | R1-1, R1-2, R1-3 | backlog |
| M2 | Remove only approved dead surfaces, quiet duplicate closure, preserve recovery and compatibility | M1 accepted + real-session smoke; no M3–M4 | R2-1, R2-2 (split by independent outcome), R2-3, R2-4, R2-5, R2-6 | backlog |
| M3 | Triage single source, evidence semantics, additive audit migration | M2 accepted; no M4 writers before migration acceptance | R3-1, R3-2, R3-3 | backlog |
| M4 | All injections judged with evidence basis; prior off; truthful stats | M3 accepted + real-session smoke; no B-layer changes | R4-1, R4-2, observation report | backlog |

Each milestone requires exact diff review, targeted tests re-run by main agent, affected crate full tests, formatting and warning-free workspace clippy. Final gates additionally include workspace tests, packaging and relevant privacy/performance tests. Acceptance details and non-goals are the corresponding authoritative plan items and issues.json. No routine human gate between milestones. Human gates only for materially revised scope, conflicting acceptance, newly deferred work, destructive action, or tradeoffs requiring judgment. Main agent reviews every implementation decision before acceptance; clean worktree and verified commits required.

M4 observation is required unfinished work until normal use reaches ≥2 weeks or ≥20 sessions and produces the specified B-entry report; synthetic tests cannot substitute. B-layer implementation remains separate backlog.

## Reviews and evidence

Pending.
