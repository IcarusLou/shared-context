# R1–R4 execution record

Authority: [approved plan](approved-plan.md). Decisions: [decision log](../decisions/refactor-r1-r4.md). Initial HEAD: `0efcc1f`; branch `feat/association-repair`; initial worktree clean. No applicable AGENTS.md found. Mew is the system of record; IDs are in tracker.json.

| Milestone | Outcome / invariant | Dependencies / forbidden early work | Issues in order | Status |
|---|---|---|---|---|
| M1 | SessionEnd cleanup works; relevance rank survives; DF>0 tokens survive budget | None; no M2–M4 implementation | R1-1, R1-2, R1-3 | doing |
| M2 | Remove only approved dead surfaces, quiet duplicate closure, preserve recovery and compatibility | M1 accepted + real-session smoke; no M3–M4 | R2-1, R2-2a → R2-2b → R2-2c → R2-2d (parent R2-2), R2-3, R2-4, R2-5, R2-6 | backlog |
| M3 | Triage single source, evidence semantics, additive audit migration | M2 accepted; no M4 writers before migration acceptance | R3-1, R3-2, R3-3 | backlog |
| M4 | All injections judged with evidence basis; prior off; truthful stats | M3 accepted + real-session smoke; no B-layer changes | R4-1, R4-2a, R4-2b (parent R4-2), R4-observation | backlog |

Each milestone requires exact diff review, targeted tests re-run by main agent, affected crate full tests, formatting and warning-free workspace clippy. Final gates additionally include workspace tests, packaging and relevant privacy/performance tests. Acceptance details and non-goals are the corresponding authoritative plan items and issues.json. No routine human gate between milestones. Human gates only for materially revised scope, conflicting acceptance, newly deferred work, destructive action, or tradeoffs requiring judgment. Main agent reviews every implementation decision before acceptance; clean worktree and verified commits required.

M4 observation is required unfinished work until normal use reaches ≥2 weeks or ≥20 sessions and produces the specified B-entry report; synthetic tests cannot substitute. B-layer implementation remains separate backlog.

## Reviews and evidence

Baseline: `cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` passed on 2026-09-09. Clippy log: `/private/tmp/sctx-refactor-baseline-clippy.log`. R2-2 is tracked as four independent atomic outcomes (Mew #249–252); this is issue decomposition, not expanded scope. Implementation reviews pending.


### R1-1 — accepted atomic issue
Commit reviewed: `f539f9c20d78b5ee5c23d43e14854de0658286b4` (Mew #235).
Scope verified: six files, optional model decoding plus append-only fixture/tests and #34 documentation; no profile bump, unrelated diagnostics unchanged. Checked both shape validator and typed event match, supplied-empty/wrong-type boundaries and all five strict events; existing fixture indices unchanged.
Direct tests independently re-run: adapter payload_contract 25 passed; CLI hook_fail_open::codex_model_less_session_end_reaches_cleanup_and_success_telemetry 1 passed (11 filtered). Executor additionally ran full hook_fail_open 12 + hook_session_activation 14.
Decision log: D-002 consistent with code and approved KD3; null handling explicitly recorded.
ROI-1: none. ROI-2: none. ROI-3: none.
Unproven requirements: full M1 workspace/affected-crate gates and cross-milestone real-host smoke remain pending; not claimed by atomic acceptance.
Worktree/tracker: staged index empty after executor commit; only main-owned execution/issue decomposition documents pending. Mew #235 done after main verification, #236 doing.
Human gate required: no. Next action: automatically advance to R1-2.


### R1-2 — accepted atomic issue
Commit reviewed: `7f2646162492b80e572b3cf3b42f8238e4fab4c5` (Mew #236).
Scope verified: three files; membership-only stable dedup, removal of graph caller sort, adversarial-ID tests, D-003. Traced every add_ranked_channel caller: explicit retains sort/dedup, unordered channels already iterate ordered maps; graph traversal, safety, weights, seven-rule relation ladder and all thresholds unchanged. No global depth policy introduced.
Direct tests independently re-run: ranked_channel_keeps_input_order_and_first_duplicate_position 1 passed (22 filtered); full candidate_analysis 19 passed, including real BM25 and graph adverse-ID order. Executor full search: 130 passed / 8 ignored / 0 failed, search clippy clean. Main inspected exact commit and empty staging and verified snapshot hashes. Numeric replay and limitations: [evidence](replay-evidence.md).
Decision-log coverage: D-003 matches code; first occurrences consume compacted unique ranks and graph remains per-root BFS.
ROI-1: none. ROI-2: none. ROI-3: none.
Unproven requirements: R1-3 and full M1 gates/smoke remain pending.
Worktree/tracker: only main-owned issue decomposition and replay evidence pending before this review record; #236 done after independent verification; #237 doing.
Human gate required: no. Next action: automatically advance to R1-3.


### Human gate H-001 — R2-1 persisted compatibility (pending)

Status: all implementation/delegation paused; existing read-only R1-3 probe process may finish. R1-1 and R1-2 remain accepted commits. R1-3 has an uncommitted three-file implementation (search/lib.rs, task_association.rs, D-004), full search133 passed/8 ignored and three ordinary probe repetitions20/22 each; semantic repetitions/replay/final audit pending. No M2 implementation began.

ROI-1: literal five-field deletion rejects existing checkpoint_json under deny_unknown_fields; empty-only compatibility would discard/reject valid nonempty positive fixtures that production Builder/analyzer consume. Removing low-level semantic JSON keys also changes exact retry equality and old-binary rollback behavior. Evidence and concrete alternatives: [H-001 review and proposed rescope](r2-1-compatibility-gate.md).

Recommended decision: approve R2-1 as new-authoring field retirement plus a strict private legacy compatibility carrier, preserving historical values/Builder behavior, serialized empty keys for new Claims, and low-level semantic byte compatibility. This retains compatibility code/consumers the pure-deletion plan intended to remove. Alternative: defer R2-1, retain its current implementation, proceed with remaining approved work after gate resolution. Neither option authorizes ContextRevision/event schema changes, data deletion, explicit-channel deletion or any other deferred issue.

Authority required: review-gated-development requires fresh approval for a materially revised plan and instructs stopping all implementation/delegation at the human gate. No choice has yet been approved. Tracker R/M1/R1-3/R2-1 set to review while the gate is pending; future items remain backlog.


H-001 evidence update: the main agent independently passed the existing positive domain serialization and runtime checkpoint retry tests (one test each); details in the gate record. Already-running R1-3 tests finished without further implementation: all three release ordinary runs search20/22 + intent20/22, all three F2LLM runs lexical29/39 + fused32/39, zero noise, explicit ignored-test selection. All agents and test runners are now stopped. After-intent replay, R1-3 clippy/ratchet disposition/commit and main review remain pending user decision. Goal remains active and incomplete.
