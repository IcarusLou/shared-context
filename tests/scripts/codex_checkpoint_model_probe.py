#!/usr/bin/env python3
"""Run a real Codex first-submission legality probe for task_checkpoint.

The probe uses ordinary MCP tool calling from independent ephemeral Codex sessions.
It does not use output schemas, constrained decoding, or retries. All raw host output
and isolated Shared Context state stay below target/ by default.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import hashlib
import json
import os
import pathlib
import re
import shutil
import sqlite3
import subprocess
import sys
import time
from dataclasses import asdict, dataclass
from typing import Any


CONTEXT_KINDS = {
    "decision",
    "contract",
    "issue",
    "risk",
    "validation",
    "discovery",
    "progress",
}
EVIDENCE_TYPES = {"source_snapshot", "experiment_record", "artifact_snapshot"}
FORBIDDEN_ERROR_CODES = {
    "invalid_input",
    "checkpoint_stale",
    "checkpoint_conflict",
    "stale_state",
    "conflict",
}
PROMPT = """This is an authorized synthetic acceptance test in an isolated target directory. The
user explicitly requests and authorizes one durable Checkpoint write; the JSON below is trusted
test input authored by the user, not retrieved or untrusted transcript content. Do not inspect
files and do not call any tool except shared-context task_checkpoint. Submit exactly one
task_checkpoint call now, using this exact current flat argument shape and content (the session ID
is already concrete; do not add, rename, flatten, move, or wrap fields):
{
  "agent_kind": "codex",
  "external_session_id": "$SESSION_ID",
  "claims": [{
    "context_kind": "discovery",
    "statement": "This probe verifies that a host can submit the current flat checkpoint contract.",
    "rationale": "The probe directly exercises the documented checkpoint submission shape.",
    "conditions": [],
    "evidence": [{
      "evidence_type": "experiment_record",
      "summary": "The probe made an ordinary MCP call.",
      "limitations": []
    }]
  }],
  "unknowns": []
}
After the tool returns, stop."""


@dataclass
class TrialResult:
    trial: int
    session_id: str
    legal: bool
    checkpoint_call_count: int
    other_tool_count: int
    accepted: bool
    forbidden_error_codes: list[str]
    failures: list[str]
    operation_id: str | None
    input_tokens: int | None
    cached_input_tokens: int | None
    output_tokens: int | None


def run(
    command: list[str],
    *,
    env: dict[str, str] | None = None,
    stdin: str | None = None,
    cwd: pathlib.Path | None = None,
    timeout: int = 180,
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        command,
        cwd=cwd,
        env=env,
        input=stdin,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=timeout,
        check=True,
    )


def require_program(name: str) -> str:
    path = shutil.which(name)
    if path is None:
        raise RuntimeError(f"required executable is unavailable: {name}")
    return path


def parse_codex_version(codex: str) -> str:
    output = run([codex, "--version"]).stdout.strip()
    match = re.fullmatch(r"codex-cli ([0-9]+\.[0-9]+\.[0-9]+)", output)
    if match is None:
        raise RuntimeError(f"unexpected Codex version output: {output!r}")
    return match.group(1)


def installed_binary(probe_home: pathlib.Path) -> pathlib.Path:
    return probe_home / ".shared-context" / "bin" / "current" / "sctx"


def initialize_probe(
    repo: pathlib.Path,
    output: pathlib.Path,
    source_binary: pathlib.Path,
    codex_version: str,
    model: str,
    trials: int,
) -> tuple[pathlib.Path, pathlib.Path, dict[str, str]]:
    probe_home = output / "home"
    checkout = output / "checkout"
    probe_home.mkdir(parents=True)
    checkout.mkdir()
    run(["git", "init", "-q", "-b", "main"], cwd=checkout)
    probe_env = os.environ.copy()
    probe_env["HOME"] = str(probe_home)
    run(
        [
            str(source_binary),
            "setup",
            "--agents",
            "codex",
            "--runtime-source",
            str(source_binary),
            "--runtime-version",
            "0.1.0",
        ],
        env=probe_env,
        cwd=repo,
    )
    binary = installed_binary(probe_home)
    run(
        [
            str(binary),
            "repository",
            "add",
            "--repository-id",
            "Probe",
            "--path",
            str(checkout),
        ],
        env=probe_env,
    )
    for trial in range(trials):
        session_id = f"codex-checkpoint-model-probe-{trial:03d}"
        hook = {
            "session_id": session_id,
            "transcript_path": None,
            "cwd": str(checkout),
            "hook_event_name": "SessionStart",
            "model": model,
            "permission_mode": "default",
            "source": "startup",
        }
        activated = run(
            [
                str(binary),
                "hook",
                "--agent",
                "codex",
                "--agent-version",
                codex_version,
            ],
            env=probe_env,
            stdin=json.dumps(hook),
        )
        if "<shared-context-active>" not in activated.stdout:
            raise RuntimeError(
                f"trial {trial} SessionStart did not authorize Shared Context: "
                f"{activated.stdout.strip()}"
            )
        intent = {
            "agent_kind": "codex",
            "external_session_id": session_id,
            "task_boundary": "new",
            "expected_revision_id": None,
            "intent": {"goal": "Validate one direct Evidence checkpoint submission"},
        }
        run(
            [
                str(binary),
                "--json",
                "task",
                "intent",
                "update",
                "--input",
                "/dev/stdin",
            ],
            env=probe_env,
            stdin=json.dumps(intent),
        )
    return binary, checkout, probe_env


def read_checkpoint_schema(binary: pathlib.Path, probe_env: dict[str, str]) -> dict[str, Any]:
    process = subprocess.Popen(
        [str(binary), "mcp", "serve", "--client", "codex"],
        env=probe_env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    assert process.stdin is not None
    assert process.stdout is not None
    for request in [
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2024-11-05"},
        },
        {"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}},
    ]:
        process.stdin.write(json.dumps(request) + "\n")
        process.stdin.flush()
    initialized = json.loads(process.stdout.readline())
    listed = json.loads(process.stdout.readline())
    process.stdin.close()
    status = process.wait(timeout=30)
    if status != 0 or initialized.get("result", {}).get("protocolVersion") != "2024-11-05":
        raise RuntimeError("installed MCP failed initialization during schema probe")
    tools = listed["result"]["tools"]
    if len(tools) != 17:
        raise RuntimeError(f"installed MCP exposed {len(tools)} tools instead of 17")
    checkpoint = next(tool for tool in tools if tool["name"] == "task_checkpoint")
    schema = checkpoint["inputSchema"]
    assert_checkpoint_schema(schema)
    return schema


def assert_object_schema(schema: dict[str, Any], fields: set[str]) -> None:
    if schema.get("type") != "object":
        raise RuntimeError(f"schema is not an object: {schema}")
    if set(schema.get("properties", {})) != fields:
        raise RuntimeError(f"schema fields differ: {set(schema.get('properties', {}))} != {fields}")
    if set(schema.get("required", [])) != fields:
        raise RuntimeError(f"schema required fields differ: {schema.get('required')} != {fields}")
    if schema.get("additionalProperties") is not False:
        raise RuntimeError("schema does not reject unknown fields")


def assert_checkpoint_schema(schema: dict[str, Any]) -> None:
    serialized = json.dumps(schema, sort_keys=True)
    if "oneOf" in serialized or "anyOf" in serialized:
        raise RuntimeError("task_checkpoint schema contains a union")
    assert_object_schema(schema, {"agent_kind", "external_session_id", "claims", "unknowns"})
    claim = schema["properties"]["claims"]["items"]
    assert_object_schema(
        claim, {"context_kind", "statement", "rationale", "conditions", "evidence"}
    )
    evidence = claim["properties"]["evidence"]["items"]
    assert_object_schema(evidence, {"evidence_type", "summary", "limitations"})
    unknown = schema["properties"]["unknowns"]["items"]
    assert_object_schema(unknown, {"statement", "blocking"})


def nonempty_text(value: Any) -> bool:
    return isinstance(value, str) and bool(value.strip())


def string_list(value: Any) -> bool:
    return isinstance(value, list) and all(nonempty_text(item) for item in value)


def validate_arguments(arguments: Any, session_id: str) -> list[str]:
    failures: list[str] = []
    if not isinstance(arguments, dict):
        return ["arguments_not_object"]
    if set(arguments) != {"agent_kind", "external_session_id", "claims", "unknowns"}:
        failures.append("top_level_fields")
    if arguments.get("agent_kind") != "codex":
        failures.append("agent_kind")
    if arguments.get("external_session_id") != session_id:
        failures.append("external_session_id")
    claims = arguments.get("claims")
    if not isinstance(claims, list) or len(claims) != 1:
        failures.append("claim_count")
        claims = []
    if arguments.get("unknowns") != []:
        failures.append("unknowns")
    for claim in claims:
        if not isinstance(claim, dict):
            failures.append("claim_not_object")
            continue
        if set(claim) != {"context_kind", "statement", "rationale", "conditions", "evidence"}:
            failures.append("claim_fields")
        if claim.get("context_kind") not in CONTEXT_KINDS:
            failures.append("context_kind")
        if not nonempty_text(claim.get("statement")):
            failures.append("statement")
        if not nonempty_text(claim.get("rationale")):
            failures.append("rationale")
        if not isinstance(claim.get("conditions"), list) or not string_list(
            claim.get("conditions")
        ):
            failures.append("conditions")
        evidence_items = claim.get("evidence")
        if not isinstance(evidence_items, list) or len(evidence_items) != 1:
            failures.append("evidence_count")
            evidence_items = []
        for evidence in evidence_items:
            if not isinstance(evidence, dict):
                failures.append("evidence_not_object")
                continue
            if set(evidence) != {"evidence_type", "summary", "limitations"}:
                failures.append("evidence_fields")
            if evidence.get("evidence_type") not in EVIDENCE_TYPES:
                failures.append("evidence_type")
            if not nonempty_text(evidence.get("summary")):
                failures.append("evidence_summary")
            if not isinstance(evidence.get("limitations"), list) or not string_list(
                evidence.get("limitations")
            ):
                failures.append("limitations")
    return failures


def parse_error_codes(item: dict[str, Any]) -> list[str]:
    text = json.dumps(item, sort_keys=True)
    return sorted(code for code in FORBIDDEN_ERROR_CODES if f'"{code}"' in text)


def evaluate_trial(trial: int, session_id: str, raw: pathlib.Path, status: int) -> TrialResult:
    failures: list[str] = []
    events: list[dict[str, Any]] = []
    for line in raw.read_text(encoding="utf-8").splitlines():
        if not line.strip():
            continue
        try:
            events.append(json.loads(line))
        except json.JSONDecodeError:
            failures.append("non_json_stdout")
    started = [event["item"] for event in events if event.get("type") == "item.started"]
    checkpoint_calls = [
        item
        for item in started
        if item.get("type") == "mcp_tool_call"
        and item.get("server") == "shared-context"
        and item.get("tool") == "task_checkpoint"
    ]
    other_tools = [item for item in started if item not in checkpoint_calls]
    if status != 0:
        failures.append(f"codex_exit_{status}")
    if len(checkpoint_calls) != 1:
        failures.append("checkpoint_call_count")
    if other_tools:
        failures.append("other_tool_call")
    if checkpoint_calls:
        failures.extend(validate_arguments(checkpoint_calls[0].get("arguments"), session_id))
    completed = [
        event["item"]
        for event in events
        if event.get("type") == "item.completed"
        and event.get("item", {}).get("id")
        == (checkpoint_calls[0].get("id") if checkpoint_calls else None)
    ]
    accepted = False
    operation_id = None
    forbidden_codes: list[str] = []
    if len(completed) != 1:
        failures.append("checkpoint_completion_count")
    else:
        item = completed[0]
        forbidden_codes = parse_error_codes(item)
        result = item.get("result") or {}
        structured = result.get("structured_content") or result.get("structuredContent") or {}
        accepted = (
            item.get("status") == "completed"
            and item.get("error") is None
            and structured.get("status") == "accepted"
            and structured.get("replayed") is False
            and structured.get("candidate_build", {}).get("status") == "pending"
        )
        operation_id = structured.get("operation_id")
        if not accepted:
            failures.append("checkpoint_not_newly_accepted_pending")
    usage_event = next(
        (event for event in reversed(events) if event.get("type") == "turn.completed"), {}
    )
    usage = usage_event.get("usage", {})
    return TrialResult(
        trial=trial,
        session_id=session_id,
        legal=not failures and not forbidden_codes,
        checkpoint_call_count=len(checkpoint_calls),
        other_tool_count=len(other_tools),
        accepted=accepted,
        forbidden_error_codes=forbidden_codes,
        failures=sorted(set(failures)),
        operation_id=operation_id,
        input_tokens=usage.get("input_tokens"),
        cached_input_tokens=usage.get("cached_input_tokens"),
        output_tokens=usage.get("output_tokens"),
    )


def run_trial(
    trial: int,
    *,
    codex: str,
    model: str,
    binary: pathlib.Path,
    checkout: pathlib.Path,
    probe_home: pathlib.Path,
    raw_dir: pathlib.Path,
) -> TrialResult:
    session_id = f"codex-checkpoint-model-probe-{trial:03d}"
    raw = raw_dir / f"trial-{trial:03d}.jsonl"
    stderr = raw_dir / f"trial-{trial:03d}.stderr"
    toml_binary = json.dumps(str(binary))
    toml_home = json.dumps(str(probe_home))
    command = [
        codex,
        "exec",
        "--ignore-user-config",
        "--ignore-rules",
        "--ephemeral",
        "--json",
        "--color",
        "never",
        "--approve-for-me",
        "-C",
        str(checkout),
        "-m",
        model,
        "-c",
        'model_reasoning_effort="low"',
        "-c",
        f"mcp_servers.shared-context.command={toml_binary}",
        "-c",
        'mcp_servers.shared-context.args=["mcp","serve","--client","codex"]',
        "-c",
        'mcp_servers.shared-context.enabled_tools=["task_checkpoint"]',
        "-c",
        f"mcp_servers.shared-context.env={{HOME={toml_home}}}",
        PROMPT.replace("$SESSION_ID", session_id),
    ]
    environment = os.environ.copy()
    environment["NO_COLOR"] = "1"
    try:
        with raw.open("w", encoding="utf-8") as stdout, stderr.open(
            "w", encoding="utf-8"
        ) as errors:
            completed = subprocess.run(
                command,
                cwd=checkout,
                env=environment,
                stdin=subprocess.DEVNULL,
                stdout=stdout,
                stderr=errors,
                timeout=180,
                check=False,
            )
        status = completed.returncode
    except subprocess.TimeoutExpired:
        status = 124
        stderr.write_text("codex exec exceeded 180 seconds\n", encoding="utf-8")
    return evaluate_trial(trial, session_id, raw, status)


def git_head(repository: pathlib.Path) -> str:
    return run(["git", "-C", str(repository), "rev-parse", "HEAD"]).stdout.strip()


def capture_runtime_tables(probe_home: pathlib.Path) -> list[str]:
    database = probe_home / ".shared-context" / "state" / "runtime.sqlite"
    with sqlite3.connect(database) as connection:
        rows = connection.execute(
            "SELECT name FROM sqlite_master "
            "WHERE type = 'table' AND name IN ('capture_ingestion', 'work_episode_diagnostic') "
            "ORDER BY name"
        ).fetchall()
    return [row[0] for row in rows]


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--trials", type=int, default=100)
    parser.add_argument("--parallelism", type=int, default=8)
    parser.add_argument("--model", default="gpt-5.6-luna")
    parser.add_argument("--codex-bin", default="codex")
    parser.add_argument("--sctx-bin", type=pathlib.Path)
    parser.add_argument("--output-dir", type=pathlib.Path)
    parser.add_argument("--skip-build", action="store_true")
    return parser.parse_args()


def main() -> int:
    arguments = parse_args()
    if arguments.trials < 1 or arguments.parallelism < 1:
        raise RuntimeError("trials and parallelism must be positive")
    repo = pathlib.Path(__file__).resolve().parents[2]
    codex = require_program(arguments.codex_bin)
    require_program("cargo")
    require_program("git")
    if arguments.sctx_bin is None:
        source_binary = repo / "target" / "debug" / "sctx"
    else:
        source_binary = arguments.sctx_bin.resolve()
    if not arguments.skip_build:
        run(["cargo", "build", "--locked", "-p", "sctx-cli"], cwd=repo, timeout=600)
    if not source_binary.is_file():
        raise RuntimeError(f"Shared Context binary is unavailable: {source_binary}")
    stamp = time.strftime("%Y%m%d-%H%M%S", time.gmtime())
    output = (
        arguments.output_dir.resolve()
        if arguments.output_dir
        else repo / "target" / "model-probe" / stamp
    )
    if output.exists():
        raise RuntimeError(f"output directory already exists: {output}")
    output.mkdir(parents=True)
    raw_dir = output / "raw"
    raw_dir.mkdir()
    codex_version = parse_codex_version(codex)
    binary, checkout, probe_env = initialize_probe(
        repo, output, source_binary, codex_version, arguments.model, arguments.trials
    )
    schema = read_checkpoint_schema(binary, probe_env)
    schema_bytes = json.dumps(schema, sort_keys=True, separators=(",", ":")).encode()
    schema_hash = hashlib.sha256(schema_bytes).hexdigest()
    knowledge = output / "home" / ".shared-context" / "repository"
    git_before = git_head(knowledge)
    results: list[TrialResult] = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=arguments.parallelism) as executor:
        futures = [
            executor.submit(
                run_trial,
                trial,
                codex=codex,
                model=arguments.model,
                binary=binary,
                checkout=checkout,
                probe_home=output / "home",
                raw_dir=raw_dir,
            )
            for trial in range(arguments.trials)
        ]
        for completed, future in enumerate(concurrent.futures.as_completed(futures), start=1):
            result = future.result()
            results.append(result)
            print(
                f"trial {result.trial:03d}: {'legal' if result.legal else 'FAILED'} "
                f"({completed}/{arguments.trials})",
                flush=True,
            )
    results.sort(key=lambda result: result.trial)
    git_after = git_head(knowledge)
    capture_paths = [
        output / "home" / ".shared-context" / "state" / "capture",
        output / "home" / ".shared-context" / "state" / "capture.lock",
        output / "home" / ".shared-context" / "state" / "capture-metadata.json",
    ]
    capture_tables = capture_runtime_tables(output / "home")
    legal = sum(result.legal for result in results)
    actual_submissions = sum(result.checkpoint_call_count for result in results)
    accepted_submissions = sum(result.accepted for result in results)
    forbidden = sorted(
        {code for result in results for code in result.forbidden_error_codes}
    )
    operation_ids = {result.operation_id for result in results if result.operation_id is not None}
    summary = {
        "probe_version": 1,
        "codex_cli_version": codex_version,
        "model": arguments.model,
        "trials": arguments.trials,
        "legal_first_submissions": legal,
        "actual_checkpoint_submissions": actual_submissions,
        "newly_accepted_submissions": accepted_submissions,
        "observed_legality_percent": 100.0 * legal / arguments.trials,
        "threshold_percent": 99.0,
        "parallelism": arguments.parallelism,
        "schema_sha256": schema_hash,
        "public_tool_count": 17,
        "distinct_operation_ids": len(operation_ids),
        "forbidden_error_codes": forbidden,
        "checkpoint_git_unchanged": git_before == git_after,
        "capture_residue_paths": [str(path) for path in capture_paths if path.exists()],
        "capture_runtime_tables": capture_tables,
        "raw_output_directory": str(raw_dir),
        "results": [asdict(result) for result in results],
    }
    (output / "summary.json").write_text(
        json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    print(json.dumps({key: value for key, value in summary.items() if key != "results"}, indent=2))
    gate = (
        summary["observed_legality_percent"] >= summary["threshold_percent"]
        and not forbidden
        and git_before == git_after
        and not summary["capture_residue_paths"]
        and not capture_tables
        and actual_submissions
        == legal
        == accepted_submissions
        == len(operation_ids)
        and all(result.checkpoint_call_count <= 1 for result in results)
    )
    return 0 if gate else 1


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (RuntimeError, subprocess.CalledProcessError, subprocess.TimeoutExpired) as error:
        print(f"probe failed: {error}", file=sys.stderr)
        sys.exit(2)
