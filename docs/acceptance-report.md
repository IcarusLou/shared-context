# Task-first Retrieval Integration Acceptance Report

Date: 2026-08-20
Scope: Mew #112 through #145
Implementation head before M2 acceptance: `main@1951803`

## Verdict

M1 closes the Task-first domain and entry-point foundation. M2 closes the local Task Runtime and explainable multi-Space retrieval path. M3 is partially implemented through a local Repository Registry, bounded scanner, persistent Engineering References, rebuildable resolution projection, graph retrieval, and explicit MCP/CLI workflows.

Session startup is capability-only: it does not run an empty automatic knowledge query. Task-aware automatic retrieval begins only from a supported PromptSubmit event.

This report does **not** claim M4 or final M3 milestone closure:

| Milestone | Status | Boundary |
|---|---|---|
| M1 — Task-first primitives and unassigned Candidate entry | IMPLEMENTED | Covered below |
| M2 — Task Runtime and multi-Space retrieval | IMPLEMENTED | TaskSession persistence, strict `task_intent_update`, read-only `task_context`, association inference, typed RetrievalPaths, and TaskContextPack pass focused cross-crate/E2E oracles |
| M3 — Engineering Graph | IN PROGRESS | Registry, Scanner, Reference Event, Resolver, generation-pinned Projection, Task Retrieval, and MCP/CLI workflows are implemented; final milestone acceptance remains |
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
| Workspace cannot select or persist a Space | PROVEN | `config.toml` contains only `version` and `store`; CLI has no binding command for a Workspace; milestone test verifies both |
| Candidate creation requires no Space | PROVEN | CLI and MCP `candidate_create` contracts reject Space fields and return an unassigned Candidate ID |
| `candidate_create` is the Agent-facing Candidate main path | PROVEN | CLI help, MCP tool list, CLI milestone test, and MCP client fixtures |
| Unconfirmed Candidate cannot enter automatic injection | PROVEN | Candidate projection is outside Context FTS; CLI/MCP candidate retrieval tests and Codex hook test return only Accepted eligible Context |
| Existing confirmed Context fixtures use neutral revision terminology | PROVEN | Event constructor is `context_revision_added`; no Context-Propose API or constructor remains |
| SessionStart emits capability guidance without knowledge retrieval | PROVEN | Shared lifecycle policy returns no Task Runtime operation; the CLI adversarial contract seeds two Spaces with Accepted eligible Context and proves startup emits neither item before a supported PromptSubmit retrieves only its task match |
| Codex dynamic sessions are isolated and incorporate later File/Test signals | PROVEN | CLI adversarial contract runs two Codex sessions in one Git Workspace against distinct Spaces, proves pre-Prompt PostToolUse cannot create a Session, rejects cross-session/out-of-Workspace File and Context leakage, proves Prompt/Workspace/Repository persistence, and observes new exact File/Test retrieval paths without raw payload retention |
| Task retrieval accepts no caller Space or Workspace route | PROVEN | `milestone_two_contract` serializes the accepted input and rejects injected `space_id`, `space_ids`, `workspace`, and `workspace_id` fields |
| Task retrieval returns zero, one, or many Space candidates | PROVEN | The M2 oracle executes unrelated, page-only, and four-Space FE tasks through the shared Runtime/MCP/Search/Index path |
| One FE task retrieves page, server protocol, compatibility, and analytics Context | PROVEN | The M2 fixture combines Intent, File, API, Schema, Test, and Applicability evidence and asserts the exact four Space/Context identities |
| Every returned Context links to a Space association and typed RetrievalPath | PROVEN | The M2 oracle validates Intent FTS, Context FTS, Scope, and exact TaskSignal variants; M3 workflow tests additionally validate EngineeringGraph paths and artifact generation |
| Same Workspace external Sessions remain isolated | PROVEN | Page and server calls share one Workspace signal but receive distinct TaskSession/Task IDs and disjoint Space/Context sets |
| Workspace and local Repository paths cannot create Space priors | PROVEN | Search contract injects matching knowledge terms into both location signals and still returns zero associations; reordered checkout paths do not change the Task fingerprint |
| PostTool File/Test observations update later retrieval paths | PROVEN | A real Codex Hook Prompt→PostTool→Prompt sequence retains one Task ID, changes its fingerprint, adds a second Space, and returns typed File/Test paths |
| Automatic TaskContextPack excludes every unsafe state | PROVEN | The oracle seeds an unassigned Candidate plus Space-associated Candidate, Deprecated, semantic-conflict, and incomplete-Evidence Context; automatic output is empty while a direct SearchEngine diagnostic query proves each fixture state exists |
| Tree, Generation, and Task fingerprint are consistent | PROVEN | M2 response Tree equals Git `HEAD^{tree}` and index metadata; Generation equals the same projection; identical Session input returns identical fingerprint, associations, items, and paths |
| Engineering workflows preserve identity, privacy, and ambiguity | PROVEN | Two-Repository/multi-worktree tests execute scan→record→rebuild→explain→Task Pack; concurrent Writer calls produce unique server-owned IDs; unsafe paths, incomplete evidence, and secrets are rejected; ambiguous candidates are returned without selection |
| Engineering failure is advisory to Task Retrieval | PROVEN | Rebuild reports unavailable registered Repositories explicitly, Explain reports typed projection availability, and a corrupt Engineering projection degrades Task responses to `artifact_generation: null` instead of blocking Context-only retrieval |

## Residue gates

The M1/M2 gate searches product code, tests, fixtures, scripts, and docs (excluding build output and the user-owned `readme.md`) for:

- the removed preferred-Space request/ranking field and CLI spelling;
- removed Space ranking cursor fields and exact-Space match reason;
- the removed Workspace-to-Space binding type and command surface;
- the removed Context-Propose API/constructor spelling.
- the removed task-text MCP bridge, bare-query Agent action, legacy Hook lookup helper, and non-Task automatic Context Pack CLI surface.

Expected result: zero matches. Generic target-design language such as a proposed new Space Intent is not a Context-Propose API. `context_search` and its explicit `space_ids` hard filter are intentionally present.

## Reproduction commands

```bash
# Focused M1/M2 contracts
cargo test --locked -p sctx-domain
cargo test --locked -p sctx-search --test search_contract
cargo test --locked -p sctx-mcp --test mcp_contract
cargo test --locked -p sctx-mcp --test engineering_workflows
cargo test --locked -p sctx-engineering-graph
cargo test --locked -p sctx-task-runtime --test runtime_store
cargo test --locked -p sctx-cli --test cli_contract
cargo test --locked -p sctx-cli --test milestone_one_contract
cargo test --locked -p sctx-cli --test milestone_two_contract

# Required repository gates
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --locked
(cd npm && npm test)
```

Current repository gate results:

- `cargo fmt --all -- --check`: passed.
- `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`: passed with no warnings.
- `cargo test --workspace --locked`: 178 passed, 0 failed, 1 ignored manual benchmark.
- `npm test`: 16 passed, 0 failed, 0 skipped.

The complete historical V1 storage, lifecycle, installer, adapter, and NPM regression coverage remains in the workspace suites. M1/M2 establish the Task-first runtime retrieval truth; they do not claim the later Engineering Graph or automatic Capture behavior.
