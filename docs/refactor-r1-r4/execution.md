# R1–R4 execution record

Authority: [approved plan](approved-plan.md). Decisions: [decision log](../decisions/refactor-r1-r4.md). Initial HEAD: `0efcc1f`; branch `feat/association-repair`; initial worktree clean. No applicable AGENTS.md found. Mew is the system of record; IDs are in tracker.json.

| Milestone | Outcome / invariant | Dependencies / forbidden early work | Issues in order | Status |
|---|---|---|---|---|
| M1 | SessionEnd cleanup works; relevance rank survives; DF>0 tokens survive budget | None; no M2–M4 implementation | R1-1, R1-2, R1-3 (+ M1-F1/F2/F3 repairs) | accepted |
| M2 | Remove only approved dead surfaces, quiet duplicate closure, preserve recovery and compatibility | M1 accepted + real-session smoke; no M3–M4 | R2-1, R2-2a → R2-2b → R2-2c → R2-2d (parent R2-2), R2-3, R2-4, R2-5, R2-6 | doing |
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

### H-001 resolved (2026-09-09, planning-session review on user's behalf)

Disposition is recorded in full in [the gate record's "Disposition" section](r2-1-compatibility-gate.md): the recommended `LegacyClaimFields` compatibility carrier is **rejected** — the decisive new fact is that all five fields are empty across every persisted Claim in the real installation (element-count sum 0 over 16 checkpoints), so the carrier would protect provably nonexistent data, reconstructing the chain audit's bloat pattern ① inside the item meant to delete it. The **empty-only tombstone adapter is approved with the fresh authorization it required**, under five binding points: empty-required wire DTO with a loud typed error on nonempty; tombstone empty-key serialization preserved in checkpoint_json and fat semantic_json (retry byte-equality and old-binary rollback hold); field deletion in active CheckpointClaim/Draft with build_claim_material supplying empty Vecs; nonempty test fixtures rewritten (explicitly authorized); direct semantic JSON, operation hashing, ContextRevision, and event-v1 schema untouched. Acceptance list in the gate record replaces the nonempty-survival item with loud-rejection. R2-1 may resume implementation under these terms; R1-3 disposition (replay, ratchet, clippy, commit) proceeds independently per its own pending review.


### R1-3 — accepted atomic issue
Commit reviewed: `70a816b431ab91e9d16b2207ee18d4a82d1a0ab0` (Mew #237). Four owned files, source key change + direct regression + measured lexical ratchet + D-004 only. Main inspected exact diff: DF0 preserved when room, nonzero rarity and tie-breakers preserved, high-DF filtering position unchanged, no tokenizer/index/version/threshold changes. Independently reran two absent-token tests (2 passed) and high-DF prefix test (1 passed). Executor full search133 passed/8 ignored, scoped lint and three repeated probe suites plus extra ratchet run passed. Fixed36-intent replay8→1 ratio-insufficient /1→0 actual omissions; limits recorded in replay-evidence.md. ROI-1 none; ROI-2 earlier doc lint tracked separately in M1-F1; ROI-3 none. D-004 complete and consistent. Human gate none after U-001. M1 full gates pending.

### M1-F1 — accepted review repair
Commit reviewed: `2b664c2d44201a9721054499122c1238a8a23106` (supersedes55b19a1 before later dependencies). Main verified four documentation-only lines across adapter source and CLI test. Initial full workspace lint exposed one additional test-comment warning; executor amended the current repair, then full workspace lint passed; main independently reran workspace lint successfully. No behavior change/test rerun needed for Markdown backticks. ROI-1/2/3 none remaining for this repair. No new implementation decision.

### M1-F2 — accepted review repair and real-host smoke
Commit reviewed: `c04bd68431679238b0ca1ae8983943a0d540b2f0`. Scope only probe JSON/current-marker validation (7insertions/1deletion); D-005 records decision. Main inspected exact diff and real completed MCP trace: one accepted direct Checkpoint, replayed=false, zero extra tools/errors; summary confirms unchanged Knowledge Git/no capture residue. Main further invoked Stop and model-less SessionEnd on same isolated product session: Buildercomplete, one candidate, lease directoryempty. No statistical quality claim from a single smoke; no M4 normal-use sample claim. Initial marker-check and sandbox attempts are not passes. Evidence `/private/tmp/sctx-refactor-m1-host-smoke-verified/{summary,lifecycle-smoke}.json` and raw/. ROI-1/2/3 none. M1 full CLI gate remains pending.


### M1-F3 — accepted fixture repair
Commit reviewed: `4254d0e1d08bcec21c971b928be98827d9078921` (Mew #258). Full CLI initial run152passed/4failed/6ignored exposed one stale helper cardinality assertion; all4 failures were in that same acceptance target. Exact diff changes only expected Codex cardinality7 vs Cursor6 and a comment, preserving original indexes and disabled traversal of the extra fixture. Executor targeted6passed; main independently reran the entire CLI suite, including these6 tests, successfully. ROI-1/2/3 none; no runtime/fixture content change and no new implementation choice.

### M1 accepted — automatic advance to M2
Required atomic commits R1-1/R1-2/R1-3 and repairs M1-F1/F2/F3 verified. Final gates: `cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` exit0; full CLI `cargo test --locked -p sctx-cli --no-fail-fast -- --test-threads=1` exit0,156passed/0failed/6ignored across41 suites. Logs `/private/tmp/sctx-m1-clippy-accepted.log`, `/private/tmp/sctx-m1-cli-accepted.log`. Prior failed runs are diagnostic evidence only, not passes. Final search133passed/8ignored and adapter25passed cover their unchanged behavioral source; ignored real F2LLM suite separately passed three times plus ratchet verification. Hook hot-path/fail-open/privacy tests are included in full CLI. NPM packaging `npm test`:21passed/0failed/1platform skip (`/private/tmp/sctx-m1-npm-test.log`). Real model/MCP→Checkpoint→Builder→SessionEnd isolated smoke passed as recorded above. RRF34-candidate and DF36-intent immutable replays documented in replay-evidence.md. Decision log D-002–D-005 + user U-001 current. No open ROI-1/required ROI-2 or human gate. Governance commit leaves clean worktree before dispatch. Mew M1 done, M2/R2-1 doing; future milestones remain backlog. H-001 empty-only ruling applies exactly, not the rejected carrier proposal.

### R2-1 — accepted under U-001
Commit reviewed: `274170cc8bee77ae2da6ca3c6f513540f20a4fba` (supersedes88c9bc5 before any dependent issue). Ten owned files; active Claim/Draft fields/parameters removed, strict empty-only private decoder, borrowed old-order serializer/fat semantic tombstones, empty Builder fields, live engineering references and explicit Search wiring preserved. Direct semantic/hash functions and Context/event types untouched; main independently compared source and inspected exact diff. Authorized nonempty fixture rewrites and two necessary serialized-empty CLI assertions are scoped consequences.
Main independently reran: domain old-wire bytes/old reader + negative empty-field tests (2passed), runtime old fat open/closed retry (1passed), direct semantic/hash lib oracle (1passed), MCP old-empty history→retry→derivation→Builder/analysis (1passed). An initial wrong integration filter for the lib hash test ran0tests and was discarded; the corrected lib invocation ran1. Executor full domain/runtime/MCP/Git/event300passed/0failed/0ignored, edited CLI13passed, workspace lint/fmt passed. Logs and summary `/private/tmp/sctx-r2-1-gates.json`, source invariance `/private/tmp/sctx-r2-1-source-invariance.json`.
ROI-1 none. ROI-2 resolved: zero-length array string decoding misclassified nonempty JSON as syntax; D-007 now explicitly rejects as Data with a clear empty-field error. Frozen old reader missed relations default; amended test now matches original default and proves omitted-field restoration. ROI-3 none. D-006 decoder explicitly superseded; borrowed serializer retained and D-007 evidence complete. H-001's approved empty-only policy followed; no carrier or nonempty preservation. Clean worktree before next dispatch; Mew #238 done, R2-2 parent/first child doing. Next: automatic R2-2a.

### R2-2a — accepted atomic issue
Commit reviewed: `2921cceba1065d5a459156630b358d67ee72a52c`. One file/four sites: remove always-None internal field/two literal initializers and forward original additional_context; system_message fallback and every runtime action/recovery branch unchanged. Executor lifecycle7 + signal14 passed; main independently reran hook_task_signals14passed. Exact diff and clean index verified. Scoped lint/fmt passed. ROI-1/2/3 none, no agent-selected policy change. Next automatic R2-2b; parent R2-2 remains doing.
