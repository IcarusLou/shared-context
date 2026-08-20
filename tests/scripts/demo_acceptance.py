#!/usr/bin/env python3
"""Independent black-box oracle for `sctx demo` and `setup --demo`."""

from __future__ import annotations

import argparse
import collections
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import time
import tomllib


ROOT = Path(__file__).resolve().parents[2]
ORACLE = json.loads((ROOT / "tests/oracles/demo-v1.json").read_text())
ID = re.compile(r"^(evt|spc|ctx|rev|evd|rvw|pub)_[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$")


def require(condition: bool, message: str) -> None:
    if not condition:
        raise AssertionError(message)


def run(binary: Path, home: Path, workspace: Path, *args: str) -> dict:
    completed = subprocess.run(
        [str(binary), "--json", *args],
        cwd=workspace,
        env={**os.environ, "HOME": str(home)},
        check=False,
        capture_output=True,
        text=True,
        timeout=180,
    )
    require(completed.returncode == 0, f"{args!r} failed: {completed.stderr}")
    return json.loads(completed.stdout)


def mcp(binary: Path, home: Path, workspace: Path, client: str, space_id: str) -> list[dict]:
    requests = [
        {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": client, "version": "acceptance"}}},
        {"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}},
        {"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "context_search", "arguments": {"query": ORACLE["context"]["search_query"], "space_ids": [space_id], "statuses": ["accepted"]}}},
    ]
    payload = "".join(json.dumps(request, separators=(",", ":")) + "\n" for request in requests)
    completed = subprocess.run(
        [str(binary), "mcp", "serve", "--client", client],
        cwd=workspace,
        env={**os.environ, "HOME": str(home)},
        input=payload,
        check=False,
        capture_output=True,
        text=True,
        timeout=30,
    )
    require(completed.returncode == 0, f"{client} MCP failed: {completed.stderr}")
    require(not completed.stderr, f"{client} MCP wrote stderr: {completed.stderr}")
    return [json.loads(line) for line in completed.stdout.splitlines()]


def git(repository: Path, *args: str) -> str:
    return subprocess.run(
        ["git", "-C", str(repository), *args],
        check=True,
        capture_output=True,
        text=True,
        timeout=30,
    ).stdout.strip()


def seed_user_configs(home: Path) -> None:
    files = {
        ".cursor/mcp.json": '{\n  "unknownTop": {"中文": true},\n  "mcpServers": {"existing": {"command": "keep cursor"}}\n}\n',
        ".cursor/hooks.json": '{\n  "version": 1,\n  "unknown": "keep cursor hook",\n  "hooks": {"stop": [{"command": "user-stop"}]}\n}\n',
        ".codex/config.toml": '# keep leading comment\nmodel = "fixture" # keep inline\n\n[mcp_servers.existing]\ncommand = "keep codex"\nunknown = true\n',
        ".codex/hooks.json": '{\n  "description": "keep codex hook",\n  "hooks": {"Stop": [{"matcher": "custom", "hooks": [{"type": "command", "command": "keep"}]}]}\n}\n',
    }
    for relative, contents in files.items():
        path = home / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(contents)


def assert_fixed_tree(repository: Path) -> tuple[str, str]:
    paths = [path for path in git(repository, "ls-tree", "-r", "--name-only", "HEAD").splitlines() if path.startswith("events/")]
    require(len(paths) == ORACLE["event_count"], f"expected exactly four events, got {paths}")
    events = [json.loads(git(repository, "show", f"HEAD:{path}")) for path in paths]
    require(collections.Counter(event["event_type"] for event in events) == ORACLE["event_types"], "event type multiset differs from oracle")
    event_ids = [event["event_id"] for event in events]
    require(len(set(event_ids)) == len(event_ids), "event IDs are not unique")
    for event in events:
        for key, value in event.items():
            if key.endswith("_id") and isinstance(value, str):
                require(bool(ID.fullmatch(value)), f"non-random or malformed {key}: {value}")
    created = next(event for event in events if event["event_type"] == "space.created")
    require(created["intent_revision"]["intent"]["title"] == ORACLE["space"]["title"], "space title differs from oracle")
    require(ORACLE["space"]["domain_term"] in created["intent_revision"]["intent"]["domain_terms"], "demo marker absent")
    revision_added = next(event for event in events if event["event_type"] == "context.revision_added")
    require(revision_added["revision"]["topic_key"] == ORACLE["context"]["topic_key"], "topic differs from oracle")
    require(revision_added["revision"]["statement"] == ORACLE["context"]["statement"], "statement differs from oracle")
    return created["space_id"], revision_added["context_id"]


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    args = parser.parse_args()
    source = args.binary.resolve()
    require(source.is_file(), f"binary not found: {source}")
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="sctx 验收 空格 ") as temporary:
        base = Path(temporary)
        home = base / "用户 HOME 中文"
        workspace = base / "项目 路径 中文"
        bundle = base / "离线 Bundle" / "sctx"
        home.mkdir()
        workspace.mkdir()
        subprocess.run(["git", "init", "-q", "-b", "acceptance-source", str(workspace)], check=True)
        (workspace / "业务源码.txt").write_text("ephemeral development input\n")
        subprocess.run(["git", "-C", str(workspace), "add", "--", "业务源码.txt"], check=True)
        subprocess.run(
            ["git", "-C", str(workspace), "-c", "user.name=Acceptance", "-c", "user.email=acceptance@example.invalid", "commit", "-q", "-m", "ephemeral development commit"],
            check=True,
        )
        bundle.parent.mkdir()
        shutil.copy2(source, bundle)
        bundle.chmod(0o755)
        seed_user_configs(home)

        first = run(bundle, home, workspace, "setup", "--demo", "--runtime-source", str(bundle), "--agents", "cursor,codex")
        require(first["command"] == "setup.demo", "setup --demo command envelope differs")
        require(first["data"]["demo"]["created_event_count"] == ORACLE["event_count"], "first demo must create exactly four facts")
        config_paths = [home / relative for relative in [".cursor/mcp.json", ".cursor/hooks.json", ".codex/config.toml", ".codex/hooks.json"]]
        after_first = {path: path.read_bytes() for path in config_paths}
        repository = home / ".shared-context/repository"
        first_head = git(repository, "rev-parse", "HEAD")

        for _ in range(2):
            repeated = run(bundle, home, workspace, "setup", "--demo", "--runtime-source", str(bundle), "--agents", "cursor,codex")
            require(repeated["data"]["demo"]["created_event_count"] == 0, "repeated setup --demo appended facts")
            require(git(repository, "rev-parse", "HEAD") == first_head, "repeated setup --demo changed HEAD")
            require({path: path.read_bytes() for path in config_paths} == after_first, "repeated setup --demo changed Agent config bytes")

        standalone = run(bundle, home, workspace, "demo")
        require(standalone["data"]["created_event_count"] == 0, "standalone demo is not idempotent")
        space_id, context_id = assert_fixed_tree(repository)
        source_branch = git(workspace, "rev-parse", "--abbrev-ref", "HEAD")
        source_commit = git(workspace, "rev-parse", "HEAD")
        require(source_branch == "acceptance-source", "business source branch differs")
        require(bool(re.fullmatch(r"[0-9a-f]{40}", source_commit)), "business source commit is malformed")

        local_config = tomllib.loads((home / ".shared-context/config.toml").read_text())
        require(Path(local_config["store"]) == repository, "config does not point to the sole repository")
        require(set(local_config) == {"version", "store"}, "config contains state beyond the sole repository")
        require("# keep leading comment" in (home / ".codex/config.toml").read_text(), "Codex TOML comment was lost")

        shutil.rmtree(workspace)
        require(not workspace.exists(), "business workspace was not deleted")
        inaccessible = subprocess.run(
            ["git", "-C", str(workspace), "cat-file", "-e", source_commit],
            check=False,
            capture_output=True,
            text=True,
            timeout=30,
        )
        require(inaccessible.returncode != 0, "deleted business commit is still accessible")

        searched = run(bundle, home, base, "search", "--query", ORACLE["context"]["search_query"], "--space-id", space_id, "--status", ORACLE["context"]["status"])
        results = searched["data"]["results"]
        require(len(results) == 1 and results[0]["context_id"] == context_id, "CLI search differs from oracle")
        require(results[0]["statement"] == ORACLE["context"]["statement"], "CLI search returned unexpected content")
        require(results[0]["evidence"][0]["supports"] == ORACLE["context"]["evidence_supports"], "CLI Search lost self-contained Evidence")
        require(results[0]["evidence"][0]["content"]["fixture"] == ORACLE["context"]["evidence_fixture"], "CLI Search Evidence depends on deleted source")

        for client in ORACLE["mcp_clients"]:
            responses = mcp(bundle, home, base, client, space_id)
            require(len(responses) == 3, f"{client} MCP response count differs")
            tools = [tool["name"] for tool in responses[1]["result"]["tools"]]
            require(tools == ORACLE["mcp_tools"], f"{client} MCP tools differ")
            structured = responses[2]["result"]["structuredContent"]
            require(not responses[2]["result"]["isError"], f"{client} MCP search failed")
            require(len(structured["results"]) == 1 and structured["results"][0]["context_id"] == context_id, f"{client} MCP search differs")
            evidence = structured["results"][0]["evidence"][0]
            require(evidence["supports"] == ORACLE["context"]["evidence_supports"], f"{client} MCP Search lost Evidence")
            require(evidence["content"]["fixture"] == ORACLE["context"]["evidence_fixture"], f"{client} MCP Evidence depends on deleted source")

        elapsed = time.monotonic() - started
        require(elapsed < 180, f"demo exceeded three minutes: {elapsed:.3f}s")
        print(json.dumps({"status": "PROVEN", "elapsed_seconds": round(elapsed, 3), "events": ORACLE["event_count"], "mcp_clients": ORACLE["mcp_clients"], "source_branch": source_branch, "source_commit": source_commit, "deletion_proven": True}, sort_keys=True))


if __name__ == "__main__":
    main()
