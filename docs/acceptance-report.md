# Task-first Retrieval Integration Acceptance Report

Date: 2026-08-22
Scope: Mew #112 through #153, excluding deferred #151/#152 and accepted boundary #150
Implementation baseline for local Repository Catalog: `main@92b88fe`

## Verdict

M1 closes the Task-first domain and entry-point foundation. M2 closes the local Task Runtime and explainable multi-Space retrieval path. M3 closes the Engineering Graph through a local Repository Registry, bounded multi-language scanner, persistent Engineering References, rebuildable resolution projection, bounded ContextRelation traversal, graph retrieval, and explicit MCP/CLI workflows.

Session startup is capability-only: it does not run an empty automatic knowledge query. Task-aware automatic retrieval begins only from a supported PromptSubmit event.

This report does **not** claim M4:

| Milestone | Status | Boundary |
|---|---|---|
| M1 — Task-first primitives and unassigned Candidate entry | IMPLEMENTED | Covered below |
| M2 — Task Runtime and multi-Space retrieval | IMPLEMENTED | TaskSession persistence, strict `task_intent_update`, read-only `task_context`, association inference, typed RetrievalPaths, and TaskContextPack pass focused cross-crate/E2E oracles |
| M3 — Engineering Graph | IMPLEMENTED | Reference-derived bounded ScanPlan, deterministic Artifact locators, sparse build-time Context/safety snapshots, historical exact retrieval, frozen 1–2 hop relations, diagnostics, fallback, rebuild equivalence, and budget bounds pass cross-crate/E2E oracles |
| M4 — Low-tax Capture | NOT IMPLEMENTED | No automatic WorkEpisode aggregation, Candidate Builder, Space recommendation, deduplication, or confirmation workflow |

`task_intent_update` is the only Task Intent write path. `task_context` accepts only an external Session locator and output bounds, and reads the already-authoritative ActiveTask without mutating Runtime. PromptSubmit supplies guidance rather than inferred Intent. Explicit `context_search.space_ids` remains available as a hard filter for diagnosis and exploration.

M4 retains Mandatory Gate #114/#117: stable `submission_id` idempotency must be represented in Git and rebuilt into a SQLite unique index so Candidate retries neither scan all Events, depend on Git commit subjects, nor fail because of an unrelated malformed Event. M4 must also make `source_episode_id` verifiable. None of those Capture guarantees are claimed by M2.

## Acceptance matrix

| Requirement | Result | Authoritative evidence |
|---|---|---|
| `TaskIntent` has no Space or Workspace route | PROVEN | `sctx-domain` serialization contract and `milestone_one_contract::task_intent_has_no_route_and_accepts_zero_or_many_space_associations` |
| One Task accepts zero or multiple Space associations | PROVEN | `TaskSpaceAssociation::validate_collection` domain contract and the cross-crate milestone test |
| Search request has no preferred-Space ranking input | PROVEN | `SearchRequest` contract; Search cursor/ranking contains only BM25, Evidence completeness, and stable IDs |
| Explicit exploration can still hard-filter by Space | PROVEN | MCP contract sends `context_search.space_ids`; `SearchFilters.space_ids` is applied in SQL before ranking |
| `task_context` is truthful and read-only | PROVEN | MCP schema accepts only locator/budget/max fields; route, Task, Intent, Workspace and Signal fields are strictly rejected; concurrent reads preserve Task/Revision/Signal bytes |
| Workspace cannot select or persist a Space | PROVEN | `config.toml` contains the fixed Store and local Repository Catalog only; Repository entries contain IDs/paths but no Space, Requirement, Task, or ranking route |
| Candidate creation requires no Space | PROVEN | CLI and MCP `candidate_create` contracts reject Space fields and return an unassigned Candidate ID |
| `candidate_create` is the Agent-facing Candidate main path | PROVEN | CLI help, MCP tool list, CLI milestone test, and MCP client fixtures |
| Unconfirmed Candidate cannot enter automatic injection | PROVEN | Candidate projection is outside Context FTS; CLI/MCP candidate retrieval tests and Codex hook test return only Accepted eligible Context |
| Existing confirmed Context fixtures use neutral revision terminology | PROVEN | Event constructor is `context_revision_added`; no Context-Propose API or constructor remains |
| SessionStart emits capability guidance without knowledge retrieval | PROVEN | Shared lifecycle policy returns no Task Runtime operation; the CLI adversarial contract seeds two Spaces with Accepted eligible Context and proves startup emits neither item before a supported PromptSubmit retrieves only its task match |
| Codex dynamic sessions are isolated and incorporate later non-locating outcomes | PROVEN | CLI adversarial contract runs two Codex sessions in one Git Workspace, proves pre-Prompt PostToolUse cannot create a Session, File observations remain Breadcrumb-only, and TestOutcome changes only the owning Task fingerprint without Graph semantics |
| Task retrieval accepts no caller Space or Workspace route | PROVEN | `milestone_two_contract` serializes the accepted input and rejects injected `space_id`, `space_ids`, `workspace`, and `workspace_id` fields |
| Task retrieval returns zero, one, or many Space candidates | PROVEN | The M2 oracle executes unrelated, page-only, and multi-Space tasks through the shared Runtime/MCP/Search/Index path |
| One FE task retrieves Requirement, server Contract, and cross-platform Validation Context | PROVEN | The fixed M3 oracle uses frontend Symbol and FE API/Schema Focuses, then asserts exact hand-authored Requirement/Decision/Contract/iOS/Android identities through Graph plus ContextRelation paths |
| Every returned Context links to a Space association and typed RetrievalPath | PROVEN | M2 validates Intent FTS, Context FTS, and Scope; M3 validates uniquely resolved EngineeringGraph and ContextRelation paths. Textual Task Signals no longer claim Graph semantics |
| Same Workspace external Sessions remain isolated | PROVEN | Page and server calls share one Workspace signal but receive distinct TaskSession/Task IDs and disjoint Space/Context sets |
| Workspace paths cannot create Space or Artifact priors | PROVEN | Workspace signals remain non-locating; RepositoryId enters Graph retrieval only inside a complete TaskArtifactFocus and never as a global filter |
| TaskArtifactFocus lifecycle is isolated and deterministic | PROVEN | Internal Runtime tests cover all six Artifact kinds, Task+Intent CAS, concurrent/repeated dedup, stable SignalId, supersede/history/re-focus, new Task reset, task switch, and same-Workspace double Session isolation without creating Intent revisions |
| Repository-scoped exact Focus never crosses repositories | PROVEN | Search builds two historical Graph nodes with identical locators under different RepositoryIds and proves each Focus returns only its named Repository Context |
| Unreachable Focus has typed zero-result semantics | PROVEN | Wrong-Repository and unavailable-Graph Focuses return no Graph Context and a budgeted `artifact_not_reachable_in_graph` diagnostic; they never claim current-code `missing` |
| PostTool observations never fabricate Artifact identity | PROVEN | A real Codex Hook Prompt→PostTool sequence retains one Task ID, keeps File data out of engineering Signals/Focus, records TestOutcome as non-locating, and creates no Graph edge |
| Automatic TaskContextPack excludes every unsafe state | PROVEN | The oracle seeds an unassigned Candidate plus Space-associated Candidate, Deprecated, semantic-conflict, and incomplete-Evidence Context; automatic output is empty while a direct SearchEngine diagnostic query proves each fixture state exists |
| Tree, Generation, and Task fingerprint are consistent | PROVEN | M2 response Tree equals Git `HEAD^{tree}` and index metadata; Generation equals the same projection; identical Session input returns identical fingerprint, associations, items, and paths |
| Engineering workflows preserve identity, privacy, and ambiguity | PROVEN | Two-Repository/multi-worktree tests execute scan→record→rebuild→explain→Task Pack; concurrent Writer calls produce unique server-owned IDs; unsafe paths, incomplete evidence, and secrets are rejected; ambiguous candidates are returned without selection |
| Graph build is sparse and Reference-derived | PROVEN | Scanner tests seed 200 unreferenced tracked files plus an unreadable sentinel and prove zero observations/Artifacts for them; duplicate Reference paths collapse to one planned path; public `repository_scan` requires `paths` with `minItems: 1`/`maxItems: 10000`; `association_rebuild` reports the deduplicated planned-path count |
| Missing and empty plans never broaden scanning | PROVEN | Empty typed ScanPlan and empty MCP/CLI `paths` fail; an explicit missing path returns a typed `missing` skip with zero scanned files/Artifacts and no directory or Repository fallback |
| Repository identity is explicit and rebuildable | PROVEN | `repository add` generates typed UUID-v4 IDs; one Catalog ID accepts multiple explicit worktrees; SQLite deletion followed by Runtime/doctor/list restores the same IDs and locators; Registry has no basename/remote/common-dir merge API |
| Cross Workspace mapping is isolated and non-discovering | PROVEN | Real-structure CLI E2E configures FE/Android/iOS repos under one parent Workspace, maps equal relative paths to each owning stable RepositoryId, leaves an unconfigured sibling signal-free, and converges root/subdirectory/parent Workspace inputs |
| Hook observations are bounded, non-locating, and Git-free | PROVEN | A fake `git` sentinel proves PostTool never launches Git; File observations remain Breadcrumb-only, TestOutcome is non-locating, and Catalog/Registry failures return sanitized success responses without Focus submission |
| Catalog and Registry validation are explicit | PROVEN | CLI `repository add/list/doctor`, installer setup/doctor, and MCP Runtime open synchronize only trusted local Catalog IDs; invalid short IDs are typed errors and public `repository_scan` rejects unconfigured checkout or caller Repository identity fields |
| Engineering failure is advisory to Task Retrieval | PROVEN | Rebuild reports unavailable registered Repositories explicitly, Explain reports typed projection availability, and a corrupt Engineering projection degrades Task responses to `artifact_generation: null` instead of blocking Context-only retrieval |
| Multi-language Artifact discovery has an independent bounded oracle | PROVEN | `milestone-three-v1.json` fixes hand-authored IDs/References and the exact Reference-derived path plan; a separate Scanner contract explicitly plans Rust, TS, JS, Swift, Kotlin, JSON, OpenAPI and Proto paths and validates exact API/Schema/Qualified Symbol/Test locators without full-repository enumeration |
| File move and Symbol rename never trigger guessing | PROVEN | M3 oracle moves a JS file and renames a TS Symbol, then proves both original deterministic locators become `missing`, create no Edge, and remain unchanged without Git-history or Agent repair workflow |
| Exact Graph retrieval expands cross-platform Context at bounded depth | PROVEN | Symbol→Decision→Contract→iOS/Android and FE API/Schema→Contract→Decision/iOS/Android paths match fixed Context IDs, relation kinds and depths; cycles never repeat a Context and all paths stop at depth two |
| Ambiguous edges are diagnostic-only | PROVEN | Explicit mode exposes both fixed candidates; automatic mode cannot use the edge to inject or raise Graph relevance |
| Engineering projection is disposable and generation-consistent | PROVEN | The oracle deletes `engineering.sqlite`, rebuilds byte-equivalent canonical projection, and proves every Graph path shares one Artifact Generation while Graph build Tree remains explicit provenance |
| Incremental and scratch scans are equivalent | PROVEN | The Scanner contract runs both paths with the same RepositoryId and deduplicated ScanPlan and asserts byte-for-byte equal RepositorySnapshot output |
| Associations, paths and omissions obey Token Budget | PROVEN | Full and constrained fixed Graph packs recompute exact charged tokens, stay within budget, obey top-k, and emit omissions when bounded |
| M3 defines the verifiable Evidence source boundary for deferred #136 | PROVEN | Technical design defines typed TaskSignal, ContextEvidence and EngineeringResolution sources, ownership/snapshot rules, negative statuses, and the M4 persistence boundary without claiming it is implemented |
| Historical Graph remains active across current Tree changes | PROVEN | Tree mismatch plus unrelated Candidate/Reference/Context/Publication append, new Revision and Withdraw preserve the old Graph path and exact frozen Revision without implicit rebuild |
| Graph safety is decided at build time | PROVEN | Candidate, incomplete-Evidence and semantic-conflict roots remain explicit-only; withdrawn-after-build safe Revision passes Agent Adapter only with matching Graph provenance, generation, identity and empty blockers |
| Current and historical revisions never collide | PROVEN | One Task returns the same ContextId's frozen old Graph Revision and current FTS Revision as separate revision-aware items; each path remains attached to its exact Revision |
| Graph ContextRelation traversal is historical | PROVEN | Frozen source/target Revision IDs survive a new current Revision with different relations; current fallback relations never extend an EngineeringGraph path |
| Sparse Context snapshot closure excludes unrelated corpus | PROVEN | Adding 64 unrelated Spaces/Contexts leaves Graph snapshot row count and Artifact Generation unchanged; only Reference roots plus two-hop closure are persisted |
| Concurrent Graph reads observe one stable generation | PROVEN | Concurrent repeated Graph rebuilds and Task reads return one Artifact Generation and the exact frozen historical Revision |

#150 remains an accepted product boundary: untracked files are not scanned, and no ActiveTask untracked scan entry was added.

#151 is implemented below the public tool layer: Runtime owns repository-scoped TaskArtifactFocus CAS/dedup/lifecycle/history, and Search consumes only Active Focus. #152 remains unimplemented, so no MCP/Skill Focus submission entry exists. Repository Catalog is local-only; team synchronization is not claimed.

## Residue gates

The M1–M3 gate searches product code, tests, fixtures, scripts, and docs (excluding build output and the user-owned `readme.md`) for:

- the removed preferred-Space request/ranking field and CLI spelling;
- removed Space ranking cursor fields and exact-Space match reason;
- the removed Workspace-to-Space binding type and command surface;
- the removed Context-Propose API/constructor spelling.
- the removed task-text MCP bridge, bare-query Agent action, legacy Hook lookup helper, and non-Task automatic Context Pack CLI surface.
- the removed textual TaskSignal channel/path that previously looked like an Engineering Graph edge.
- the removed Repository auto-registration types, remote/declared/common-dir merge hints, and Hook `git rev-parse` discovery path;
- any `TaskArtifactFocus`/MCP Graph-query implementation deferred to #151/#152.

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
cargo test --locked -p sctx-cli --test cli_contract
cargo test --locked -p sctx-cli --test hook_fail_open
cargo test --locked -p sctx-cli --test milestone_one_contract
cargo test --locked -p sctx-cli --test milestone_two_contract
cargo test --locked -p sctx-cli --test milestone_three_contract

# Required repository gates
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --locked
(cd npm && npm test)
```

Current repository gate results:

- `cargo fmt --all -- --check`: passed.
- `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`: passed with no warnings.
- `cargo test --workspace --locked`: 280 passed, 0 failed, 1 ignored manual benchmark.
- `npm test`: 16 passed, 0 failed, 0 skipped.
- Shared Context Skill `quick_validate.py`: passed (`Skill is valid!`).

The complete historical V1 storage, lifecycle, installer, adapter, and NPM regression coverage remains in the workspace suites. M1–M3 establish Task-first runtime retrieval and Engineering Graph truth; they do not claim automatic Capture behavior.
