# M1 Task-first Integration Acceptance Report

Date: 2026-08-20
Scope: Mew #112 and #113
Baseline: `main@e7f48f8`

## Verdict

M1 closes the Task-first domain and entry-point foundation. The executable product surface no longer asks a caller or Workspace to choose a preferred Space, and unassigned Candidate creation is the only new-knowledge entry path exposed to Agents.

Session startup is capability-only: it does not run an empty automatic knowledge query. Task-aware automatic retrieval begins only from a supported PromptSubmit event.

This report does **not** claim M2, M3, or M4:

| Milestone | Status | Boundary |
|---|---|---|
| M1 — Task-first primitives and unassigned Candidate entry | IMPLEMENTED | Covered below |
| M2 — Task Runtime and multi-Space retrieval | NOT IMPLEMENTED | No TaskSession persistence, dynamic association engine, Space Intent retrieval, or association-bearing TaskContextPack |
| M3 — Engineering Graph | NOT IMPLEMENTED | No Artifact scanner, Context-to-code association, resolution rebuild, or graph expansion |
| M4 — Low-tax Capture | NOT IMPLEMENTED | No automatic WorkEpisode aggregation, Candidate Builder, Space recommendation, deduplication, or confirmation workflow |

The current `context_for_task` tool is a route-free task-text Context Pack over the existing knowledge search. It is not the M2 dynamic Task Runtime. The current `candidate_create` tool is a manual, unassigned M1 seam. It is not the M4 automatic Capture pipeline.

## M1 acceptance matrix

| Requirement | Result | Authoritative evidence |
|---|---|---|
| `TaskIntent` has no Space or Workspace route | PROVEN | `sctx-domain` serialization contract and `milestone_one_contract::task_intent_has_no_route_and_accepts_zero_or_many_space_associations` |
| One Task accepts zero or multiple Space associations | PROVEN | `TaskSpaceAssociation::validate_collection` domain contract and the cross-crate milestone test |
| Search request has no preferred-Space ranking input | PROVEN | `SearchRequest` contract; Search cursor/ranking contains only BM25, Evidence completeness, and stable IDs |
| Explicit exploration can still hard-filter by Space | PROVEN | MCP contract sends `context_search.space_ids`; `SearchFilters.space_ids` is applied in SQL before ranking |
| `context_for_task` cannot be routed to a Space by the caller | PROVEN | MCP schema omits Space fields and `context_for_task_rejects_a_caller_supplied_space_route` verifies strict rejection |
| Workspace cannot select or persist a Space | PROVEN | `config.toml` contains only `version` and `store`; CLI has no binding command for a Workspace; milestone test verifies both |
| Candidate creation requires no Space | PROVEN | CLI and MCP `candidate_create` contracts reject Space fields and return an unassigned Candidate ID |
| `candidate_create` is the Agent-facing Candidate main path | PROVEN | CLI help, MCP tool list, CLI milestone test, and MCP client fixtures |
| Unconfirmed Candidate cannot enter automatic injection | PROVEN | Candidate projection is outside Context FTS; CLI/MCP candidate retrieval tests and Codex hook test return only Accepted eligible Context |
| Existing confirmed Context fixtures use neutral revision terminology | PROVEN | Event constructor is `context_revision_added`; no Context-Propose API or constructor remains |
| SessionStart emits capability guidance without knowledge retrieval | PROVEN | Shared lifecycle policy returns no Context query; the CLI adversarial contract seeds two Spaces with Accepted eligible Context and proves startup emits neither item before a supported PromptSubmit retrieves only its task match |

## Residue gates

The M1 gate searches product code, tests, fixtures, scripts, and docs (excluding build output and the user-owned `readme.md`) for:

- the removed preferred-Space request/ranking field and CLI spelling;
- removed Space ranking cursor fields and exact-Space match reason;
- the removed Workspace-to-Space binding type and command surface;
- the removed Context-Propose API/constructor spelling.

Expected result: zero matches. Generic target-design language such as a proposed new Space Intent is not a Context-Propose API.

## Reproduction commands

```bash
# Focused M1 contracts
cargo test --locked -p sctx-domain
cargo test --locked -p sctx-search --test search_contract
cargo test --locked -p sctx-mcp --test mcp_contract
cargo test --locked -p sctx-cli --test milestone_one_contract

# Required repository gates
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --locked
(cd npm && npm test)
```

Final M1 gate results on the stated baseline:

- `cargo fmt --check`: passed.
- `cargo clippy --workspace --all-targets`: passed with no warnings.
- `cargo test --workspace --locked`: 141 passed, 0 failed, 1 ignored manual benchmark.
- `npm test`: 16 passed, 0 failed, 0 skipped.

The complete historical V1 storage, lifecycle, installer, adapter, and NPM regression coverage remains in the workspace suites. M1 narrows the product-routing truth; it does not claim the later Task Runtime, Engineering Graph, or automatic Capture behavior.
