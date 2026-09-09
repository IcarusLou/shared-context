# R1–R4 requirement-to-evidence audit

Authority: [approved plan](approved-plan.md), amended only by [H-001 disposition](r2-1-compatibility-gate.md). Implementation decisions: [D-001–D-018](../decisions/refactor-r1-r4.md). Detailed tests and review dispositions: [execution log](execution.md). Mew remains the status system of record; [tracker snapshot](tracker.json).

| Requirement | Implementation / evidence | State |
|---|---|---|
| R1-1 model-less Codex SessionEnd | f539f9c; appended real five-key fixture, strict other events, CLI telemetry and lease cleanup; real M1/M3 host smokes | Accepted |
| R1-2 preserve ranked first occurrence | 7f26461; stable RRF dedup, graph order, immutable34-candidate replay | Accepted |
| R1-3 prefer DF>0 without deleting DF0 | 70a816b; direct scarcity/budget tests,36-intent replay, repeated ordinary/F2LLM ratchets | Accepted |
| R2-1 retire Claim fields under approved empty-only compatibility | 274170c; old-empty wire/byte retries, loud nonempty Data rejection, unchanged direct semantic/hash, empty Builder vectors and live explicit channel | Accepted |
| R2-2 dead surfaces / lazy reasons | 2921cce,7319bbc,d901d5a,fcfd7e0; retained historical table/files, generic hook outputs and schema; Full/Compact38-record byte comparison | Accepted |
| R2-3 quiet repeated close without skipping recovery | 4c0b935; both hosts, eight quiet repeats, pending/failed/older build recovery and first receipt | Accepted |
| R2-4 remove advisory payload | f1cd057; provisional markers/IDs and maintain authority preserved | Accepted |
| R2-5 one provisional predicate | 6613709; four production sites and shared domain/index/MCP/CLI fixture | Accepted |
| R2-6 N1 append-only source guidance | 85e0ee0; exact additions, later workflow sections unchanged, byte accounting verified | Accepted |
| R3-1 one complete triage body | cf4d9c2; description/ACK equality, restored criteria, workflow authority,8172/8180bytes and frozen input schemas | Accepted |
| R3-2 genuine evidence gaps and replay guard | 7e5bf7b; unresolved@4000 ready with evidence, blocking/Failed handling, unchanged permission guards | Accepted |
| R3-3 one additive18→19 migration | 13013db; old rows retained, strong default basis, durable relation through cleanup, relation/source disposition groups | Accepted |
| R4-1 SessionEnd missing verdict coverage | 63791bc; all owned Tasks, both hosts, exact identity/concurrency, no overwrite, strong upgrades, quiet bounded lock | Accepted code |
| R4-2a prior off and strong calibration only | c1f77c8; Full items equal apart from counters at reused3/ignored3, Compact order/reasons neutral; deferred#39 | Accepted code |
| R4-2b readonly recall stats | 6e3204a; live-WAL/current/old/missing source contracts, no source byte/sidecar/index writes, strong/weak and Task/Session aggregation | Accepted code |
| M1 / M2 / M3 full gates | Recorded accepted gate runs, actual targeted repairs/rechecks and explicit ignored/manual-model coverage in execution.md | Accepted |
| U-002 paired real-host smoke | cafe920; same-build real Cursor Agent CLI + Codex, actual MCP/identity/Builder/lease evidence and fail-closed pair verifier; [report](paired-real-host-smoke.md) | Accepted |
| M4 final code gates |1052passed/1fixturefail/14ignored;1396a5d repairs exact maintenance-lock race; whole scenario-runner41passed and independentobserver5passed. Finalfmt/clippy, NPM21passed/1skip | Accepted code gates |
| Installed M4 activation and baseline | Signed local build and actual-database copy upgrade preserve all23old table projections;9live old MCP clients require coordinated reconnection | [H-002 pending](r4-activation-gate.md) |
| Normal-use≥2weeks OR≥20sessions | Must start after activation; exclude historical IDs and isolated smokes; keep ranking and permissions stable | Pending |
| B-layer entry data report | Relation distribution, agent_policy attempts/refusals, coverage and strong reuse from observed normal-use cohort | Pending |

## Preserved boundaries

No event-v1 or ContextRevision/Draft schema migration; no direct checkpoint semantic/hash change; no clearing migration in18→19; no knowledge deletion. No new proximity_only column, no B1–B5 implementation or ADR-0005 revision. No usage-prior automatic activation:60%coverage and100strong samples only permit a new discussion. No reset credit was consumed and unavailable subagents were not represented as running.

Original failed gate invocations remain failures in the execution log. Targeted successful rechecks are identified separately; zero-test filters are not acceptance evidence. Full goal and M4 remain incomplete until installation/normal-use/report requirements above are fulfilled.
