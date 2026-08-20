# Issue #96 Acceptance Report

Date: 2026-08-19
Baseline: `main@d3f48de`
Branch: `issue/96-acceptance`

## Verdict

All acceptance claims that can be automated on this Apple Silicon host are proven. No known P0/P1 defect remains. Claims requiring native Intel hardware, production signing/notarization, or publication to a real Registry are explicitly `NOT_PROVEN` below.

## Independent harness and oracle

The black-box harness is `tests/scripts/demo_acceptance.py`; its fixed expected values are in `tests/oracles/demo-v1.json`. The oracle does not turn CLI output into expected data. It independently:

- reads the committed Git Tree and asserts the exact four-event type multiset;
- validates random UUIDv4 IDs, fixed authoritative fixture text, and the sole repository path;
- preserves seeded unknown Cursor/Codex fields and Codex TOML comments, then compares Agent config bytes across the second and third `setup --demo` runs;
- drives CLI Search and two independent MCP sessions (Cursor and Codex client selection); distinct framing is covered by the Rust MCP contract;
- deletes the original business Git workspace, branch, and commit, then proves the published Context remains searchable;
- enforces an end-to-end wall-clock limit of 180 seconds.

`tests/scripts/run-acceptance.sh` is the unified acceptance entry point. It routes each claim to the existing focused test suite, then runs the black-box oracle, all 12 NPM tests, and the release 100k benchmark.

## Environment

| Item | Measured value |
|---|---|
| Mac | Mac15,7 |
| CPU | Apple M3 Pro, 12 logical CPUs, arm64 |
| Memory | 38,654,705,664 bytes (36 GiB) |
| macOS | 15.7.7 (24G720), Darwin 24.6.0 |
| Filesystem | APFS on SSD, Apple Fabric |
| Rust | rustc/cargo 1.97.1 |
| Node / NPM | Node 22.15.0 / NPM 10.9.2 |
| Git | 2.55.115 |

## 18.1 Installation and environment

| Claim | Result | Evidence |
|---|---|---|
| arm64 offline `setup --demo` through Publish/Search in under 3 minutes | PROVEN | NPM test `arm64 real CLI completes the offline setup --demo loop without Registry access` completed in 17.289s in the final unified run; complete black-box deletion oracle elapsed 14.025s |
| No sudo, remote Git, external account, or API key | PROVEN | Offline test uses a sandbox HOME, local Git and loopback-dead Registry; harness creates no remote and supplies no credential |
| arm64 offline install/Setup/Append/CLI/MCP/rebuild/uninstall on one artifact | PROVEN | One NPM offline-bundle smoke runs `setup --demo`, Search, `index rebuild`, and uninstall with an unreachable Registry and empty cache; it verifies Tree/generation, retained repository, removed runtime, removed owned config, and preserved user config |
| x64 package and offline-bundle structure | PROVEN | NPM package metadata, thin Mach-O, checksum, local-only lockfile and archive tests |
| Setup three times and `setup --demo` three times do not duplicate config or facts | PROVEN | `setup_three_times_is_idempotent_and_preserves_existing_configuration`; black-box byte/HEAD/event assertions |
| Spaces and Chinese characters in HOME/workspace/runtime paths | PROVEN | installer fixture and demo oracle use `用户 HOME 中文`, `项目 路径 中文`, and `离线 Bundle` |
| Exactly one Context Git repository | PROVEN | fixed-store tests plus direct oracle assertion of `config.toml.store`; `setup --demo` explicitly reuses the setup root |
| Every setup write seam rolls Agent config back byte-for-byte; uninstall preserves post-setup user edits | PROVEN | `every_setup_write_seam_restores_exact_agent_bytes_and_permissions`; `uninstall_removes_only_exact_owned_entries_and_retains_repository` |

## 18.2 Git and events

| Claim group | Result | Evidence |
|---|---|---|
| Product APIs create only new random-ID facts and expose no caller path/ID override | PROVEN | event generation contract; MCP tool schema contract; CLI lifecycle tests |
| 100 concurrent proposals produce 100 distinct files/IDs | PROVEN | `one_hundred_concurrent_proposals_create_distinct_files_without_overwrite` |
| Explicit path staging; foreign staged/untracked files excluded | PROVEN | `commit_contains_only_explicit_batch_paths_and_leaves_untracked_files_alone`; `foreign_staged_path_is_rejected_without_changing_it` |
| Managed M/D/R and pending object overwrite are rejected with actionable errors | PROVEN | Writer M/D/R and object reuse tests; CLI pending/validate contract |
| Revision, withdrawal, publication conflicts, and semantic-conflict resolution are new events with explicit causal heads | PROVEN | reducer fixtures and CLI lifecycle/semantic-resolution tests |
| Recovery trusts only valid journal Path/Hash and rejects foreign/partial batches | PROVEN | pending recovery rejection tests |
| All 13 Writer crash seams retain Event ID/content, at most one semantic commit, and converge HEAD/index | PROVEN | `every_crash_seam_recovers_stable_content_with_at_most_one_semantic_commit` |

The crash matrix covers `AfterJournal`, before/after create, add and commit, before/after commit-OID persistence, before/after index update, and before/after cleanup.

## 18.3 Domain stability

| Claim | Result | Evidence |
|---|---|---|
| Context/Evidence survives deletion of the business workspace, branch and commit | PROVEN | black-box oracle deletes its source Git workspace before the final successful Search |
| annotations/origin_hint presence and stripping reduce identically | PROVEN | `annotations_do_not_participate_in_reduction`; semantic-hash metadata exclusion test |
| Arbitrary event order yields byte-identical projection | PROVEN | full permutation/property tests and shuffled discovery-order index test |
| Concurrent Publication Heads are an explicit governance conflict; time/commit/path never resolves it | PROVEN | publication lifecycle fixture and arbitrary-order tests |
| Confirmed overlapping accepted decisions block both sides and return both conflicts | PROVEN | semantic-conflict reducer fixture and Search conflict contract |

## 18.4 SQLite and query

| Claim | Result | Evidence |
|---|---|---|
| Missing/corrupt DB rebuilds deterministically from the current Git Tree | PROVEN | missing/corrupt/unknown-version rebuild tests and projection dumps |
| Space, accepted Context, conflict, FTS and stable order match after rebuild | PROVEN | `sqlite_rebuild` projection dump plus Search rebuild-order contract |
| Every append and manual M/D/R path matches scratch rebuild, excluding documented operational warnings | PROVEN | incremental closure, reverse closure, duplicate append, and manual M/D/R tests |
| Query/Index/Rebuild/Git Append concurrency converges to HEAD Tree without generation regression | PROVEN | `concurrent_query_index_rebuild_and_append_converge_without_generation_regression` |
| Long-lived readers reopen after DB replacement | PROVEN | `long_lived_query_connection_reopens_after_corrupt_file_replacement` |
| Multi-page data stays on one Tree/generation/read transaction | PROVEN | paginated snapshot and Search cursor-page contracts |
| Response includes indexed Tree, generation, match reason and conflict markers | PROVEN | Search and MCP contracts |
| 100k Warm Search P95 < 100ms | PROVEN | release benchmark below |

Fixed benchmark corpus: 100,000 accepted decision rows; every tenth row contains `needle searchResultParser 中文检索`; domain filter `search`; page size 20; one warm-up plus 30 measured samples per query.

| Query | Warm P50 | Warm P95 |
|---|---:|---:|
| `needle` | 39.873 ms | 41.606 ms |
| `search_result_parser` | 43.765 ms | 45.875 ms |
| `中文检索` | 41.400 ms | 42.387 ms |
| empty query + filters | 73.624 ms | 75.114 ms |

Worst measured P95: **75.114 ms**, 24.886 ms below the 100 ms gate. The benchmark now asserts the threshold rather than only printing timings.

## 18.5 Agent and configuration

| Claim | Result | Evidence |
|---|---|---|
| Cursor and Codex Initialize/List Tools/Search/Get/Propose | PROVEN | client-specific `mcp_contract` sessions; black-box demo independently searches through both clients |
| Cursor 3.13 and Codex 0.147 real Hook payload fixtures | PROVEN | adapter payload contract suites under `fixtures/agents` |
| CLI/MCP work when Hook is unavailable | PROVEN | capability fallback and direct CLI/MCP contracts |
| Untrusted Codex Hook shows `ACTION REQUIRED` | PROVEN | adapter, CLI capability, and installer Doctor tests |
| A hanging `cursor/codex --version` cannot block setup | PROVEN | production probes now have a two-second hard timeout; `agent_version_probe_has_a_hard_timeout` prevents regression |
| Uninstall restores only owned Agent config and retains repository | PROVEN | installer uninstall matrix including post-setup user edits |

## claims_proven

- `18.1.arm64_offline_setup_demo_under_180s`
- `18.1.no_sudo_remote_git_account_or_api_key`
- `18.1.arm64_offline_lifecycle`
- `18.1.x64_bundle_structure_and_checksums`
- `18.1.setup_and_demo_idempotency`
- `18.1.space_and_chinese_paths`
- `18.1.single_context_store_repository`
- `18.1.config_transaction_and_uninstall_merge`
- `18.2.append_only_api_and_random_ids`
- `18.2.writer_100_concurrent_proposals`
- `18.2.explicit_path_staging_and_mdr_rejection`
- `18.2.journal_recovery_and_all_crash_seams`
- `18.3.source_workspace_deletion_independence`
- `18.3.annotations_equivalence`
- `18.3.event_permutation_invariance`
- `18.3.explicit_publication_and_semantic_conflicts`
- `18.4.db_delete_corrupt_rebuild`
- `18.4.incremental_scratch_mdr_equivalence`
- `18.4.concurrent_snapshot_and_reader_reopen`
- `18.4.response_provenance_and_stable_pagination`
- `18.4.release_100k_warm_search_p95_under_100ms`
- `18.5.cursor_codex_mcp_and_hook_contracts`
- `18.5.hook_fallback_and_codex_action_required`
- `18.5.uninstall_retains_repository`
- `p1.agent_version_probe_timeout_and_fallback`

## claims_not_proven

- `18.1.intel_x64_native_offline_execution`: `NOT_PROVEN` — no native Intel x64 Mac was available. Cross-built x64 thin Mach-O structure, signature-verification behavior, package metadata, checksums and offline lockfile are proven; Rosetta/cross-build is not reported as native evidence.
- `18.1.production_developer_id_signature_and_notarization`: `NOT_PROVEN` — tests use local ad-hoc signing. No production Developer ID, notarization, Gatekeeper distribution, or release certificate was exercised.
- `18.1.registry_publish_and_clean_registry_install`: `NOT_PROVEN` — no package was published to a real Registry and no production Registry install was performed. Local pack and an offline install with the Registry forced to unreachable loopback are proven.

## Reproduction commands

```bash
# Unified focused acceptance gate
tests/scripts/run-acceptance.sh

# Independent demo oracle
cargo build --locked -p sctx-cli
python3 tests/scripts/demo_acceptance.py --binary target/debug/sctx

# Full workspace gates
cargo fmt --all -- --check
cargo metadata --no-deps --format-version 1
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --locked

# NPM 12-test gate
(cd npm && npm test)

# Release 100k benchmark
cargo test --release --locked -p sctx-search warm_search_benchmark_baseline -- --ignored --nocapture
```

## Test totals and measured elapsed

- Rust workspace: 115 passed, 0 failed, 1 ignored benchmark (run separately in release mode).
- NPM: 12 passed, 0 failed, 0 skipped.
- Release benchmark: 1 passed, 4 fixed queries × 30 measured samples.
- Independent black-box demo oracle: 1 passed.
- Distinct automated cases/gates: **129 passed** (`115 + 12 + 1 + 1`).
- Unified focused acceptance runner: 373.87s wall time (includes 127.62s Writer concurrency matrix, 46.972s NPM gate, and 7.46s benchmark execution).
- Full workspace `fmt + metadata + check + clippy + test`: 231.40s wall time.
