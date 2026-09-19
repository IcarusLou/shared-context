# R2-1 compatibility readiness — source evidence, no implementation

## Conclusion

The real snapshot proves the five fields are empty on the **current direct authoring path**, not that the current persisted Claim contract only allows empty values. A wire adapter can preserve empty-key history and old-binary parsing, but **cannot erase all five values and their consumers while preserving valid nonempty historical Builder/analyzer behavior**. Preserving that behavior requires retaining the values in a compatibility carrier and retaining consumers through that carrier. That is bounded but changes the literal deletion outcome; it needs explicit R2-1 rescope authority before implementation. Do not silently reject or discard nonempty legacy fields; do not silently redefine all current valid legacy fixtures as invalid.

## Concrete source/test evidence

- `crates/domain/src/episode.rs:563–656`: CheckpointClaim exposes all five typed fields. `from_parts` accepts them, validates text/uniqueness/reference validity, and does **not** require empty. All five admit valid nonempty inputs.
- `crates/domain/src/episode.rs:2128`: `checkpoint_fixture()` constructs and unwraps a valid Claim with `recheck_when = ["The v3 contract ships"]` and one `ArtifactRef`. This is existing positive fixture data, not a malformed-input test.
- `crates/task-runtime/tests/work_episode_store.rs:78`: positive `checkpoint_claim()` helper has nonempty `recheck_when`; `write_agent_checkpoint` tests use it to write/read/retry real SQLite rows (e.g. the Continue/Close/retry assertions around413–466). `crates/cli/tests/episode_lifecycle_hooks.rs:200` likewise constructs valid nonempty Claim recheck text for lifecycle fixtures.
- All current calls to `write_agent_checkpoint` outside its definition are tests. Production MCP converts direct Claims via `materialize_direct_checkpoint_claim` (`task-runtime:5267–5315`), assigning five empty vectors. This supports removing new authoring access but does **not** invalidate already-supported stored data.
- Production `build_claim_material` (`mcp:5177–5179`) copies assumptions, recheck_when and relations into required ContextRevisionDraft fields. Production analyzer consumes related_contexts at `mcp:2917` and artifact_refs at `mcp:5054`. These consumers are active for deserialized nonempty historical Claims. No speculative external caller is needed for this finding.
- `read_checkpoint_by_parent` (`task-runtime:5423`) and `read_episode_checkpoints` (`:6752`) deserialize checkpoint_json directly. The current Claim is deny_unknown_fields; four deleted fields are required vectors, while relations has default. Thus simple deletion fails forward reading; only accepting old names but omitting them in new writes fails old-binary rollback reading. `derive_episode_claim_references` rewrites the whole checkpoint_json at2036–2041, so ignoring nonempty legacy keys would also lose them on a normal existing recovery path.

## Recommended bounded rescope option

Retire the five fields from **new authoring/public active Claim fields and CheckpointClaimDraft**, but preserve historical wire semantics:

1. Strict private wire DTO retains the old named typed fields and deny_unknown_fields; no arbitrary unknown-key acceptance and no JSON Value catch-all.
2. Active Claim carries one private optional `LegacyClaimFields` compatibility object populated only when any retired field is nonempty. New constructors set it absent. This still stores the old values for legacy Claims, so describe the outcome truthfully as authoring retirement + compatibility isolation, not complete data-field deletion.
3. Serialize new Claims with the old five empty-array keys; serialize historical Claims with their preserved values. This permits old-binary reading and preserves historical bytes' meaning when derivation rewrites a checkpoint. Keep pre-existing serde defaults/requiredness rather than broadly making fields optional unless separately justified.
4. Existing Builder/analyzer consumers read the compatibility carrier through narrow legacy-only accessors (empty for new Claims). Keep the explicit Search channel. Do not replace these consumers with unconditional Vec::new() for historical Claims.
5. Keep retired fields' **empty literal tombstones** in persisted low-level retry semantic JSON. No new authoring parameters are needed to produce them. Direct checkpoint semantic JSON remains byte/identity stable.

This is the narrowest design that simultaneously removes new authoring surface, preserves old valid history, permits rollback, and avoids schema/data migration. It retains some code the original pure-deletion item intended to remove; that consequence is the concrete human tradeoff. Alternative: defer R2-1 unchanged and continue other independent approved M2 work after disposition. Empty-only tombstone adapter is smaller, but is unacceptable without fresh authorization to abandon valid nonempty history.

## Low-level retry proof

`write_agent_checkpoint` computes `checkpoint_semantic_json(input)` at1531, reads `(checkpoint,persisted_semantics)` by Episode+parent version, and compares strings exactly at1566. The old helper includes all five keys (`:5187–5216`). With otherwise identical Claim content:

- old persisted semantics with five `[]` keys != new semantics with keys omitted;
- open Episode: returns conflict ('parent version already contains different content');
- closed Episode and expected parent version0: falls through and can insert another Episode instead of returning the old receipt.

A new-reader-only comparison normalizer fixes forward retries but **does not fix rollback**: old binary still compares its old-shaped request to the new keyless persisted string. Therefore compatible new low-level writes must retain old-shaped empty-key persistence, even if the logical active Claim representation omits the fields. Old nonempty low-level requests cannot be recreated through a deliberately field-removed new authoring API; do not collapse them to empty semantics or conflate them with an empty request. Their stored history/Builder behavior must nevertheless survive.

The real snapshot's16 semantic_json rows all use the separate direct claims/unknowns shape. Those are unaffected if `direct_checkpoint_semantic_json` and operation hashing remain untouched; this does not waive legacy low-level tests.

## Acceptance evidence to require after approved rescope

- Literal old empty-key checkpoint fixture through runtime history read, same direct retry receipt, derivation rewrite, and Candidate Build.
- Literal old **valid nonempty** Claim fixture through read + derivation rewrite + Builder/analyzer; recheck/assumptions/relations and artifact/explicit target behavior survive.
- New Claim serialization decodes using a frozen old Claim wire type requiring the old vectors; old-style retry semantic bytes remain equal after new low-level write.
- Unknown keys remain rejected; malformed retired typed values are not swallowed.
- Existing direct operation semantic JSON/hash unchanged; no schema/event/ContextRevision changes.

No tests, builds, tracked edits, DB writes or implementation were performed for this readiness assessment. Source/test references above are inspected evidence, not a claim that those tests were executed in this task.

## Exact existing test IDs for reviewer reproduction

These are existing source test definitions, not newly run tests:

- `sctx-domain::episode::tests::episode_open_close_and_serialization_keep_only_typed_normalized_inputs` uses `checkpoint_fixture` with nonempty recheck/artifact values.
- `crates/task-runtime/tests/work_episode_store.rs::checkpoint_is_atomic_semantically_idempotent_and_closes_without_hook_observations` (function starts393): helper has nonempty Claim recheck_when, eight concurrent writes collapse to one receipt, close+retry checks preserve identity.
- `crates/task-runtime/tests/work_episode_store.rs::checkpoint_disambiguates_closed_retries_new_episodes_and_open_episode_versions` (starts525): a closed first write atparent0 retries with created=false and identical checkpoint/episode; deleting semantic keys changes this branch.
- `crates/cli/tests/episode_lifecycle_hooks.rs::real_hooks_close_checkpointed_episodes_build_once_and_keep_sessions_isolated` (starts366): lifecycle helper supplies nonempty Claim recheck text; real hook Builder path processes it.

Suggested concise gate wording: Approve R2-1 as “retire new Claim authoring fields, preserve legacy wire data and Builder behavior via a private compatibility carrier” (recommended), or defer R2-1. Neither choice authorizes changing ContextRevision/event schema, deleting valid history, or removing the explicit Search channel.


## Main-agent verification at H-001

On 2026-09-09 the main agent independently ran both existing positive tests:

- `cargo test --locked -p sctx-domain --lib episode::tests::episode_open_close_and_serialization_keep_only_typed_normalized_inputs -- --exact`: 1 passed, 55 filtered.
- `cargo test --locked -p sctx-task-runtime --test work_episode_store checkpoint_is_atomic_semantically_idempotent_and_closes_without_hook_observations -- --exact`: 1 passed, 13 filtered.

These prove the nonempty Claim fixtures are currently accepted and persisted/retried, rather than merely hypothetical malformed fixtures. No R2-1 production changes have been made. H-001 remains pending user disposition.

## Disposition (2026-09-09, reviewed and ruled — H-001 closed)

**Ruling: the recommended bounded rescope (`LegacyClaimFields` compatibility carrier) is REJECTED. The smaller "empty-only tombstone adapter" — which this document said "is unacceptable without fresh authorization" — is APPROVED, and that authorization is granted here.**

### The decisive fact the recommendation weighed wrong

The assessment above is factually correct that the *contract* admits nonempty values. But the gate question is whether valid nonempty **history** exists, and it does not: measured across all 16 checkpoints in the real installation (`~/.shared-context/state/runtime.sqlite`), the combined element count of `assumptions` + `recheck_when` + `artifact_refs` + `related_contexts` + `relations` over every persisted Claim is **zero**. Nonempty values exist only in test fixtures. The public MCP `inputSchema` has never accepted these five fields (ADR-0003), `write_agent_checkpoint` has no production caller, and this document's own evidence (line on `materialize_direct_checkpoint_claim`) concedes production has only ever written five empty vectors. A compatibility carrier, legacy-only accessors, and preserved Builder/analyzer behavior for nonempty historical Claims would protect data that provably does not exist in any real installation — which is exactly the chain audit's bloat pattern ① ("build runtime machinery for a hypothetical need") reconstructed inside the very work item whose purpose is to delete an instance of it.

### Approved — do exactly this

1. **Empty-required wire DTO.** A private wire DTO reads the five legacy keys and **requires them empty; a nonempty value is a loud typed error**, never a silent discard. This honors the red line above ("do not silently reject or discard"); on real data the error path is unreachable.
2. **Tombstone writes.** New writes keep serializing the five empty-key literals in `checkpoint_json` and the fat `checkpoint_semantic_json`. This is what the "Low-level retry proof" section actually requires: old-binary rollback still parses, `derive_episode_claim_references`'s full-JSON rewrite stays byte-stable, and low-level retry string equality holds in both directions.
3. **Field deletion in the active types.** `CheckpointClaim` / `CheckpointClaimDraft` lose the five fields; `build_claim_material` supplies empty `Vec`s (byte-identical to what the direct path produces today); the explicit Search channel stays wired and structurally empty (keep-list honored — the channel is not removed, it simply has no producers, matching its 0-hit reality).
4. **Fixture rewrite is authorized.** The nonempty fixtures (`episode.rs:2128`, `work_episode_store.rs:78`, `episode_lifecycle_hooks.rs:200`) are rewritten to the empty form as part of this change. This is the inherent consequence of deleting an authoring surface, explicitly authorized here — not a silent redefinition of valid data as invalid.
5. **Untouched:** `direct_checkpoint_semantic_json`, operation hashing, `ContextRevision`/`ContextRevisionDraft`, `schemas/event-v1.schema.json`. All 16 real semantic rows are direct-shaped and unaffected.

### Rejected — do not do, and why

- **`LegacyClaimFields` carrier + legacy-only accessors + nonempty-history Builder/analyzer behavior**: protects nonexistent data at the cost of retaining most of what R2-1 exists to remove (the tradeoff the recommendation itself named). Rejected on the project rule that generation-side cost must buy real value.
- **Deferring R2-1** (the listed alternative): unnecessary once the empty-only variant is authorized.
- **Unchanged prohibitions from the R2-1 charter**: no event-schema or `ContextRevision` changes, no schema/data migration, no unknown-key acceptance or `Value` catch-all, no removal of the explicit Search channel wiring.

### Acceptance (replaces the earlier list's nonempty-survival item)

- Literal old empty-key checkpoint fixture: runtime history read, same direct retry receipt, derivation rewrite, and Candidate Build all pass.
- A nonempty legacy input fails with the loud typed error (test pinned).
- New Claim serialization decodes under a frozen old wire type that requires the five vectors (as empty).
- Old-style retry semantic bytes remain equal after a new low-level write; unknown keys remain rejected.
- Direct operation semantic JSON/hash byte-identical; no schema/event/ContextRevision diffs in the change.

## Implementation acceptance (2026-09-09)

The final approved Disposition is implemented and independently accepted at `274170cc8bee77ae2da6ca3c6f513540f20a4fba`. Five active fields are removed; the private decoder accepts only empty legacy arrays with explicit typed nonempty rejection. Old empty wire order/keys and fat retry bytes survive; direct semantics/hash and Context/event schema remain unchanged. Authorized fixture rewrites are complete. No LegacyClaimFields carrier was introduced. Main re-ran five direct compatibility tests; full affected gates passed300tests plus13CLI tests. Current status: H-001 resolved, R2-1 accepted. See execution.md and decision log D-006/D-007 for evidence and the reviewed decoder correction.
