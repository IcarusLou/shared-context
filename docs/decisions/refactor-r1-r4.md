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

## U-001 — User-approved R2-1 disposition (supersedes the proposed carrier)
Authority: user message points to the updated [H-001 Disposition](../refactor-r1-r4/r2-1-compatibility-gate.md#disposition-2026-09-09-reviewed-and-ruled--h-001-closed), committed in 501305a. The empty-only tombstone adapter is explicitly approved; LegacyClaimFields/nonempty-history preservation and deferring R2-1 are rejected. Retired nonempty values must fail loudly; test-fixture rewrites are authorized. Empty wire keys and fat retry semantic literals remain, direct semantics/hashes and ContextRevision/event schema stay untouched. This is a user-approved scope amendment, not an agent-selected compatibility policy.

## D-005 — Validate the current activation marker in real-host smoke
Unspecified question / design reference: M1 requires real-host smoke, but the existing probe searched raw JSON for an obsolete un-attributed activation tag.
Chosen approach: parse the JSON response, read hookSpecificOutput.additionalContext, and validate a complete marker carrying the trial's exact escaped session ID.
Alternatives considered: loosen to a raw prefix substring (does not prove correct session identity); change runtime marker (outside scope and wrong source of defect).
Rationale and assumptions: the runtime already emits the current attributed marker; this is a test-harness defect, not a product authorization failure.
Tradeoffs / consequences: malformed or mismatched markers still fail; no runtime/protocol change. A synthetic smoke is not an M4 normal-use observation.
Affected issue and code: M1-F2, tests/scripts/codex_checkpoint_model_probe.py.
Validation evidence: one real Codex0.153.4/gpt-5.6-luna session completed exactly one accepted task_checkpoint with zero other tools; main inspected raw completed MCP response and summary, then drove Stop/SessionEnd through the same isolated product root (Builder complete, one candidate, lease directory empty). Files: /private/tmp/sctx-refactor-m1-host-smoke-verified/{summary,lifecycle-smoke}.json. Initial stale-marker and restricted-process attempts were failures and are not counted as successful smoke.
Status: agent-selected
Supersedes / superseded by: none.

## D-006 — Empty wire DTO with borrowed serialization
Unspecified question / design reference: the R2-1 final Disposition requires empty-only historical keys and active field deletion, without prescribing the serde implementation.
Chosen approach: deserialize through a private strict DTO whose retired fields are zero-length typed arrays, preserving the original required fields and the relations default. Move active fields into Claim. Serialize active fields by reference with SerializeStruct and five empty-array literals in the original field order; retain optional engineering-reference omission and fat semantic empty keys.
Alternatives considered: owned serde `into` conversion (clones the Claim on serialization); a second borrowed serialize DTO (duplicates the wire declaration); a generic legacy adapter or compatibility carrier (unnecessary, and the carrier is explicitly rejected).
Rationale and assumptions: zero-length arrays enforce empty input without allocating or retaining legacy values; direct serialization preserves old bytes without cloning. Current real data and the approved fixture rewrite use empty retired arrays.
Tradeoffs / consequences: valid nonempty old authoring fixtures now fail on read as explicitly approved, while empty history, old-reader rollback, and retry bytes stay compatible. The original four required vectors remain required; missing relations retains its original empty default. No ContextRevision/event/direct-operation contract changes.
Affected issue and code: R2-1; domain Claim serde/constructor, runtime ClaimDraft/materialization/fat semantics, MCP Builder/analyzer wiring, and directly affected fixtures/compatibility tests.
Validation evidence: the frozen old Claim byte oracle and direct semantic oracle each passed against the original implementation before field deletion. Final serializer/runtime compatibility and full gates are recorded in D-007; the initial fixed-array decoding was rejected by real JSON-string integration evidence.
Status: superseded (decoder only; serializer choice retained)
Supersedes / superseded by: D-007; zero-length serde arrays misclassify valid nonempty JSON as syntax errors. the empty-only policy is user-approved U-001, not this agent-selected implementation choice.


## D-007 — Explicit typed rejection of retired array contents
Unspecified question / design reference: R2-1 requires loud typed nonempty rejection; D-006 fixed-length array decoding returns an opaque syntax error for well-formed nonempty JSON strings, despite returning a data error through serde_json::Value.
Chosen approach: retain the strict private DTO and borrowed serializer; decode its five retired arrays through one narrow sequence visitor. An empty sequence returns a zero-length array; the first nonempty element returns a custom data error stating that retired checkpoint Claim fields must be empty. No value is stored or silently accepted. Preserve missing-relations default and the four required keys.
Alternatives considered: typed Vec fields plus TryFrom validation (valid but temporarily stores unsupported input); accept generic trailing-characters errors (does not satisfy the approved error contract).
Rationale and assumptions: a local sequence visitor distinguishes valid-but-forbidden data from malformed JSON consistently for both persisted strings and in-memory serde values, without heap allocation for the retired fields.
Tradeoffs / consequences: one small serde helper replaces reliance on fixed-array decoder behavior; no active compatibility fields or general-purpose legacy framework is added.
Affected issue and code: R2-1 domain Claim wire decoder and negative domain/runtime regression assertions.
Validation evidence: original runtime integration reproduced `parse Agent Checkpoint history: trailing characters at line 1 column 188`; main independently reproduced Syntax classification. Replacement domain tests pin Data errors and the explicit empty-field diagnostic for each retired key through Value and JSON-string decoding; the runtime integration pins InvariantViolation with the same diagnostic. Literal old-empty checkpoint bytes survive read, direct retry, derivation rewrite, Builder and analysis; a frozen old wire reader accepts new writes. Fat semantic byte/open-and-closed-retry tests and the frozen direct semantic/hash oracle pass. Full no-fail-fast domain/task-runtime/MCP/Git-store/event-schema tests: 300 passed, 0 failed, 0 ignored (5 doc suites with 0 tests). Edited CLI lifecycle and two acceptance targets: 13 passed, 0 failed. Scoped rustfmt and full workspace all-target/all-feature clippy passed. Logs: `/private/tmp/sctx-r2-1-full-tests.log`, `sctx-r2-1-cli-lifecycle.log`, `sctx-r2-1-cli-acceptance.log`, `sctx-r2-1-workspace-clippy.log`; source-invariance audit confirms direct semantic/hash implementations, event schema and Context types unchanged. Additional CLI assertions reading the removed artifact_refs field were converted to assertions of its empty serialized tombstone; no behavior edit was needed. Main review also corrected the frozen test reader to retain the original relations default and pinned omission/re-emission behavior; both domain wire tests and scoped formatting passed after that test-only repair (`/private/tmp/sctx-r2-1-old-reader-review-fix.log`).
Status: agent-selected
Supersedes / superseded by: supersedes D-006 decoder choice; its borrowed serializer/order choice remains in force.

## D-008 — Prove the retained legacy diagnostic table directly
Unspecified question / design reference: R2-2b removes table APIs but preserves schema 18, the migration chain and legacy rows; the existing schema 13 migration test uses the removed APIs for its final table-usability proof.
Chosen approach: replace that final proof with a named-column SQL sentinel insert, reopen the Runtime, then compare all eight stored values. Keep earlier migration/data-preservation assertions. Remove only API-specific storage/retention tests; keep reminder and checkpoint compatibility tests.
Alternatives considered: drop the final proof (weaker table preservation evidence); retain dead public APIs for the fixture (defeats the approved cleanup); add a second migration-only fixture (duplicates existing coverage).
Rationale and assumptions: direct SQL tests the compatibility surface that remains after API removal, including reopening without discarding history. The issue explicitly requires retaining the live decision enum, moving the 256-character bound into CLI, and preserving the 3ms reminder timeout under its actual name; those are scope requirements rather than new policy choices.
Tradeoffs / consequences: legacy table remains readable/writable by older binaries, while the new Runtime offers no diagnostic CRUD API. Telemetry and reminder behavior are unchanged.
Affected issue and code: R2-2b; Runtime API/tests, migration proof, CLI-local diagnostic bound and directly stale docs.
Validation evidence: `cargo test --locked -p sctx-task-runtime` passed 70 tests (0 ignored; 0 doc tests), including preserved checkpoint compatibility and all 6 migration tests. The adapted migration target passed all 6 again after replacing a lint-rejected tuple representation with named assertion fields. CLI doctor/lifecycle/fail-open/activation/signals targets passed 2+7+12+14+14=49 tests. Release hook_hot_path passed its 32-way two-phase test: shared-file p99 108.412167ms, distinct-file p99 100.288792ms. Full workspace all-target/all-feature warning-free clippy and scoped rustfmt/diff checks passed. The complete ensure_schema/migration/helper region and checkpoint compatibility module are byte-identical; reminder activity executable code is identical except the constant name. No removed symbol remains in crates (word-boundary scan). Logs `/private/tmp/sctx-r2-2b-{runtime-tests,cli-tests,migration-recheck,hook-performance,workspace-clippy}.log`; audit `/private/tmp/sctx-r2-2b-invariance.json`. Removed 4 APIs, 3 record/view/count types, 2 prune constants, 5 storage integration tests and 1 retention unit test; retained table/schema/version/decision/reminder semantics.
Status: agent-selected
Supersedes / superseded by: none.
