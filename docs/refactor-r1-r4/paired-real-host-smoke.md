# U-002 paired real-host smoke acceptance

User authority: “cursor也需要真实的冒烟测试，并且作为后续测试规则，如果有Codex的真实冒烟测试，那就必须得有Cursor。” The former either-host rule is superseded in the plan, its original HTML and DEVELOPMENT.md. Future gates use [the paired entry point](../../tests/scripts/README.md). This record supplements historical Codex-only M1/M3 evidence on the final product build; it does not falsely claim Cursor ran at those historical commit boundaries.

| Evidence | Codex | Cursor |
|---|---|---|
| Real installed host | Codex CLI0.153.4 | Cursor Agent CLI2026.08.31-4057e58 |
| Reported model | gpt-5.6-luna | Auto (underlying routed model not disclosed) |
| Real model business calls | task_checkpoint, accepted/not replayed | task_checkpoint, accepted/not replayed; candidate_list |
| Builder | Complete;1novel candidate | Complete;1novel candidate via normal candidate_list recovery |
| Session identity | Explicit harness Task/Session, used by real MCP call | Actual native Cursor session ID, preserved through hook/bootstrap/MCP/end |
| Lifecycle delivery | Driver Stop,8quiet repeats, model-less SessionEnd | Native sessionStart/postToolUse/sessionEnd; native stop not observed |
| Lease cleanup | Empty | Empty after native SessionEnd |
| Desktop UI claim | None | None |

Both hosts used SHA256 `0d7ac23be579e01132c6f4c9e6b786c5bef02e372deef854fb66619d5e5fb10a`; checkpoint schema hash `fe13a5df7adc7e891236fedb519e58d52ec3c34ca4a1ea35bcbdf8cabe4eaa5a`,17public tools. [Machine-readable paired result](paired-real-host-smoke.json). These are isolated synthetic acceptance samples, not normal-use observations or statistical quality claims.

Authoritative private traces:
- `/private/tmp/sctx-refactor-paired-codex-real/`: raw model output, summary and explicitly driven lifecycle result.
- `/private/tmp/sctx-refactor-cursor-real-03/`: raw host stream, transparent MCP input/output capture, native-hook payload/results and final summary.
- `/private/tmp/sctx-refactor-paired-host-acceptance/paired-summary.json`: independently revalidated pair.

Cursor diagnostics remain separate: real-01 verified only checkpoint/native end and left Builder pending because its prompt stopped there; it is not full-gate evidence. Real-02 completed the product chain but exposed an overly restrictive harness read filter: the real host reads its installed Skill. The final fixture explicitly permits only that private SKILL.md/workflow.md, and real-03 passes with those reads recorded. Metadata discovery is distinguished from business calls. No product Rust behavior was changed to make these tests pass.

Negative verifier checks passed: missing Cursor rejected; failed Cursor rejected even under Python optimization; mismatched build rejected; a failed rerun clears a previous green receipt. Python syntax checks and positive pair verification passed. H-002 production activation remains unapproved and unchanged.


## 2026-09-10 — default live entry point also executed

In response to the user's question about actual script execution, the default `real_host_smoke_pair.py` path was run end to end (not `--verify-only`). It sequentially launched real Cursor and Codex, drove the explicitly labeled Codex lifecycle, and completed pair verification with process exit0. [Result](paired-real-host-smoke-full-entry.json); raw output `/private/tmp/sctx-pair-full-entry-20260910-01/`, command log `/private/tmp/sctx-pair-full-entry-20260910-01.log`.

Cursor again made the two expected real MCP calls, completed1novel candidate and natively cleared its lease on SessionEnd. Codex again submitted1accepted checkpoint; Builder and driven close/repeat/end checks passed. Both used the same signed binary and checkpoint schema as the earlier pair. This closes the earlier evidence gap: the default orchestration entry had previously been composed from separately run hosts and tested only with `--verify-only`. Desktop UI and native Cursor stop remain outside the observed coverage. No script or product-code change was needed for this full-entry run.
