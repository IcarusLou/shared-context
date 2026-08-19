#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
cd "$ROOT"

# 18.2-18.3: independent fixtures for metadata equivalence, arbitrary ordering,
# append-only writer seams, explicit Git paths, and 100 concurrent proposals.
cargo test --locked -p sctx-event-schema --test reducer_contract
cargo test --locked -p sctx-git-store --test git_writer

# 18.4: delete/corrupt recovery, incremental/scratch and M/D/R equivalence,
# pinned snapshots, long-lived readers, and concurrent convergence.
cargo test --locked -p sctx-index --test sqlite_rebuild
cargo test --locked -p sctx-search --test search_contract

# 18.5: both MCP client contracts, real Hook payload fixtures, Codex trust,
# setup rollback/idempotency and uninstall repository retention.
cargo test --locked -p sctx-mcp --test mcp_contract
cargo test --locked -p sctx-adapter-cursor --test payload_contract
cargo test --locked -p sctx-adapter-codex --test payload_contract
cargo test --locked -p sctx-installer --test installer_matrix
cargo test --locked -p sctx-cli --test cli_contract
cargo test --locked -p sctx-cli --test proactive_skill_e2e

# 18.1 and the end-to-end loop: the Python oracle reads Git/config directly and
# speaks MCP itself; it never derives expected values from production output.
cargo build --locked -p sctx-cli
python3 tests/scripts/demo_acceptance.py --binary target/debug/sctx

# arm64 execution plus arm64/x64/offline package structure (12 Node tests).
(cd npm && npm test)

# Fixed 100k-row, four-query, 30-sample release benchmark with an asserted P95.
cargo test --release --locked -p sctx-search warm_search_benchmark_baseline -- --ignored --nocapture
