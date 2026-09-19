# Dynamic Replay Phase One

Status: non-blocking test evidence for Mew #171/#176
Scenario suite: `9c4f505`
Audited aggregate: [`tests/reports/dynamic-replay-phase-one-v1.json`](../tests/reports/dynamic-replay-phase-one-v1.json)

## What phase one proves

Phase one executes six hand-written, synthetic, privacy-reviewed scenarios against the real local
`sctx` binary in a runner-owned isolated sandbox. It does not replay a real session. The scenarios
cover Codex and Cursor normal Candidate confirmation, PreCompact resume under the same Task,
missing-Hook empty-close fallback, TurnStop/restart recovery, and same-Workspace dual-Session
isolation.

Local real-session material influenced only human-reviewed aggregate pacing: event density,
relative lifecycle ordering, compaction position, missing-Hook position, and concurrency position.
No Prompt, transcript, tool output, path, user identity, Session identity, domain ID, or per-session
event sequence is a test input or committed artifact. The one-off local aggregation helper and its
intermediate output were deleted and were not committed.

Codex `0.147.0` and Cursor `3.13.2` are exact fixture profiles. They are not a product support
matrix, an Agent compatibility claim, or evidence about another version.

The default suite keeps the canonical fixture checks and one deterministic execution of each of
the six scenarios as blocking test coverage. The 6×20 replay is additive non-blocking evidence; it
is explicitly ignored by default and is not a per-commit or merge gate. Mew #176 changes no
product logic, product Schema, Agent Adapter, scenario contract semantics, or scenario fixture.

## Sanitized report contract

`ReplayReport` projects scenario parsing, runner completion, and closed assertions onto an
allowlist. A report item contains only a validated synthetic scenario name, seed, duration,
step/assertion counts, disposition, closed classification, safe diagnostic code, and semantic
digest. It never retains `RunOutcome.variables`, dynamic IDs, step/assertion names, raw action
responses, RunnerFailure messages, stdout/stderr, filesystem paths, Prompt, transcript, tool
output, canary text, or canary hashes.

The semantic digest covers the validated scenario semantics and raw-free execution/assertion
shape. It excludes duration and runtime-generated identities. Timing therefore cannot change the
digest, while a changed contract, step status, or assertion result does. Timing in the committed
stage summary is rounded and aggregated; there are no per-run rows, timestamps, host identifiers,
or seed-to-timing pairs.

Every result is non-blocking evidence with one of these outcomes:

- `Passed` disposition: the run completed and every closed assertion passed, with no declared
  expected-failure step.
- `invalid_scenario`: JSON is readable but the contract or runner preflight is invalid.
- `unsupported_version`: the scenario Schema or contract version is outside the supported
  phase-one contract.
- `corrupt_data`: the scenario document is invalid or truncated JSON.
- `expected_fail_open`: the declared typed failure occurred, the run continued, and all closed
  assertions passed. This is an expected result, not a product failure.
- `infrastructure_flake`: a valid scenario encountered a runner process, timeout, observer, or
  outcome-transport failure.
- `product_invariant_violation`: the run completed and at least one closed `AssertionRecord` was
  false.

Classification uses typed contract/runner/assertion state. It never parses a free-form error
message. A corrupt scenario, runner failure, or false assertion becomes another sanitized report
item; the harness neither edits product code nor creates or updates a Mew Issue.

## Phase-one result

The explicit acceptance run used all six scenarios with seeds `0..19`, for 120 isolated runs:

- 120 completed and zero failed;
- 100 used the `Passed` disposition;
- 20 Codex stale-CAS runs were `expected_fail_open` and completed their downstream assertions;
- `invalid_scenario`, `unsupported_version`, `corrupt_data`, `infrastructure_flake`, and
  `product_invariant_violation` were all zero;
- rounded aggregate runtime was 221,600 ms, with rounded per-run minimum 900 ms and maximum
  4,100 ms;
- semantic digest:
  `sha256:5e44981537a083fd7d4093fb752c524da71fd38e75b6ff621d27f5bcdbd4cfae`.

The machine-readable aggregate is the authoritative stage summary. It contains no individual
run, dynamic identity, path, output, or log.

## Product-fix gate

A replay finding may authorize a new production-fix Issue only when all four conditions are true:

1. The exact fixture profile is in the declared phase-one supported test set.
2. The result reproduces stably and is not invalid, corrupt, or an infrastructure failure.
3. It breaks a named closed invariant on the primary flow and is ROI-1.
4. A human explicitly approves a new atomic production Issue.

Before that approval there is no product edit, compatibility branch, exit-policy promotion, or
automatically created fix. The report does not make product decisions.

## Blocking evidence that remains authoritative

The hand-authored `fixtures/m4/fixed-oracle.json`, `hook_to_confirm_chain`, the workspace Rust/NPM
gates, and the accepted #150 boundary for untracked files remain blocking. Replay cannot override,
weaken, or replace them. Existing M4 acceptance remains the product gate.

## Explicitly deferred

The following are not implemented or proven by phase one. Each requires a new Issue, privacy and
design review, and fresh human approval:

- replay of an actual Shared Context user session;
- a collection, export, or sanitization pipeline for real Session data;
- model-in-the-loop execution;
- a multi-version Agent matrix beyond the exact fixture profiles;
- promotion of long replay to a per-commit or merge-blocking gate.

## Commands

The default deterministic scenario execution remains part of ordinary workspace tests. The long
run is explicit:

```bash
cargo test -p sctx-cli --test replay_report \
  phase_one_replay_runs_all_six_scenarios_twenty_times --locked -- \
  --ignored --exact --nocapture --test-threads=1
```

Phase-one report logic and the real-runner smoke remain ordinary tests:

```bash
cargo test -p sctx-scenario-runner --locked
cargo test -p sctx-cli --test replay_report --locked
```
