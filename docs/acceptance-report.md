# Task-first Retrieval Integration Acceptance Report

Date: 2026-08-23
Scope: completed redesign work through Mew #170, including Mandatory Gates #114/#117, Working Intent fixes #136/#169, accepted boundary #150, and final M4 Gate #164
Documentation baseline before the #170 language alignment: `main@e03193b`

## Verdict

M1 closes the Task-first domain and entry-point foundation. M2 closes the local Task Runtime and explainable multi-Space retrieval path. M3 closes the Engineering Graph through a local Repository Registry, bounded multi-language scanner, persistent Engineering References, rebuildable resolution projection, bounded ContextRelation traversal, graph retrieval, and explicit MCP/CLI workflows.

Session startup and PromptSubmit are capability-only: neither infers Working Intent nor runs an automatic knowledge query. Task-aware retrieval begins after the Agent explicitly calls `task_intent_update`; subsequent `task_context` calls are read-only.

| Milestone | Status | Boundary |
|---|---|---|
| M1 — Task-first primitives | IMPLEMENTED | Covered below |
| M2 — Task Runtime and multi-Space retrieval | IMPLEMENTED | TaskSession persistence, strict `task_intent_update`, read-only `task_context`, association inference, typed RetrievalPaths, and TaskContextPack pass focused cross-crate/E2E oracles |
| M3 — Engineering Graph | IMPLEMENTED | Reference-derived bounded ScanPlan, deterministic Artifact locators, sparse build-time Context/safety snapshots, historical exact retrieval, frozen 1–2 hop relations, diagnostics, fallback, rebuild equivalence, and budget bounds pass cross-crate/E2E oracles |
| M4 — Low-tax Capture | IMPLEMENTED | #117, #136, #156–#164 and #169 provide lightweight idempotent Working Intent, Hint Text retrieval, exact WorkEpisode/Checkpoint automation, Builder/analysis/Review, and atomic existing/new Candidate Confirmation, closed by a fixed cross-layer oracle plus Hook/Capture/privacy/performance suites |

`task_intent_update` is the only public Working Intent write path. It creates or advances a `TaskIntentRevision` containing one `WorkingIntentSnapshot`. `task_context` accepts only an external Session locator and output bounds, and reads the already-authoritative ActiveTask without mutating Runtime. PromptSubmit supplies guidance rather than inferred Intent. Explicit `context_search.space_ids` remains available as a hard filter for diagnosis and exploration.

Mandatory Gates #114/#117 are implemented: stable `submission_id` and exact closed Episode ownership are represented in Git, rebuilt into SQLite submission/conflict indexes, and admitted only after Task/Intent/source Episode verification. Same-ID retries reuse the original Candidate while conflicting or identifiable malformed same-ID Events fail closed; different IDs and unrelated invalid/unknown Events remain isolated. The write path uses indexed lookup rather than Event scans or commit subjects.

## Acceptance matrix

| Requirement | Result | Authoritative evidence |
|---|---|---|
| `WorkingIntentSnapshot` has no Space or Workspace route | PROVEN | `sctx-domain` serialization contract and historical test identifier `milestone_one_contract::task_intent_has_no_route_and_accepts_zero_or_many_space_associations` |
| One Task accepts zero or multiple Space associations | PROVEN | `TaskSpaceAssociation::validate_collection` domain contract and the cross-crate milestone test |
| Search request has no preferred-Space ranking input | PROVEN | `SearchRequest` contract; Search cursor/ranking contains only BM25, Evidence completeness, and stable IDs |
| Explicit exploration can still hard-filter by Space | PROVEN | MCP contract sends `context_search.space_ids`; `SearchFilters.space_ids` is applied in SQL before ranking |
| `task_context` is truthful and read-only | PROVEN | MCP schema accepts only locator/budget/max fields; route, Task, Intent, Workspace and Signal fields are strictly rejected; concurrent reads preserve Task/Revision/Signal bytes |
| Workspace cannot select or persist a Space | PROVEN | `config.toml` contains the fixed Store and local Repository Catalog only; Repository entries contain IDs/paths but no Space, Requirement, Task, or ranking route |
| Automatic Candidate creation requires no Space | PROVEN | Closed-Episode Builder output carries exact source ownership and no Space; Primary/Related selection occurs only during explicit confirmation |
| Manual Candidate creation is absent from public product surfaces | PROVEN | MCP dispatch/schema/tool list and CLI help/command expose no manual submission; the fixed residue gate dynamically checks the removed name while internal `submit_candidate` remains Builder-only |
| Candidate submission retry is exact and rebuildable | PROVEN | 20-thread and 20-process concurrency converge to one Candidate; all writer crash seams recover stable batch/commit metadata; deleting SQLite rebuilds the same submission mapping; same-ID/different-content is typed conflict while different IDs never deduplicate |
| Malformed Candidate Events are submission-local | PROVEN | Bounded known-v1 hint extraction accepts only exact Candidate envelopes with a valid SubmissionId; same-ID malformed/duplicate/conflicting Events populate only that submission's conflict row, while unknown schema, missing hint and different IDs retain diagnostics without blocking valid creation; incremental, scratch and DB-deletion rebuilds agree |
| Manual Candidate staging cannot bypass submission admission | PROVEN | Generic append rejects valid Candidate Events, while `validate_staged` explicitly rejects both known Candidate additions and identifiable malformed Candidate additions as requiring the Candidate submission service; unrelated parse errors remain strict rather than swallowed |
| Candidate admission verifies source ownership before Git | PROVEN | MCP adversarial coverage rejects missing, open, cross-Task and stale-Intent Episode sources with zero Event writes |
| Unconfirmed Candidate cannot enter automatic injection | PROVEN | Candidate projection is outside Context FTS; CLI/MCP candidate retrieval tests and Codex hook test return only Accepted eligible Context |
| Review discovery is automatic-Candidate-only and Task-isolated | PROVEN | Runtime v9 initializes Review exactly when a Builder item finalizes; same-Workspace dual Sessions and multiple Episodes/Candidates remain isolated, while a directly submitted Git-only internal fixture never appears in list/get |
| Candidate Review is complete, bounded, and untrusted | PROVEN | Cursor/Codex MCP and CLI list/get return whole draft/Evidence/provenance/analysis/Space recommendations with an untrusted marker; stable cursor, limit, token-budget omission and complete get pass adversarial tests |
| Discard is explicit, audited, and idempotent | PROVEN | Task/Intent/Review CAS changes Pending to Discarded once; same-reason timeout retry returns already_discarded, different reason/stale/cross-Task/Confirmed/secret input fail typed, and default list hides discarded |
| Expired Reviews cannot revive | PROVEN | TTL cleanup deletes heavy Runtime analysis, advances a terminal Expired tombstone, never touches Git, and a later Builder retry cannot recreate Pending; deleting runtime removes all unconfirmed Review discovery |
| Existing confirmed Context fixtures use neutral revision terminology | PROVEN | Event constructor is `context_revision_added`; no Context-Propose API or constructor remains |
| SessionStart and PromptSubmit emit capability guidance without knowledge retrieval | PROVEN | Shared lifecycle policy returns no Task Runtime operation; the CLI adversarial contract seeds two Spaces with Accepted eligible Context and proves both lifecycle events emit no item before the Agent explicitly calls `task_intent_update` and retrieves only its task match |
| Codex dynamic sessions are isolated and incorporate later non-locating outcomes | PROVEN | CLI adversarial contract runs two Codex sessions in one Git Workspace, proves pre-Prompt PostToolUse cannot create a Session, File observations remain Breadcrumb-only, and TestOutcome changes only the owning Task fingerprint without Graph semantics |
| Task retrieval accepts no caller Space or Workspace route | PROVEN | `milestone_two_contract` serializes the accepted input and rejects injected `space_id`, `space_ids`, `workspace`, and `workspace_id` fields |
| Task retrieval returns zero, one, or many Space candidates | PROVEN | The M2 oracle executes unrelated, page-only, and multi-Space tasks through the shared Runtime/MCP/Search/Index path |
| One FE task retrieves Requirement, server Contract, and cross-platform Validation Context | PROVEN | The fixed M3 oracle uses frontend Symbol and FE API/Schema Focuses, then asserts exact hand-authored Requirement/Decision/Contract/iOS/Android identities through Graph plus ContextRelation paths |
| Every returned Context links to a Space association and typed RetrievalPath | PROVEN | M2 validates Intent FTS, Context FTS, and Scope; M3 validates uniquely resolved EngineeringGraph and ContextRelation paths. Textual Task Signals no longer claim Graph semantics |
| Same Workspace external Sessions remain isolated | PROVEN | Page and server calls share one Workspace signal but receive distinct TaskSession/Task IDs and disjoint Space/Context sets |
| Workspace paths cannot create Space or Artifact priors | PROVEN | Workspace signals remain non-locating; RepositoryId enters Graph retrieval only inside the complete `ResolvedFocus` of one ArtifactFocusQuery and never as a global filter |
| Artifact Focus is query-scoped and leaves no Runtime state | PROVEN | Domain and Runtime contain no Focus record, ID, lifecycle, history, table or snapshot field; repeated A, A→B, ordinary no-Focus reads and MCP restart preserve stable serialized ExternalSession/Task/Intent/Signal state and retrieval fingerprint |
| Public Artifact Focus contract is strict and server-resolved | PROVEN | MCP `task_artifact_focus` exposes only Session locator, expected Revision, absolute path, six no-path coordinate shapes and output bounds with recursive `additionalProperties=false`; forged identity/route/generation/Hook fields fail |
| Public MCP queries reach Graph for all six kinds | PROVEN | Real MCP frames create ActiveTasks and query File/Module/Symbol/API/Schema/Test Focuses, immediately return exact Graph Context, return no ID/lifecycle metadata, isolate equal locators by Repository, and leave the next ordinary `task_context` without Focus |
| Repository-scoped exact Focus never crosses repositories | PROVEN | Search builds two historical Graph nodes with identical locators under different RepositoryIds and proves each Focus returns only its named Repository Context |
| Cross-parent Catalog mapping never mixes repositories | PROVEN | Real MCP Focuses use equal `src/shared.ts` locators under configured FE/Android/iOS checkouts and return only the Context owned by each stable Catalog RepositoryId |
| Unreachable Focus has typed zero-result semantics | PROVEN | Wrong-Repository and unavailable-Graph Focuses return no Graph Context and a budgeted `artifact_not_reachable_in_graph` diagnostic; they never claim current-code `missing` |
| Reachability reflects actual selected-mode output | PROVEN | Unsafe/ambiguous automatic Focus remains unreachable; Explicit marks reachable only after a GraphDiagnostic, preventing false suppression of `artifact_not_reachable_in_graph` |
| PostTool observations never fabricate Artifact identity | PROVEN | A real Codex Hook Prompt→PostTool sequence retains one Task ID, keeps File data out of engineering Signals/Focus, records TestOutcome as non-locating, and creates no Graph edge |
| Capture identity and ownership are verifiable | PROVEN | Typed `CaptureId`, ExternalSessionLocator and optional exact ActiveTask/Intent owner are stored after redaction; pre-Task Capture remains TTL-bound with `no_active_task` and cannot be claimed or attributed elsewhere |
| Capture storage is bounded, private and retryable | PROVEN | CaptureStore read/list/claim/cleanup enforce limits, 0700/0600, privacy redaction, TTL, symlink/invalid preservation, same-owner claim idempotency and cross-Task/Episode rejection; no transcript/command/tool output is accepted |
| WorkEpisode persistence is Task-isolated and CAS-guarded | PROVEN | Runtime v8 enforces one Open Episode per TaskSession, server IDs, ordered Intent/Signal refs, normalized Observation sources, Episode version CAS, inactive-Task rejection and same-Workspace dual-Session isolation; Candidate Build and derived analysis remain Episode-owned |
| Public AgentCheckpoint is strict and transactionally owned | PROVEN | MCP/CLI require external Session, Task/Intent/Episode CAS, boundary, complete Claims/Unknowns and typed refs with recursive `additionalProperties=false`; Runtime atomically opens/advances Episode, assigns WorkObservation/Claim/Checkpoint IDs and continues or closes |
| Checkpoint retries are semantic and concurrent-safe | PROVEN | `(episode_id,parent_episode_version)` is unique; eight concurrent identical writes return one created Checkpoint and stable IDs, identical timeout retries return the original, changed content conflicts and stale versions have distinct typed errors |
| Checkpoint Evidence is verified and private | PROVEN | One Index snapshot validates Context→Revision→Evidence, Episode/Task validates WorkObservation ownership, and inline Validation is self-contained. TaskSignal references are validated only as owned, non-factual source clues: Prompt/Workspace cannot become engineering Evidence, while explicitly cited normalized Diff/TestOutcome clues are converted by the Builder into self-contained EvidenceSnapshot content. PrivacyScanner rejects Secret/PII, and ArtifactRef alone cannot satisfy Evidence |
| Codex and Cursor close Episodes without Hook claims | PROVEN | Real newline Cursor and Content-Length Codex MCP sessions execute Intent→inline Validation Checkpoint→unknown-only close; continue writes no Candidate, while close builds the earlier evidenced Claim exactly once and returns stable Candidate summaries |
| Candidate Builder is deterministic, evidence-bounded and crash-safe | PROVEN | Typed WorkObservation, Context Evidence, normalized Diff/TestOutcome source conversion, and inline Validation paths produce complete drafts; Unknown-only and unsupported Prompt clues produce zero Git writes; missing kind uses Discovery and missing topic remains a non-blocking Unknown; Runtime-before-Git, Git-before-Runtime, semantic retry, identical-text distinct Claims, and 20 concurrent builds preserve stable operation identities |
| Candidate relationship analysis is typed and conservative | PROVEN | Full canonical draft equality alone yields exact duplicate; equal statement with different rationale/Evidence yields support; same topic plus explicit Context or exact Graph path yields revision; differing topic/scope statements yield potential contradiction; pure BM25 remains unresolved related; no candidate yields novel |
| Candidate Space recommendation is non-binding and generation-pinned | PROVEN | Deterministic RRF fuses assessment targets, source Task associations and Space Intent evidence under top-k/token budget; fixed Context/Graph generations and stable target ties survive Index rebuild; conflicted/unsafe Spaces cannot become Primary and absence of a safe Primary produces one complete system-suggested Intent |
| Candidate Confirmation facts are causal and conflict-explicit | PROVEN | Existing/new Primary, multiple Related Spaces and field edits reduce identically under Event permutation; exact Candidate/source, embedded owner, Association, causal Publish and generated-ID-free content hash are verified; duplicate Confirmations and Association heads remain explicit conflicts, while later Withdraw preserves historical confirmation and current lifecycle exclusion |
| Candidate Confirmation is recoverable and atomic | PROVEN | Runtime reserves a complete stable 4/5-Event plan before Git; every Writer crash seam, pending recovery, Git-before-Runtime recovery and index deletion converge to one batch/commit with no partial facts |
| Confirmation input keeps identities server-owned | PROVEN | MCP primary is strict oneOf existing Space or current proposed recommendation ID; full new Intent and Confirmation/Context/Revision/Evidence/Event/Batch/Git IDs are absent and additional properties fail |
| Explicit confirmation closes Review and exposes accepted Context | PROVEN | Existing/new Primary, two Related Spaces and optional edits return assessment acknowledgments; 100 threads and 20 CLI processes converge, Pending list clears, Confirmed audit remains, and Context Search returns the published revision |
| Candidate analysis is disposable Runtime review state | PROVEN | Runtime v8 atomically replaces current analysis with monotonically increasing analysis generation; failed analysis remains Draft and retryable; deleting the derived row permits recomputation; Builder and internal CLI rerun preserve Candidate/Submission/Event identity and add no Git, Search, Hook or auto-injection state |
| Capture ingestion survives claim/commit races | PROVEN | `capture_ingestion.capture_id` is unique; concurrent/retried ingestion returns one Observation, changed retry content/cross owner is rejected, and claim-before-failed-commit remains retryable without source deletion |
| Capture File hints never guess Repository | PROVEN | Catalog maps only safe existing Workspace-allowed configured paths to File ArtifactRef; unconfigured/unsafe paths retain Capture source/summary plus typed Runtime diagnostic |
| Source Episode is query-verifiable without Candidate creation | PROVEN | Explicit Runtime and Checkpoint APIs return exact Episode owner/version/status/Observation/Checkpoint state; deleting runtime loses Episode and Checkpoint only while Git/Index/Capture bytes remain |
| Hook automates only a persisted Checkpoint boundary | PROVEN | Real Codex PreCompact and Cursor TurnStop fixtures close only an already checkpointed current-Intent Episode and invoke the shared Builder; missing/stale Checkpoints remain Open, 12 concurrent processes and repeated events converge, same-Workspace Sessions stay isolated, and Hook never opens an Episode or fabricates Claim content |
| One black-box identity chain reaches accepted Context | PROVEN | `hook_to_confirm_chain` uses supported Codex Hook and MCP framing plus the real CLI: one external Session supplies Task/Revision IDs, exact request-scoped Graph Focus, redacted File/Test Captures and a real TestOutcome SignalId; a continue Checkpoint cites that Signal and exact ArtifactRef, TurnStop alone closes/builds, repeated Stop reuses one Candidate, Review preserves exact Episode/Checkpoint/Claim/content, and one 4-Event confirmation commit is retry-idempotent and searchable |
| Missing or failed Hook has a no-retype fallback | PROVEN | `task_checkpoint boundary=close` with current Episode version and empty Claims/Unknowns closes the previously persisted Checkpoint under Task/Intent/Episode CAS, returns that same Checkpoint ID, and invokes the same Builder without inventing an Unknown or duplicate Claim |
| Automatic TaskContextPack excludes every unsafe state | PROVEN | The oracle seeds an unassigned Candidate plus Space-associated Candidate, Deprecated, semantic-conflict, and incomplete-Evidence Context; automatic output is empty while a direct SearchEngine diagnostic query proves each fixture state exists |
| Tree, Generation, and Task fingerprint are consistent | PROVEN | M2 response Tree equals Git `HEAD^{tree}` and index metadata; Generation equals the same projection; identical Session input returns identical fingerprint, associations, items, and paths |
| Engineering workflows preserve identity, privacy, and ambiguity | PROVEN | Two-Repository/multi-worktree tests execute scan→record→rebuild→explain→Task Pack; concurrent Writer calls produce unique server-owned IDs; unsafe paths, incomplete evidence, and secrets are rejected; ambiguous candidates are returned without selection |
| Graph build is sparse and Reference-derived | PROVEN | Scanner tests seed 200 unreferenced tracked files plus an unreadable sentinel and prove zero observations/Artifacts for them; duplicate Reference paths collapse to one planned path; public `repository_scan` requires `paths` with `minItems: 1`/`maxItems: 10000`; `association_rebuild` reports the deduplicated planned-path count |
| Missing and empty plans never broaden scanning | PROVEN | Empty typed ScanPlan and empty MCP/CLI `paths` fail; an explicit missing path returns a typed `missing` skip with zero scanned files/Artifacts and no directory or Repository fallback |
| Repository identity is explicit and rebuildable | PROVEN | `repository add` generates typed UUID-v4 IDs; one Catalog ID accepts multiple explicit worktrees; SQLite deletion followed by Runtime/doctor/list restores the same IDs and locators; Registry has no basename/remote/common-dir merge API |
| Cross Workspace mapping is isolated and non-discovering | PROVEN | Real-structure CLI E2E configures FE/Android/iOS repos under one parent Workspace, maps equal relative paths to each owning stable RepositoryId, leaves an unconfigured sibling signal-free, and converges root/subdirectory/parent Workspace inputs |
| Hook observations are bounded, non-locating, and Git-free | PROVEN | A fake `git` sentinel proves PostTool never launches Git; File observations remain Breadcrumb-only, TestOutcome is non-locating, and Catalog/Registry failures return sanitized success responses without Focus submission |
| Catalog and Registry validation are explicit | PROVEN | CLI `repository add/list/doctor`, installer setup/doctor, and MCP Runtime open synchronize only trusted local Catalog IDs; invalid short IDs are typed errors and public `repository_scan` rejects unconfigured checkout or caller Repository identity fields |
| Declared missing paths resolve safely without code inspection | PROVEN | Catalog accepts missing leaf/tail below an exact configured checkout while rejecting dot segments, symlink components, existing non-directory parents, and unconfigured paths; historical Graph remains reachable after checkout deletion |
| Focus hot path has no engineering or Task mutation and no Hook dependency | PROVEN | Git HEAD, Graph canonical bytes, and canonical Runtime authoritative bytes remain unchanged; source sentinel rejects Command/Scanner/rebuild/Reference/Hook calls; real established-session MCP p95 stays below 250ms |
| Engineering failure is advisory to Task Retrieval | PROVEN | Rebuild reports unavailable registered Repositories explicitly, Explain reports typed projection availability, and a corrupt Engineering projection degrades Task responses to `artifact_generation: null` instead of blocking Context-only retrieval |
| Multi-language Artifact discovery has an independent bounded oracle | PROVEN | `milestone-three-v1.json` fixes hand-authored IDs/References and the exact Reference-derived path plan; a separate Scanner contract explicitly plans Rust, TS, JS, Swift, Kotlin, JSON, OpenAPI and Proto paths and validates exact API/Schema/Qualified Symbol/Test locators without full-repository enumeration |
| File move and Symbol rename never trigger guessing | PROVEN | M3 oracle moves a JS file and renames a TS Symbol, then proves both original deterministic locators become `missing`, create no Edge, and remain unchanged without Git-history or Agent repair workflow |
| Exact Graph retrieval expands cross-platform Context at bounded depth | PROVEN | Symbol→Decision→Contract→iOS/Android and FE API/Schema→Contract→Decision/iOS/Android paths match fixed Context IDs, relation kinds and depths; cycles never repeat a Context and all paths stop at depth two |
| Ambiguous edges are diagnostic-only | PROVEN | Explicit mode exposes both fixed candidates; automatic mode cannot use the edge to inject or raise Graph relevance |
| Engineering projection is disposable and generation-consistent | PROVEN | The oracle deletes `engineering.sqlite`, rebuilds byte-equivalent canonical projection, and proves every Graph path shares one Artifact Generation while Graph build Tree remains explicit provenance |
| Incremental and scratch scans are equivalent | PROVEN | The Scanner contract runs both paths with the same RepositoryId and deduplicated ScanPlan and asserts byte-for-byte equal RepositorySnapshot output |
| Associations, paths and omissions obey Token Budget | PROVEN | Full and constrained fixed Graph packs recompute exact charged tokens, stay within budget, obey top-k, and emit omissions when bounded |
| #136 removes Intent maturity and Evidence while preserving knowledge Evidence | PROVEN | `WorkingIntentSnapshot`, its Hints, and TaskSignals are explicitly non-factual and carry no maturity or Evidence binding. M3 typed Context Evidence, EngineeringReference support, Graph safety provenance, and exact Revision ownership continue to serve CheckpointClaim, Candidate and ContextRevision assertion chains |
| Historical Graph remains active across current Tree changes | PROVEN | Tree mismatch plus unrelated Candidate/Reference/Context/Publication append, new Revision and Withdraw preserve the old Graph path and exact frozen Revision without implicit rebuild |
| Graph safety is decided at build time | PROVEN | Candidate, incomplete-Evidence and semantic-conflict roots remain explicit-only; withdrawn-after-build safe Revision passes Agent Adapter only with matching Graph provenance, generation, identity and empty blockers |
| Current and historical revisions never collide | PROVEN | One Task returns the same ContextId's frozen old Graph Revision and current FTS Revision as separate revision-aware items; each path remains attached to its exact Revision |
| Graph ContextRelation traversal is historical | PROVEN | Frozen source/target Revision IDs survive a new current Revision with different relations; current fallback relations never extend an EngineeringGraph path |
| Sparse Context snapshot closure excludes unrelated corpus | PROVEN | Adding 64 unrelated Spaces/Contexts leaves Graph snapshot row count and Artifact Generation unchanged; only Reference roots plus two-hop closure are persisted |
| Concurrent Graph reads observe one stable generation | PROVEN | Concurrent repeated Graph rebuilds and Task reads return one Artifact Generation and the exact frozen historical Revision |

#150 remains an accepted product boundary: untracked files are not scanned, and no ActiveTask untracked scan entry was added.

#154 replaces the unlaunched #151/#152 persistence model: `task_artifact_focus` is a read-only ArtifactFocusQuery, Catalog supplies a request-local `ResolvedFocus`, and Search consumes only that value for the current Pack. Runtime owns no Focus state, and later Focus queries, ordinary `task_context`, MCP restart, Task switch, or compaction restore nothing. Repository Catalog remains local-only; team synchronization is not claimed.

#157 exposes explicit AgentCheckpoint through MCP/CLI/Skill without Hook-authored Claims. #114/#117 connect internal Builder submissions to closed Episode verification, submission-idempotent Git admission, and malformed-Event isolation. #158 deterministically builds unassigned drafts at close and through an internal CLI retry. #163 lets verified PreCompact/TurnStop Hooks close only an already checkpointed Episode and invoke that same Builder. #136 stores optional, non-factual Working Intent snapshots with canonical retry convergence, and #169 adds typed Hint Text recall without Graph semantics. #164 closes the chain with a hand-authored fixed oracle covering exact Episode provenance, no-retype Review, Candidate isolation, and atomic existing/new confirmation.

The fixed #136 oracle proves goal-only input, created/already-current continue, real changes, old-parent retry convergence, 20-way concurrency, stale zero-write, explicit new, Task switch and Runtime deletion. Artifact/interface Hints retrieve only through `WorkingIntentHintText`; they create no Git Event, Candidate, EngineeringReference, Graph path, Evidence, or automatic eligibility. Existing `episode_lifecycle_hooks`, `work_episode_capture`, and `hook_fail_open` suites cover PreCompact/TurnStop and Intent/Capture/Hook fail-open.

## Residue gates

The M1–M3 gate searches product code, tests, fixtures, scripts, and docs (excluding build output and the user-owned `readme.md`) for:

- the removed preferred-Space request/ranking field and CLI spelling;
- removed Space ranking cursor fields and exact-Space match reason;
- the removed Workspace-to-Space binding type and command surface;
- the removed Context-Propose API/constructor spelling.
- the removed task-text MCP bridge, bare-query Agent action, legacy Hook lookup helper, and non-Task automatic Context Pack CLI surface.
- the removed textual TaskSignal channel/path that previously looked like an Engineering Graph edge.
- the removed Repository auto-registration types, remote/declared/common-dir merge hints, and Hook `git rev-parse` discovery path;
- caller-supplied RepositoryId, RepoRelativePath, ArtifactKey, Generation, Workspace, Hook or corroboration fields in `task_artifact_focus`.
- persistent Focus records, Signal IDs, active/superseded Focus state, canonical Focus identities, Runtime Focus tables, and Focus fields in Task snapshots or fingerprints.
- bare-string Capture IDs, raw transcript/command/tool-output fields in Capture/Observation state, or ownerless Capture ingestion;
- Hook calls that open/checkpoint/ingest Episodes or derive Claims from summaries; implemented lifecycle automation may only advance ordered refs and close an already checkpointed Episode.

Expected result: zero matches. Generic target-design language such as a proposed new Space Intent is not a Context-Propose API. `context_search` and its explicit `space_ids` hard filter are intentionally present.

## Reproduction commands

```bash
# Focused M1–M3 contracts
cargo test --locked -p sctx-domain
cargo test --locked -p sctx-search --test search_contract
cargo test --locked -p sctx-mcp --test mcp_contract
cargo test --locked -p sctx-mcp --test engineering_workflows
cargo test --locked -p sctx-engineering-graph
cargo test --locked -p sctx-local-state --test repository_catalog
cargo test --locked -p sctx-task-runtime --test runtime_store
cargo test --locked -p sctx-task-runtime --test work_episode_store
cargo test --locked -p sctx-cli --test cli_contract
cargo test --locked -p sctx-cli --test hook_fail_open
cargo test --locked -p sctx-cli --test work_episode_capture
cargo test --locked -p sctx-cli --test milestone_one_contract
cargo test --locked -p sctx-cli --test milestone_two_contract
cargo test --locked -p sctx-cli --test milestone_three_contract
cargo test --locked -p sctx-cli --test milestone_four_contract

# Required repository gates
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --locked
(cd npm && npm test)
```

Current repository gate results:

- `cargo fmt --all -- --check`: passed.
- `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`: passed with no warnings.
- `cargo test --workspace --locked`: passed with no failures and one ignored manual benchmark.
- `npm test`: 16 passed, 0 failed, 0 skipped.
- Shared Context Skill `quick_validate.py`: passed (`Skill is valid!`).

The complete historical V1 storage, lifecycle, installer, adapter, and NPM regression coverage remains in the workspace suites. M1–M3 establish Task-first runtime retrieval and Engineering Graph truth. The hand-authored `fixtures/m4/fixed-oracle.json`, Working Intent oracle, cross-platform M3 oracle, real Cursor/Codex Hook suites, privacy/performance contracts, and Builder/Review/Confirmation recovery tests close M4 without claiming team synchronization or untracked-file scanning beyond accepted #150.
