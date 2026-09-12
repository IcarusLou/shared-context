#!/usr/bin/env python3
"""Replay a real Codex session's human prompts into a fresh headless Codex session.

The point of this script is fidelity, not convenience: a replay is only worth
auditing when the run it produces could plausibly have been the original. So the
replay reuses the original's commit, sandbox policy, approval policy, model and
Shared Context repository identity, and it changes exactly three things it cannot
avoid changing -- an isolated ``HOME`` so no Shared Context state is written into
the operator's real installation, an isolated ``CODEX_HOME`` so the new rollout
lands inside the audit directory, and headless approvals so nobody has to sit at
the keyboard. Every one of those three is recorded in the manifest.

Usage::

    python3 tests/scripts/session_replay/replay.py --session <thread_id> \
        [--turns N|all] [--audit-root ~/.shared-context-audit] \
        [--checkout worktree|inplace] [--codex-home isolated|real] [--dry-run]

Nothing here writes into the repository under replay, into ``~/.shared-context``
or into ``~/.codex``: the replay's whole footprint is the audit root plus one
``git worktree``, attached to a fresh ``replay/<branch>`` so the replayed session
reports the same ``git_branch`` shape the original did.

Two environment facts are load-bearing and are recorded in the manifest rather
than assumed. A proxy exported by the calling shell is inherited by the Codex
child, and a proxy that serves ordinary HTTP can still reset the app-server's
stream -- measured: three prompts, zero output, and a manifest that described
three replayed turns. Use ``--proxy``/``--no-proxy`` to decide it deliberately;
``manifest.environment.proxy`` says what was in force, and ``manifest.run``
carries the run's verdict instead of leaving it to be inferred from warnings.
"""

from __future__ import annotations

import argparse
import contextlib
import datetime as dt
import glob
import hashlib
import json
import os
import pathlib
import re
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
from dataclasses import dataclass, field
from typing import Any, Iterator

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

import codex_trust  # noqa: E402

# Codex writes one JSON object per rollout line: {timestamp, ordinal, type, payload}.
ROLLOUT_GLOB = "sessions/*/*/*/rollout-*-{thread_id}.jsonl"

# A user message that opens with either of these is the host talking, not a human:
# AGENTS.md injection, or one of the `<recommended_plugins>` / `<environment_context>`
# style blocks Codex splices into the transcript.
HOST_PROMPT_PREFIXES = ("# AGENTS.md instructions", "<")

# Every Shared Context state directory is created 0700 by the installer, and
# `sctx` refuses to open an installation root or state directory that is not
# private -- it fails closed and the hook silently reports "disabled". A snapshot
# that forgets this produces a replay in which Shared Context never activates and
# nothing says why, so the mode is asserted rather than inherited from umask.
PRIVATE_DIR_MODE = 0o700

# Files under ~/.shared-context/state that must be snapshotted through SQLite's
# own backup API rather than copied, because a live writer may be mid-transaction.
SQLITE_SUFFIXES = (".sqlite",)
SQLITE_SIDECAR_SUFFIXES = ("-wal", "-shm", ".sqlite-wal", ".sqlite-shm")

# Codex home entries that are pure inputs (read-only caches, bundles, credentials)
# and can be shared with the real installation. Everything else -- sessions,
# history, the thread index -- must be fresh so the replay's rollout is isolated.
CODEX_HOME_SYMLINKS = (
    "plugins",
    "skills",
    "rules",
    "computer-use",
    "vendor_imports",
    "ambient-suggestions",
    "attachments",
    "browser",
    "node_repl",
    "memories",
)
CODEX_HOME_COPIES = (
    "auth.json",
    "hooks.json",
    "installation_id",
    "models_cache.json",
    "version.json",
    "AGENTS.md",
    "chrome-native-hosts.json",
    "chrome-native-hosts-v2.json",
    ".personality_migration",
    ".sandbox_migration",
)

# Top-level entries of the real HOME that must NOT be symlinked into the replay
# HOME, because the replay needs its own copy of them. Everything else is
# symlinked through: the first replay diverged from turn 1 because the isolated
# HOME was bare, so the agent lost the corporate CLI configs the original had
# read a document with and answered "this machine has no lark-cli configured"
# where the original had answered from the document. A tool that writes through
# one of these symlinks writes into the real HOME; that is accepted, because the
# three directories below are the only state whose contamination would corrupt
# the audit.
HOME_OVERLAY_EXCLUSIONS = (
    ".shared-context",
    ".shared-context-logs",
    ".codex",
    ".cursor",
)

# `~/.agents/skills/` holds the managed Agent Skill bundles, and `sctx setup`
# writes them there from bytes the binary carries (`SKILL_ASSETS` in
# crates/installer, `include_bytes!` off `skills/` at build time). Symlinking the
# real `~/.agents` into the replay HOME therefore runs a dev binary against the
# *installed* skill text, which is a silent mismatch on the one input that steers
# the agent hardest. With `--sctx-bin` the directory is excluded from the overlay
# and rebuilt: the two managed bundles are copied from the source tree the binary
# was built from, and every other skill the operator has is symlinked through, so
# the agent still sees the same machine.
MANAGED_SKILL_BUNDLES = ("shared-context", "sctx-review")
GLOBAL_SKILLS_DIRECTORY = pathlib.PurePath(".agents", "skills")

# Where a Shared Context installation root keeps the binary the hooks and the MCP
# server are invoked through. `sctx` resolves this from `$HOME` with no override,
# so a replay of a dev build has to put the dev build *here*.
SCTX_BIN_RELATIVE = pathlib.PurePath("bin", "current", "sctx")

# Any `<something>/.shared-context/bin/<version>/sctx` inside a hook command line.
# The real `hooks.json` hard-codes the installed path; rewriting it is what lets a
# replay drive a dev build without touching the operator's installation.
SCTX_COMMAND_PATH = re.compile(r"""[^\s'"]*/\.shared-context/bin/[^\s'"]*/sctx""")

# A tool call whose name contains this is the agent asking the operator something
# and blocking on the answer. Headless, nobody answers, and the turn ends with the
# question unanswered -- which is a divergence, not a result.
QUESTION_TOOL_MARKER = "request_user_input"

# Stream item types that count as one tool call. The two hosts spell them
# differently -- `codex exec --json` snake_case, the app-server lowerCamelCase --
# and the rollout on disk uses PascalCase again, so nothing here is shared.
TOOL_ITEM_TYPES_APP_SERVER = frozenset(
    {"commandExecution", "fileChange", "mcpToolCall", "webSearch", "dynamicTool", "toolCall"}
)
TOOL_ITEM_TYPES_EXEC = frozenset(
    {"command_execution", "file_change", "mcp_tool_call", "web_search", "dynamic_tool",
     "tool_call"}
)
QUESTION_SUFFIXES = ("?", "？")

# Tables in the isolated runtime database whose row counts make a compact
# before/after fingerprint of "did this replay write Shared Context state".
RUNTIME_FINGERPRINT_TABLES = (
    "external_session",
    "task_session",
    "task_injection",
    "work_episode",
    "work_observation",
    "context_usage",
    "agent_checkpoint",
    "candidate_build",
    "hook_event",
)


class ReplayError(RuntimeError):
    """A condition the operator has to resolve before a replay can be faithful."""


def log(message: str) -> None:
    print(f"[replay] {message}", flush=True)


def warn(warnings: list[str], message: str) -> None:
    warnings.append(message)
    print(f"[replay:warn] {message}", flush=True)


# --------------------------------------------------------------------------
# Resolving the original session
# --------------------------------------------------------------------------


@dataclass
class Original:
    thread_id: str
    rollout_path: pathlib.Path
    cwd: pathlib.Path
    commit: str | None
    branch: str | None
    repository_url: str | None
    cli_version: str | None
    originator: str | None
    thread_source: str | None
    model: str | None
    approval_policy: str | None
    approvals_reviewer: str | None
    sandbox_type: str | None
    sandbox_network_access: bool | None
    workspace_roots: list[str]
    prompts: list[str]


def codex_home() -> pathlib.Path:
    return pathlib.Path(os.environ.get("CODEX_HOME") or pathlib.Path.home() / ".codex")


def _open_thread_index(scratch: pathlib.Path) -> sqlite3.Connection | None:
    """Copies the Codex thread index aside and opens it.

    The live index is a WAL database another process is writing; opening it in
    place would either take a lock or read a stale snapshot. The copy includes
    the ``-wal``/``-shm`` sidecars so the copy sees committed-but-uncheckpointed
    rows -- which is where a session started minutes ago still lives.
    """
    source = codex_home() / "state_5.sqlite"
    if not source.exists():
        return None
    scratch.mkdir(parents=True, exist_ok=True)
    target = scratch / "state_5.sqlite"
    for suffix in ("", "-wal", "-shm"):
        candidate = source.with_name(source.name + suffix)
        if candidate.exists():
            shutil.copy2(candidate, scratch / candidate.name)
    return sqlite3.connect(target)


def locate_rollout(thread_id: str) -> tuple[pathlib.Path, dict[str, Any]]:
    """Returns the rollout path plus whatever the thread index knows about it."""
    index_row: dict[str, Any] = {}
    with tempfile.TemporaryDirectory(prefix="sctx-replay-index-") as scratch:
        connection = _open_thread_index(pathlib.Path(scratch))
        if connection is not None:
            with contextlib.closing(connection):
                connection.row_factory = sqlite3.Row
                try:
                    row = connection.execute(
                        "SELECT id, rollout_path, cwd, cli_version, thread_source, git_branch, "
                        "git_sha, model, first_user_message FROM threads WHERE id = ?",
                        (thread_id,),
                    ).fetchone()
                except sqlite3.DatabaseError as error:  # pragma: no cover - defensive
                    raise ReplayError(f"cannot read the Codex thread index: {error}") from error
                if row is not None:
                    index_row = dict(row)
                    candidate = pathlib.Path(index_row["rollout_path"])
                    if candidate.exists():
                        return candidate, index_row
    matches = sorted(
        pathlib.Path(path)
        for path in glob.glob(str(codex_home() / ROLLOUT_GLOB.format(thread_id=thread_id)))
    )
    if not matches:
        raise ReplayError(
            f"no rollout found for session {thread_id}: it is neither in "
            f"{codex_home() / 'state_5.sqlite'} nor on disk under {codex_home() / 'sessions'}"
        )
    return matches[-1], index_row


def iter_rollout(path: pathlib.Path) -> Iterator[dict[str, Any]]:
    with path.open(encoding="utf-8", errors="replace") as handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            try:
                yield json.loads(line)
            except json.JSONDecodeError:
                continue


def message_text(payload: dict[str, Any]) -> str:
    content = payload.get("content")
    if isinstance(content, str):
        return content
    if not isinstance(content, list):
        return ""
    return "".join(
        part.get("text", "") for part in content if isinstance(part, dict) and part.get("text")
    )


def is_human_prompt(text: str) -> bool:
    stripped = text.strip()
    if not stripped:
        return False
    return not stripped.startswith(HOST_PROMPT_PREFIXES)


def parse_original(thread_id: str, rollout: pathlib.Path, index_row: dict[str, Any]) -> Original:
    meta: dict[str, Any] = {}
    turn_context: dict[str, Any] = {}
    model: str | None = None
    prompts: list[str] = []
    for record in iter_rollout(rollout):
        kind = record.get("type")
        payload = record.get("payload")
        if not isinstance(payload, dict):
            continue
        if kind == "session_meta" and not meta:
            meta = payload
        elif kind == "turn_context" and not turn_context:
            turn_context = payload
        elif kind == "response_item" and payload.get("type") == "message":
            if payload.get("role") != "user":
                continue
            text = message_text(payload)
            if is_human_prompt(text):
                prompts.append(text)
        if model is None:
            # The model is recorded on `turn_context` in 0.153.x; older rollouts put
            # it on `token_usage_record` or an `event_msg`, so all three are tried
            # before falling back to the thread index.
            if kind == "turn_context":
                model = payload.get("model")
            elif kind in {"token_usage_record", "event_msg"}:
                candidate = payload.get("model") or payload.get("model_slug")
                if isinstance(candidate, str) and candidate:
                    model = candidate
    if not meta:
        raise ReplayError(f"{rollout} carries no session_meta record")
    if not prompts:
        raise ReplayError(f"{rollout} carries no human prompts to replay")

    git = meta.get("git") if isinstance(meta.get("git"), dict) else {}
    sandbox = turn_context.get("sandbox_policy")
    sandbox = sandbox if isinstance(sandbox, dict) else {}
    workspace_roots = turn_context.get("workspace_roots")
    cwd = meta.get("cwd") or index_row.get("cwd")
    if not cwd:
        raise ReplayError(f"{rollout} does not record a working directory")
    return Original(
        thread_id=thread_id,
        rollout_path=rollout,
        cwd=pathlib.Path(cwd),
        commit=git.get("commit_hash") or index_row.get("git_sha"),
        branch=git.get("branch") or index_row.get("git_branch"),
        repository_url=git.get("repository_url"),
        cli_version=meta.get("cli_version") or index_row.get("cli_version"),
        originator=meta.get("originator"),
        thread_source=meta.get("thread_source") or index_row.get("thread_source"),
        model=model or index_row.get("model"),
        approval_policy=turn_context.get("approval_policy"),
        approvals_reviewer=turn_context.get("approvals_reviewer"),
        sandbox_type=sandbox.get("type"),
        sandbox_network_access=sandbox.get("network_access"),
        workspace_roots=[str(root) for root in workspace_roots]
        if isinstance(workspace_roots, list)
        else [],
        prompts=prompts,
    )


# --------------------------------------------------------------------------
# Repository state
# --------------------------------------------------------------------------


def git_output(args: list[str]) -> str:
    completed = subprocess.run(
        ["git", *args], capture_output=True, text=True, check=False
    )
    if completed.returncode != 0:
        raise ReplayError(
            f"git {' '.join(args)} failed: {completed.stderr.strip() or completed.stdout.strip()}"
        )
    return completed.stdout.strip()


PROXY_VARIABLES = (
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
)

# Codex's app-server talks to the model over a long-lived stream. A proxy that serves ordinary
# HTTP perfectly well can still reset that stream, and Codex's own recovery text is the only
# signal: it reconnects, silently, until the turn's budget runs out. Matched on the raw line
# rather than on parsed JSON, because the same text reaches the replay on stderr too.
STREAM_INTERRUPTION = re.compile(r"responseStreamDisconnected|Reconnecting", re.IGNORECASE)


def count_stream_interruptions(text: str) -> int:
    return len(STREAM_INTERRUPTION.findall(text))


def apply_proxy_policy(
    environment: dict[str, str],
    proxy: str | None,
    no_proxy: bool,
    warnings: list[str],
) -> dict[str, Any]:
    """Settles the Codex child's proxy variables and returns what the manifest must record.

    The replay used to hand the child a plain copy of the caller's environment, which meant the
    proxy was whatever the operator's shell happened to export -- measured: a local proxy on
    127.0.0.1:17890 reset the app-server's websocket on every turn, all three prompts produced
    nothing, and the manifest still described three replayed turns. An environment that decides
    whether a replay can run at all belongs in the record.
    """
    record: dict[str, Any] = {
        "home": environment.get("HOME"),
        "codex_home": environment.get("CODEX_HOME"),
        "cleared": ["SCTX_LOGS_ROOT"],
    }
    if no_proxy:
        removed = sorted(name for name in PROXY_VARIABLES if name in environment)
        for name in PROXY_VARIABLES:
            environment.pop(name, None)
        environment["NO_PROXY"] = "*"
        environment["no_proxy"] = "*"
        record["proxy"] = {"source": "--no-proxy", "removed": removed, "values": {}}
        return record
    if proxy is not None:
        for name in PROXY_VARIABLES:
            environment[name] = proxy
        record["proxy"] = {"source": "--proxy", "values": {name: proxy for name in PROXY_VARIABLES}}
        return record
    inherited = {name: environment[name] for name in PROXY_VARIABLES if name in environment}
    record["proxy"] = {"source": "inherited", "values": inherited}
    if inherited:
        warn(
            warnings,
            "the Codex child inherited a proxy from the calling shell "
            f"({', '.join(f'{name}={value}' for name, value in sorted(inherited.items()))}); "
            "a proxy that resets the app-server's stream makes every turn fail with no output. "
            "Pass --proxy or --no-proxy to decide this deliberately",
        )
    record["no_proxy"] = environment.get("NO_PROXY") or environment.get("no_proxy")
    return record


def replay_branch_name(original: Original, replay_id: str) -> str:
    """`replay/<original branch>`, unique per replay when that name is taken.

    A detached worktree reports `git_branch` as `-`, which propagates into the
    replayed session's own metadata and into every hook that reads it -- so the
    replay stops being comparable to the original on a field the audit reads.
    A real local branch at the same commit fixes that at no cost.
    """
    base = (original.branch or (original.commit or "unknown")[:12]).strip()
    base = base.removeprefix("refs/heads/").strip("/") or "unknown"
    base = re.sub(r"[^A-Za-z0-9._/-]", "-", base)
    return f"replay/{base}"


def prepare_worktree(
    original: Original, worktree: pathlib.Path, replay_id: str = "", warnings: list[str] | None = None
) -> tuple[str, str | None, dict[str, Any]]:
    """Adds a worktree at the original commit on a `replay/...` branch.

    Returns `(head commit, branch or None, a record of how the branch was obtained)`.

    The detached fallback used to swallow the real failure: any non-zero exit from the ``-b`` form
    -- a branch already checked out in another worktree, a path that exists, a stale worktree
    registration, a permissions problem -- landed silently on ``--detach``, and the only trace was
    one warning saying "detached". A replayed agent then read ``git_branch: '-'`` and wrote it into
    its own knowledge as a fact about the repository. So the chain is now: try the name, and if
    that fails, say *why* and try a uniquified name before giving up on a branch at all. The
    record goes into ``manifest.checkout`` so the audit can tell "the host would not give us a
    branch" apart from "the replay never asked for one".
    """
    if not original.commit:
        raise ReplayError(
            f"session {original.thread_id} does not record a git commit; rerun with "
            "--checkout inplace if you accept replaying against the current tree"
        )
    if not (original.cwd / ".git").exists():
        raise ReplayError(f"{original.cwd} is not a git checkout")
    probe = subprocess.run(
        ["git", "-C", str(original.cwd), "cat-file", "-e", f"{original.commit}^{{commit}}"],
        capture_output=True,
        text=True,
        check=False,
    )
    if probe.returncode != 0:
        raise ReplayError(
            f"commit {original.commit} (branch {original.branch or 'unknown'}) is not present "
            f"in {original.cwd}; fetch it first, e.g. `git -C {original.cwd} fetch origin "
            f"{original.branch or original.commit}`"
        )
    worktree.parent.mkdir(parents=True, exist_ok=True)

    def branch_exists(name: str) -> bool:
        return (
            subprocess.run(
                ["git", "-C", str(original.cwd), "rev-parse", "--verify", "--quiet",
                 f"refs/heads/{name}"],
                capture_output=True,
                text=True,
                check=False,
            ).returncode
            == 0
        )

    def add(*extra: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            ["git", "-C", str(original.cwd), "worktree", "add", *extra, str(worktree),
             original.commit],
            capture_output=True,
            text=True,
            check=False,
        )

    def failure_text(completed: subprocess.CompletedProcess[str]) -> str:
        return (completed.stderr.strip() or completed.stdout.strip())[:400]

    base = replay_branch_name(original, replay_id)
    # `replay_id` is normally set; when it is not, a suffix from the clock still beats colliding.
    unique_suffix = replay_id or dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    candidates = [base] if not branch_exists(base) else []
    candidates.append(f"{base}-{unique_suffix}")

    log(f"creating worktree at {worktree} (branch {candidates[0]} at {original.commit[:12]}) -- "
        "this can take minutes on a large repository")
    started = time.monotonic()
    record: dict[str, Any] = {"branch_mode": "created", "attempts": []}
    branch: str | None = None
    for candidate in candidates:
        completed = add("-b", candidate)
        if completed.returncode == 0:
            branch = candidate
            break
        record["attempts"].append({"branch": candidate, "error": failure_text(completed)})
        if warnings is not None:
            warn(
                warnings,
                f"`git worktree add -b {candidate}` failed: {failure_text(completed)}",
            )
    if branch is None:
        # A branch is a nicety; a checkout at the right commit is the requirement. But an audit
        # that cannot see why it lost the branch is an audit that will mistake the loss for a fact
        # about the original repository.
        detached = add("--detach")
        if detached.returncode != 0:
            raise ReplayError(
                "git worktree add failed for every branch name and for --detach: "
                + failure_text(detached)
            )
        record["branch_mode"] = "detached"
    elif branch != base:
        record["branch_mode"] = "created_after_collision"
    log(f"worktree ready in {time.monotonic() - started:.1f}s")
    return git_output(["-C", str(worktree), "rev-parse", "HEAD"]), branch, record


# --------------------------------------------------------------------------
# Isolated HOME with a Shared Context snapshot
# --------------------------------------------------------------------------


def private_mkdir(path: pathlib.Path) -> pathlib.Path:
    path.mkdir(parents=True, exist_ok=True)
    path.chmod(PRIVATE_DIR_MODE)
    return path


def snapshot_sqlite(source: pathlib.Path, target: pathlib.Path) -> None:
    with contextlib.closing(sqlite3.connect(f"file:{source}?mode=ro", uri=True)) as origin:
        with contextlib.closing(sqlite3.connect(target)) as copy:
            origin.backup(copy)
    target.chmod(0o600)


def snapshot_state_tree(source: pathlib.Path, target: pathlib.Path) -> None:
    """Copies a Shared Context state directory, snapshotting databases safely.

    A plain ``cp -R`` of a live SQLite database plus its ``-wal``/``-shm`` sidecars
    produces a copy whose sidecars describe a different file; every later opener
    then either rebuilds or errors. The backup API hands over a consistent single
    file instead, so the sidecars are deliberately not copied.
    """
    private_mkdir(target)
    for entry in sorted(source.iterdir()):
        destination = target / entry.name
        if entry.is_dir() and not entry.is_symlink():
            snapshot_state_tree(entry, destination)
        elif entry.name.endswith(SQLITE_SIDECAR_SUFFIXES):
            continue
        elif entry.name.endswith(SQLITE_SUFFIXES):
            snapshot_sqlite(entry, destination)
        elif entry.is_symlink():
            os.symlink(os.readlink(entry), destination)
        else:
            shutil.copy2(entry, destination)


def overlay_real_home(
    home: pathlib.Path,
    real_home: pathlib.Path,
    exclusions: tuple[str, ...],
    warnings: list[str],
    skip_paths: tuple[pathlib.Path, ...] = (),
) -> dict[str, Any]:
    """Symlinks every top-level entry of the real HOME into the replay HOME.

    A replay in a bare HOME is not a replay of the same machine: shells, tool
    credentials, corporate CLI configs and caches all live under `~` and the
    agent notices their absence within one turn. Symlinks give the agent the same
    machine while keeping the four directories in `exclusions` -- the ones whose
    contamination would corrupt the audit -- private to the replay.
    """
    linked: list[str] = []
    skipped: list[str] = []
    for entry in sorted(real_home.iterdir()):
        if entry.name in exclusions:
            skipped.append(entry.name)
            continue
        # The audit root usually lives under HOME, and the replay HOME lives
        # inside the audit root: symlinking it back would make every tree walk
        # from the replay HOME infinite.
        if any(entry == skip for skip in skip_paths):
            skipped.append(entry.name)
            continue
        target = home / entry.name
        if target.exists() or target.is_symlink():
            continue
        try:
            os.symlink(entry, target)
        except OSError as error:  # pragma: no cover - defensive
            warn(warnings, f"could not overlay {entry.name} into the replay HOME: {error}")
            continue
        linked.append(entry.name)
    log(f"HOME overlay: symlinked {len(linked)} entries, kept {len(skipped)} private/skipped")
    return {"symlinked": linked, "excluded": list(exclusions), "skipped": skipped}


def setup_replay_logs(
    logs_root: pathlib.Path, binary: pathlib.Path, warnings: list[str]
) -> dict[str, Any]:
    """Configures the replay's own log-service root so hook diagnostics get written.

    An empty `.shared-context-logs/` silences the replay side of the audit: hook
    decisions are not in `runtime.sqlite` any more, they are aggregated by the
    log collector into `state/hook-diagnostics.json`, and with no config there is
    no collector and no file. `sctx logs init` without `--remote` produces a
    configuration that collects locally and has nowhere to upload to; running it
    with HOME unset skips the launchd reconciliation, so the operator's real
    collector and sync services are left exactly as they were.
    """
    private_mkdir(logs_root)
    environment = {key: value for key, value in os.environ.items() if key != "HOME"}
    completed = subprocess.run(
        [str(binary), "logs", "init", "--logs-root", str(logs_root)],
        capture_output=True,
        text=True,
        check=False,
        env=environment,
    )
    if completed.returncode != 0:
        warn(
            warnings,
            "`sctx logs init` failed for the replay logs root "
            f"({completed.stderr.strip() or completed.stdout.strip()}); replay-side hook "
            "diagnostics will be missing",
        )
        return {"configured": False, "path": str(logs_root)}
    config = logs_root / "config.toml"
    body = config.read_text(encoding="utf-8")
    if "remote" in body:
        warn(warnings, "the replay logs config names a remote; uploads are NOT disabled")
    # `on_maintain` is the one path that would push replay telemetry outward on
    # its own; there is no remote to push to, but belt and braces.
    body = body.replace("on_maintain = true", "on_maintain = false")
    config.write_text(body, encoding="utf-8")
    config.chmod(0o600)
    return {
        "configured": True,
        "path": str(logs_root),
        "remote_configured": "remote" in body,
        "sync_on_maintain": False,
    }


def start_log_collector(
    binary: pathlib.Path, logs_root: pathlib.Path, log_path: pathlib.Path
) -> subprocess.Popen[bytes] | None:
    """Runs the log collector for the lifetime of the replay, in-process, no launchd."""
    try:
        return subprocess.Popen(
            [str(binary), "logs", "collect", "--logs-root", str(logs_root)],
            stdin=subprocess.DEVNULL,
            stdout=log_path.open("wb"),
            stderr=subprocess.STDOUT,
            env={key: value for key, value in os.environ.items() if key != "HOME"},
        )
    except OSError:  # pragma: no cover - defensive
        return None


def file_digest(path: pathlib.Path) -> str:
    """sha256 of one file, so the manifest can name exact bytes rather than a path."""
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def place_sctx_binary(sctx_root: pathlib.Path, sctx_bin: pathlib.Path) -> pathlib.Path:
    """Copies a dev build into the snapshot's own `bin/current/sctx`.

    A copy, not a symlink: `sctx` resolves its installation root from `$HOME`, and
    a symlink back into `~/.shared-context/bin` would make the replay's binary the
    operator's again the moment the real install is upgraded mid-run. `embedding/`
    stays a symlink (2.3G of model weights nothing writes to).
    """
    target = sctx_root / SCTX_BIN_RELATIVE
    private_mkdir(target.parent.parent)
    private_mkdir(target.parent)
    shutil.copy2(sctx_bin, target)
    target.chmod(0o700)
    return target


def sctx_bin_facts(
    binary: pathlib.Path, source: pathlib.Path, home: pathlib.Path
) -> dict[str, Any]:
    """What the manifest records about the binary this replay actually ran."""
    version = subprocess.run(
        [str(binary), "--version"],
        capture_output=True,
        text=True,
        check=False,
        env=dict(os.environ, HOME=str(home)),
    )
    return {
        "source_path": str(source),
        "resolved_source_path": str(source.resolve()),
        "installed_path": str(binary),
        "sha256": file_digest(binary),
        "bytes": binary.stat().st_size,
        "version": version.stdout.strip() or version.stderr.strip(),
    }


def resolve_skill_source(sctx_bin: pathlib.Path, warnings: list[str]) -> pathlib.Path | None:
    """Finds the `skills/` tree of the checkout a dev binary was built from.

    The installer embeds the bundle with `include_bytes!("../../../skills/...")`,
    so "the skills that match this binary" is literally `<repo>/skills` of the
    tree it was compiled in -- and `target/{debug,release}/sctx` puts that two
    directories up. The walk is by marker file rather than by fixed depth so a
    `--target-dir` build still resolves.
    """
    for parent in sctx_bin.resolve().parents:
        candidate = parent / "skills"
        if all(
            (candidate / bundle / "SKILL.md").is_file() for bundle in MANAGED_SKILL_BUNDLES
        ):
            return candidate
    warn(
        warnings,
        f"no skills/ tree with {', '.join(MANAGED_SKILL_BUNDLES)} above {sctx_bin}; "
        "the replay HOME keeps the operator's INSTALLED skill bundles, which may not "
        "match this binary",
    )
    return None


def install_skill_bundle(
    home: pathlib.Path, real_home: pathlib.Path, source: pathlib.Path, warnings: list[str]
) -> dict[str, Any]:
    """Rebuilds `<home>/.agents` so the managed bundles match the binary under replay.

    Everything the operator has under `~/.agents` is symlinked through -- the
    replay is supposed to be the same machine -- except the two Shared Context
    bundles, which are copied from `source` (the checkout the binary was built
    from). Per-file digests go into the manifest, because "which skill bytes were
    in effect" is not answerable from a version string.
    """
    real_agents = real_home / ".agents"
    agents = private_mkdir(home / ".agents")
    if real_agents.is_dir():
        for entry in sorted(real_agents.iterdir()):
            if entry.name == "skills":
                continue
            with contextlib.suppress(OSError):
                os.symlink(entry, agents / entry.name)
    skills = private_mkdir(home / GLOBAL_SKILLS_DIRECTORY)
    linked: list[str] = []
    real_skills = real_agents / "skills"
    if real_skills.is_dir():
        for entry in sorted(real_skills.iterdir()):
            if entry.name in MANAGED_SKILL_BUNDLES:
                continue
            with contextlib.suppress(OSError):
                os.symlink(entry, skills / entry.name)
                linked.append(entry.name)
    files: dict[str, dict[str, Any]] = {}
    for bundle in MANAGED_SKILL_BUNDLES:
        origin = source / bundle
        if not origin.is_dir():
            warn(warnings, f"{origin} is missing; that skill bundle is absent from the replay")
            continue
        shutil.copytree(origin, skills / bundle, dirs_exist_ok=True)
        for path in sorted(p for p in (skills / bundle).rglob("*") if p.is_file()):
            relative = str(path.relative_to(skills))
            files[relative] = {"bytes": path.stat().st_size, "sha256": file_digest(path)}
    bundle_digest = hashlib.sha256(
        json.dumps(files, sort_keys=True, separators=(",", ":")).encode("utf-8")
    ).hexdigest()
    log(f"skill bundles: copied {len(files)} files from {source}, symlinked {len(linked)} others")
    return {
        "source": str(source),
        "installed_at": str(skills),
        "bundles": list(MANAGED_SKILL_BUNDLES),
        "files": files,
        "bundle_sha256": bundle_digest,
        "symlinked_other_skills": linked,
    }


def build_replay_home(
    home: pathlib.Path,
    real_home: pathlib.Path,
    warnings: list[str],
    overlay_exclusions: tuple[str, ...] = HOME_OVERLAY_EXCLUSIONS,
    skip_paths: tuple[pathlib.Path, ...] = (),
    sctx_bin: pathlib.Path | None = None,
) -> dict[str, Any]:
    """Builds the replay HOME. With ``sctx_bin`` it is a *dev build's* HOME.

    Without it nothing changes: `bin/` is a symlink back to the operator's
    installation and `.agents` comes through the overlay, so the replay exercises
    the installed `sctx` and the installed skill text. With it, `bin/current/sctx`
    is a copy of the given binary and `.agents/skills/{shared-context,sctx-review}`
    are copied from the checkout that binary was built from -- the two halves of
    "this installation", kept in step with each other.
    """
    private_mkdir(home)
    real_sctx = real_home / ".shared-context"
    if not real_sctx.is_dir():
        raise ReplayError(f"{real_sctx} does not exist; nothing to snapshot")
    sctx = private_mkdir(home / ".shared-context")

    started = time.monotonic()
    for name in ("state", "repository"):
        source = real_sctx / name
        if not source.is_dir():
            warn(warnings, f"{source} is missing; the replay runs without it")
            continue
        log(f"snapshotting {source} -> {sctx / name}")
        snapshot_state_tree(source, sctx / name)
    binary_record: dict[str, Any] | None = None
    for name in ("embedding", "bin"):
        source = real_sctx / name
        if name == "bin" and sctx_bin is not None:
            placed = place_sctx_binary(sctx, sctx_bin)
            binary_record = sctx_bin_facts(placed, sctx_bin, home)
            log(f"sctx binary: copied {sctx_bin} -> {placed} ({binary_record['version']})")
            continue
        if source.exists():
            os.symlink(source, sctx / name)
        else:
            warn(warnings, f"{source} is missing; the replay runs without it")

    config = real_sctx / "config.toml"
    if not config.is_file():
        raise ReplayError(f"{config} does not exist; run `sctx setup` first")
    # Every absolute path inside config.toml points at the real installation root.
    # Rewriting the prefix keeps the repository store and the embedding model
    # pointing at this HOME -- the store is a snapshot, the embedding directory a
    # symlink back to the 2.3G original.
    body = config.read_text(encoding="utf-8").replace(str(real_sctx), str(sctx))
    (sctx / "config.toml").write_text(body, encoding="utf-8")
    (sctx / "config.toml").chmod(0o600)

    # `policy.md` is the installation's own text for the `## session` / `## checkpoint`
    # / `## triage` sections the tool surface carries. It is optional: an install
    # without one gets the binary's built-in default, which is what the replay then
    # gets too. Copying it when present keeps the replay on the operator's wording.
    policy = real_sctx / "policy.md"
    copied = ["state", "repository", "config.toml"]
    if policy.is_file():
        shutil.copy2(policy, sctx / "policy.md")
        (sctx / "policy.md").chmod(0o600)
        copied.append("policy.md")

    # Telemetry resolves its root from HOME. The directory is the replay's own, so
    # nothing it records reaches the operator's spool -- but it is configured
    # rather than left empty, because an empty one records nothing at all.
    # The log service is part of what a replay of a dev build has to exercise: hook
    # decisions reach `hook-diagnostics.json` through it, and that file is the
    # replay side's only record that a hook ran at all.
    logs = setup_replay_logs(
        home / ".shared-context-logs",
        (sctx / SCTX_BIN_RELATIVE) if sctx_bin is not None else (real_sctx / SCTX_BIN_RELATIVE),
        warnings,
    )

    if sctx_bin is not None:
        overlay_exclusions = tuple(dict.fromkeys(overlay_exclusions + (".agents",)))
    overlay = overlay_real_home(home, real_home, overlay_exclusions, warnings, skip_paths)
    skills: dict[str, Any] | None = None
    if sctx_bin is not None:
        source = resolve_skill_source(sctx_bin, warnings)
        if source is None:
            # Better the operator's installed bundles than none at all; the warning
            # above already says the skill text may not match the binary.
            with contextlib.suppress(OSError):
                os.symlink(real_home / ".agents", home / ".agents")
        else:
            skills = install_skill_bundle(home, real_home, source, warnings)
    if not (home / ".agents").exists():
        warn(warnings, f"{real_home / '.agents'} is missing; skill bundles will not load")
    if not (home / ".gitconfig").exists():
        warn(warnings, f"{real_home / '.gitconfig'} is missing; git uses its defaults")
    log(f"HOME snapshot ready in {time.monotonic() - started:.1f}s")
    return {
        "path": str(home),
        "snapshot_taken_at": dt.datetime.now(dt.timezone.utc).isoformat(),
        "copied": copied + (["bin/current/sctx"] if sctx_bin is not None else []),
        "symlinked": ["embedding"] if sctx_bin is not None else ["embedding", "bin"],
        "overlay": overlay,
        "logs": logs,
        "sctx_bin": binary_record,
        "skill_bundle": skills,
    }


def read_repository_catalog(config: pathlib.Path) -> list[tuple[str, list[str]]]:
    """Parses `[[repositories]]` out of a Shared Context config.toml.

    ``tomllib`` is used where available; the regex fallback keeps the script
    runnable on Python 3.10, which the repo's other helper scripts still target.
    """
    body = config.read_text(encoding="utf-8")
    try:
        import tomllib

        document = tomllib.loads(body)
        return [
            (str(entry.get("id")), [str(path) for path in entry.get("paths", [])])
            for entry in document.get("repositories", [])
            if entry.get("id")
        ]
    except Exception:  # pragma: no cover - fallback for older interpreters
        catalog: list[tuple[str, list[str]]] = []
        for block in body.split("[[repositories]]")[1:]:
            block = block.split("\n[", 1)[0]
            identity = re.search(r'id\s*=\s*"([^"]+)"', block)
            paths = re.findall(r'"([^"]+)"', block.split("paths", 1)[-1])
            if identity:
                catalog.append((identity.group(1), paths))
        return catalog


def register_checkout(
    home: pathlib.Path,
    binary: pathlib.Path,
    original_cwd: pathlib.Path,
    checkout: pathlib.Path,
    warnings: list[str],
) -> dict[str, Any]:
    """Teaches the snapshotted catalog that the replay checkout is the same Repository.

    Shared Context authorizes a session by longest-prefix match of its startup
    directory against the registered checkout paths. A worktree lives somewhere
    else entirely, so without this the replay of a session that *did* activate
    would silently run with Shared Context disabled -- the exact signal the audit
    is trying to measure. Registering the worktree under the original's Repository
    identity keeps the knowledge base the same and only adds a second checkout.
    """
    config = home / ".shared-context" / "config.toml"
    catalog = read_repository_catalog(config)
    matched: str | None = None
    matched_path = ""
    for identity, paths in catalog:
        for path in paths:
            if str(original_cwd) == path or str(original_cwd).startswith(path.rstrip("/") + "/"):
                if len(path) > len(matched_path):
                    matched, matched_path = identity, path
    if matched is None:
        warn(
            warnings,
            f"{original_cwd} is outside every registered Shared Context Repository, so the "
            "replay runs with Shared Context disabled -- exactly as the original session did",
        )
        return {"repository_id": None, "registered_path": None}
    if checkout == original_cwd:
        return {"repository_id": matched, "registered_path": str(original_cwd)}
    log(f"registering {checkout} as Repository {matched} in the snapshot (first scan may be slow)")
    environment = dict(os.environ, HOME=str(home))
    completed = subprocess.run(
        [str(binary), "repository", "add", "--repository-id", matched, "--path", str(checkout)],
        capture_output=True,
        text=True,
        check=False,
        env=environment,
    )
    if completed.returncode != 0:
        warn(
            warnings,
            f"`sctx repository add` failed for the replay checkout ({completed.stderr.strip()}); "
            "Shared Context will stay disabled for the replay",
        )
        return {"repository_id": None, "registered_path": None}
    return {"repository_id": matched, "registered_path": str(checkout)}


# --------------------------------------------------------------------------
# Isolated CODEX_HOME
# --------------------------------------------------------------------------


def drop_hook_state_tables(body: str, key_prefix: str) -> tuple[str, int]:
    """Removes every ``[hooks.state."<key_prefix>..."]`` table from a config.toml body.

    Used when the replay writes its own ``hooks.json``: the operator's stored
    hashes describe the operator's hook entries, and leaving them behind under a
    rewritten path would make Codex read them as ``Modified`` and refuse to run
    the hook -- the failure this whole mechanism exists to avoid.
    """
    lines = body.splitlines(keepends=True)
    kept: list[str] = []
    dropped = 0
    skipping = False
    for line in lines:
        stripped = line.lstrip()
        if stripped.startswith("["):
            skipping = stripped.startswith(f'[hooks.state."{key_prefix}')
            if skipping:
                dropped += 1
                continue
        if skipping:
            continue
        kept.append(line)
    return "".join(kept), dropped


def retarget_sctx_hooks(
    hooks_file: dict[str, Any], binary: pathlib.Path
) -> tuple[dict[str, Any], int]:
    """Points every Shared Context hook command in a hooks.json at ``binary``.

    Everything else about the entry -- the event it is registered for, the
    ``--agent codex --agent-version '<x>'`` arguments, the quoting, the status
    message -- is left exactly as the operator's installer wrote it, because the
    argument string is part of what is being replayed.
    """
    replaced = 0
    for ref in codex_trust.iter_hook_handlers(hooks_file):
        command = ref.handler.get("command")
        if not isinstance(command, str) or not SCTX_COMMAND_PATH.search(command):
            continue
        ref.handler["command"] = SCTX_COMMAND_PATH.sub(str(binary), command)
        replaced += 1
    return hooks_file, replaced


def retarget_mcp_command(body: str, server: str, binary: pathlib.Path) -> tuple[str, bool]:
    """Rewrites ``[mcp_servers.<server>] command = ...`` in a config.toml body.

    The app-server driver passes no ``-c`` overrides, so for that driver the
    copied ``config.toml`` is the only place the MCP server's command is stated.
    """
    lines = body.splitlines(keepends=True)
    inside = False
    changed = False
    for index, line in enumerate(lines):
        stripped = line.strip()
        if stripped.startswith("["):
            inside = stripped == f"[mcp_servers.{server}]"
            continue
        if inside and stripped.startswith("command"):
            lines[index] = f"command = {toml_string(str(binary))}\n"
            changed = True
            inside = False
    return "".join(lines), changed


def build_codex_home(
    target: pathlib.Path,
    real_codex_home: pathlib.Path,
    checkout: pathlib.Path,
    warnings: list[str],
    sctx_bin: pathlib.Path | None = None,
) -> dict[str, Any]:
    """Mirrors the real Codex home closely enough that hooks and plugins still load.

    Codex 0.153.4 refuses to run a hook unless config.toml holds a
    ``[hooks.state."<hooks.json path>:<event>:<group>:<handler>"]`` entry whose
    ``trusted_hash`` matches the hash it recomputes from the hook entry. The key
    is *keyed by the absolute path of the hooks file*, so a verbatim copy of both
    files into a new CODEX_HOME trusts nothing.

    Two routes out of that, and which one is taken depends on whether the replay
    is allowed to change the hook entries:

    - **No ``sctx_bin``** (unchanged behaviour): the hook entries are copied byte
      for byte, so their hashes are unchanged and only the *path prefix* inside
      the state keys has to be rewritten.
    - **With ``sctx_bin``**: the hook commands have to name the dev binary, which
      changes their hashes, so the operator's state entries are dropped and fresh
      ones are computed with `codex_trust`. Before trusting that computation the
      operator's own entries are recomputed and compared against what Codex
      stored -- if that disagrees, this build of Codex hashes differently and the
      replay says so instead of silently running with hooks disabled.

    Neither route needs ``--dangerously-bypass-hook-trust``.
    """
    private_mkdir(target)
    config = real_codex_home / "config.toml"
    if not config.is_file():
        raise ReplayError(f"{config} does not exist")
    real_hooks = real_codex_home / "hooks.json"
    target_hooks = target / "hooks.json"
    body = config.read_text(encoding="utf-8")
    record: dict[str, Any] = {"hooks_json": str(target_hooks), "sctx_bin_retargeted": False}

    if sctx_bin is None:
        hooks_key_prefix = f'[hooks.state."{real_hooks}:'
        rewritten_keys = body.count(hooks_key_prefix)
        body = body.replace(hooks_key_prefix, f'[hooks.state."{target_hooks}:')
        if rewritten_keys == 0:
            warn(
                warnings,
                f"no [hooks.state] entries in {config} name {real_hooks}; "
                "Codex may refuse to run the Shared Context hooks in the isolated CODEX_HOME",
            )
        record["trust"] = {"mode": "path-prefix-rewrite", "state_keys_rewritten": rewritten_keys}
    else:
        if not real_hooks.is_file():
            raise ReplayError(f"{real_hooks} does not exist; nothing to retarget")
        hooks_file = codex_trust.load_hooks_file(real_hooks)

        # Self-check first: recompute the operator's *unmodified* entries and
        # compare them with what Codex itself stored. Agreement is the evidence
        # that the hashes written below will be accepted.
        stored = codex_trust.read_config_hook_states(config)
        reference = codex_trust.hook_state_entries(hooks_file, str(real_hooks))
        agreed = sum(1 for key, value in reference.items() if stored.get(key) == value)
        if agreed != len(reference):
            warn(
                warnings,
                f"the replay's hook-hash implementation reproduces only {agreed} of "
                f"{len(reference)} trusted_hash values Codex stored for {real_hooks}; "
                "the hooks in the isolated CODEX_HOME may be refused as untrusted "
                f"(codex_trust follows openai/codex {codex_trust.CODEX_SOURCE_COMMIT})",
            )

        hooks_file, retargeted = retarget_sctx_hooks(hooks_file, sctx_bin)
        if retargeted == 0:
            warn(
                warnings,
                f"no hook command in {real_hooks} names a Shared Context binary; "
                "the replay's hooks still point wherever the operator's did",
            )
        target_hooks.write_text(
            json.dumps(hooks_file, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
        )
        target_hooks.chmod(0o600)

        body, dropped = drop_hook_state_tables(body, f"{real_hooks}:")
        entries = codex_trust.hook_state_entries(hooks_file, str(target_hooks))
        body = body.rstrip("\n") + "\n\n" + codex_trust.render_hook_state(entries)
        record["sctx_bin_retargeted"] = True
        record["trust"] = {
            "mode": "recomputed",
            "codex_source_commit": codex_trust.CODEX_SOURCE_COMMIT,
            "operator_entries_reproduced": f"{agreed}/{len(reference)}",
            "state_keys_dropped": dropped,
            "state_keys_written": len(entries),
            "trusted_hashes": entries,
            "hook_commands_retargeted": retargeted,
        }

    body, mcp_changed = retarget_mcp_command(body, "shared-context", sctx_bin) if sctx_bin else (
        body,
        False,
    )
    record["mcp_command_retargeted"] = mcp_changed
    if sctx_bin is not None and not mcp_changed:
        warn(
            warnings,
            f"no [mcp_servers.shared-context] command in {config} to retarget; the "
            "app-server driver passes no -c overrides, so the MCP server may start the "
            "operator's installed binary",
        )

    # A checkout Codex has never seen is untrusted, which in a headless run means
    # the sandbox tightens without saying so. The original cwd carries this mark
    # already; the replay checkout inherits it.
    body += f'\n[projects."{checkout}"]\ntrust_level = "trusted"\n'
    (target / "config.toml").write_text(body, encoding="utf-8")
    (target / "config.toml").chmod(0o600)

    for name in CODEX_HOME_COPIES:
        if name == "hooks.json" and sctx_bin is not None:
            continue  # written above, retargeted
        source = real_codex_home / name
        if source.is_file():
            shutil.copy2(source, target / name)
    if not (target / "auth.json").exists():
        warn(warnings, f"{real_codex_home / 'auth.json'} is missing; Codex may not be logged in")
    for name in CODEX_HOME_SYMLINKS:
        source = real_codex_home / name
        if source.exists() and not (target / name).exists():
            os.symlink(source, target / name)
    return record


# --------------------------------------------------------------------------
# Driving the turns
# --------------------------------------------------------------------------


def looks_like_question(text: str) -> bool:
    """Whether a final assistant message is a question waiting for an answer."""
    stripped = (text or "").strip().rstrip("*_`)]\"'\u201d\u300d\u300f")
    return bool(stripped) and stripped.endswith(QUESTION_SUFFIXES)


def question_from_item(item: dict[str, Any]) -> str | None:
    """Extracts the operator-facing question out of one completed stream item.

    Two shapes carry one: a `request_user_input`-family tool call (the agent
    blocking on an answer), and a final assistant message that simply ends in a
    question mark. Both leave the replay stalled against an original in which a
    human answered, so both have to be visible.
    """
    kind = str(item.get("type") or "")
    name = str(item.get("name") or item.get("tool_name") or item.get("toolName") or "")
    if QUESTION_TOOL_MARKER in name.lower().replace("-", "_") or QUESTION_TOOL_MARKER in kind.lower(
    ).replace("-", "_"):
        payload = item.get("arguments") or item.get("input") or item.get("questions") or item
        return json.dumps(payload, ensure_ascii=False)[:2000]
    if kind in {"agent_message", "agentMessage"}:
        questions = item.get("questions")
        if questions:
            return json.dumps(questions, ensure_ascii=False)[:2000]
        text = item.get("text") or ""
        if item.get("phase") in (None, "final_answer") and looks_like_question(text):
            return text.strip()[:2000]
    return None


def read_from(path: pathlib.Path, offset: int) -> str:
    """Whatever was appended to `path` after `offset`, or `""` if it cannot be read."""
    try:
        with path.open("r", encoding="utf-8", errors="replace") as handle:
            handle.seek(offset)
            return handle.read()
    except OSError:
        return ""


@dataclass
class TurnOutcome:
    """What one turn produced, as the driver saw it."""

    usage: dict[str, Any] | None
    question: str | None
    tool_calls: int
    errors: list[str]
    turn_id: str | None = None
    stream_interruptions: int = 0
    aborted: bool = False


@dataclass
class TurnResult:
    prompt_index: int
    prompt_chars: int
    started: str
    finished: str
    exit_code: int
    # The *thread's* id, which is the same for every turn of a replay by design -- the app-server
    # keeps one thread across turns. `turn_id` is the per-turn identity.
    thread_id: str | None
    usage: dict[str, Any] | None
    waited_for_turn_stop: bool
    stream_path: str
    stderr_path: str
    errors: list[str] = field(default_factory=list)
    question: str | None = None
    answered_with_next_prompt: bool = False
    tool_calls: int = 0
    turn_id: str | None = None
    stream_interruptions: int = 0
    # A turn that exited non-zero or reported an error produced nothing worth auditing. The
    # manifest used to describe such a turn exactly like a good one.
    failed: bool = False


def toml_string(value: str) -> str:
    return json.dumps(value)


def sandbox_flag(original: Original) -> str:
    mapping = {
        "workspace-write": "workspace-write",
        "read-only": "read-only",
        "danger-full-access": "danger-full-access",
    }
    return mapping.get(original.sandbox_type or "", "workspace-write")


def shared_config_overrides(
    original: Original, home: pathlib.Path, binary: pathlib.Path
) -> list[str]:
    overrides = [
        f"mcp_servers.shared-context.command={toml_string(str(binary))}",
        'mcp_servers.shared-context.args=["mcp","serve","--client","codex"]',
        f"mcp_servers.shared-context.env={{HOME={toml_string(str(home))}}}",
        # `--approve-for-me` is the headless equivalent of the reviewer the original
        # interactive session used; stating it explicitly makes `resume`, which has
        # no such flag, behave like turn 1.
        'approvals_reviewer="auto_review"',
    ]
    if original.approval_policy:
        overrides.append(f"approval_policy={toml_string(original.approval_policy)}")
    if original.sandbox_network_access is not None:
        overrides.append(
            "sandbox_workspace_write.network_access="
            f"{'true' if original.sandbox_network_access else 'false'}"
        )
    return overrides


def build_turn_command(
    codex: str,
    original: Original,
    prompt: str,
    checkout: pathlib.Path,
    home: pathlib.Path,
    binary: pathlib.Path,
    resume_thread: str | None,
) -> list[str]:
    command = [codex, "exec"]
    if resume_thread:
        command += ["resume", resume_thread]
    command += ["--json"]
    if not resume_thread:
        # `codex exec resume` accepts neither --color nor -C; NO_COLOR in the
        # environment covers the first, and the process working directory the second.
        command += ["--color", "never", "-C", str(checkout)]
        # `--approve-for-me` is the only headless approval route `codex exec` offers,
        # and it hard-codes the workspace-write sandbox: passing `--sandbox` beside it
        # is a usage error. When the original ran under a different sandbox the flag
        # is dropped and `approvals_reviewer=auto_review` (below) carries approvals,
        # because mirroring the sandbox matters more than the flag's convenience.
        if sandbox_flag(original) == "workspace-write":
            command += ["--approve-for-me"]
        else:
            command += ["-s", sandbox_flag(original)]
    if original.model:
        command += ["-m", original.model]
    if original.thread_source and not resume_thread:
        command += ["--thread-source", original.thread_source]
    for override in shared_config_overrides(original, home, binary):
        command += ["-c", override]
    if resume_thread:
        command += ["-c", f"sandbox_mode={toml_string(sandbox_flag(original))}"]
    command.append(prompt)
    return command


def run_turn(
    command: list[str],
    environment: dict[str, str],
    checkout: pathlib.Path,
    stream_path: pathlib.Path,
    stderr_path: pathlib.Path,
    timeout: int,
) -> tuple[int, str | None, dict[str, Any] | None, list[str], str | None, int]:
    """Runs one `codex exec` turn, teeing the JSONL stream to disk as it arrives."""
    thread_id: str | None = None
    usage: dict[str, Any] | None = None
    errors: list[str] = []
    question: str | None = None
    tool_calls = 0
    deadline = time.monotonic() + timeout
    with stderr_path.open("w", encoding="utf-8") as stderr_handle:
        process = subprocess.Popen(
            command,
            cwd=str(checkout),
            env=environment,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=stderr_handle,
            text=True,
            bufsize=1,
        )
        assert process.stdout is not None
        with stream_path.open("w", encoding="utf-8") as stream_handle:
            for line in process.stdout:
                stream_handle.write(line)
                stream_handle.flush()
                try:
                    event = json.loads(line)
                except json.JSONDecodeError:
                    continue
                kind = event.get("type")
                if kind == "thread.started" and event.get("thread_id"):
                    thread_id = event["thread_id"]
                    log(f"thread {thread_id}")
                elif kind == "item.completed":
                    item = event.get("item") or {}
                    if item.get("type") in TOOL_ITEM_TYPES_EXEC:
                        tool_calls += 1
                    question = question or question_from_item(item)
                elif kind == "turn.completed":
                    usage = event.get("usage")
                elif kind == "turn.failed":
                    errors.append(json.dumps(event.get("error", event))[:400])
                elif kind == "error":
                    errors.append(json.dumps(event)[:400])
                if time.monotonic() > deadline:
                    process.kill()
                    errors.append(f"turn exceeded {timeout}s and was killed")
                    break
        try:
            status = process.wait(timeout=max(1.0, deadline - time.monotonic()))
        except subprocess.TimeoutExpired:
            process.kill()
            status = 124
            errors.append(f"turn exceeded {timeout}s and was killed")
    return status, thread_id, usage, errors, question, tool_calls


class AppServerDriver:
    """Drives a whole replay through one `codex app-server` process.

    `codex exec resume` starts a *new* process per turn, and every start replays
    the SessionStart hook: a two-turn replay carried the Shared Context activation
    marker twice where the interactive original carried it once, which is exactly
    the kind of injected-context difference this audit exists to measure. The
    app-server protocol keeps one process and one thread across turns
    (`thread/start` once, `turn/start` per prompt), so the hook sequence matches an
    interactive session: session_start once, then prompt_submit / post_tool_use /
    stop per turn.
    """

    def __init__(
        self,
        codex: str,
        environment: dict[str, str],
        cwd: pathlib.Path,
        transcript: pathlib.Path,
        warnings: list[str],
    ) -> None:
        self.codex = codex
        self.environment = environment
        self.cwd = cwd
        self.warnings = warnings
        self.transcript = transcript.open("w", encoding="utf-8")
        self.process: subprocess.Popen[str] | None = None
        self.messages: list[dict[str, Any]] = []
        self.responses: dict[Any, dict[str, Any]] = {}
        self._lock = threading.Lock()
        self._next_id = 0
        self._sink: list[dict[str, Any]] | None = None
        self.pending_answer: str | None = None
        self.questions: list[str] = []
        self.stderr_path = transcript.with_suffix(".stderr")
        # Stream disconnects and reconnects seen since the current turn started. Counted from the
        # raw line, because the recovery text is not always well-formed JSON and reaches the
        # replay on stderr as well.
        self.stream_interruptions = 0

    # -- plumbing ---------------------------------------------------------
    def _send(self, payload: dict[str, Any]) -> None:
        assert self.process is not None and self.process.stdin is not None
        line = json.dumps(payload, ensure_ascii=False)
        self.process.stdin.write(line + "\n")
        self.process.stdin.flush()

    def _request(self, method: str, params: dict[str, Any]) -> int:
        with self._lock:
            self._next_id += 1
            request_id = self._next_id
        self._send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
        return request_id

    def _await(self, request_id: int, timeout: float) -> dict[str, Any] | None:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            with self._lock:
                if request_id in self.responses:
                    return self.responses.pop(request_id)
            if self.process is not None and self.process.poll() is not None:
                return None
            time.sleep(0.05)
        return None

    def _record(self, message: dict[str, Any]) -> None:
        self.transcript.write(json.dumps(message, ensure_ascii=False) + "\n")
        self.transcript.flush()
        with self._lock:
            self.messages.append(message)
            if self._sink is not None:
                self._sink.append(message)

    def _answer_user_input(self, message: dict[str, Any]) -> None:
        """Replies to a blocking `item/tool/requestUserInput` server request.

        With `--on-question next-prompt` the answer is the next original prompt,
        which is what the human typed next anyway. Otherwise every question is
        answered with an empty string so the turn can finish and be recorded,
        rather than deadlocking the replay against a question nobody will answer.
        """
        params = message.get("params") or {}
        questions = params.get("questions") or []
        for question in questions:
            self.questions.append(json.dumps(question, ensure_ascii=False)[:2000])
        answer = self.pending_answer or ""
        self._send(
            {
                "jsonrpc": "2.0",
                "id": message["id"],
                "result": {
                    "answers": {
                        question.get("id", str(index)): {"answers": [answer]}
                        for index, question in enumerate(questions)
                    }
                },
            }
        )

    def _reader(self) -> None:
        assert self.process is not None and self.process.stdout is not None
        for line in self.process.stdout:
            line = line.strip()
            if not line:
                continue
            interruptions = count_stream_interruptions(line)
            if interruptions:
                with self._lock:
                    self.stream_interruptions += interruptions
            try:
                message = json.loads(line)
            except json.JSONDecodeError:
                continue
            self._record(message)
            if "id" in message and "method" in message:
                if str(message.get("method", "")).endswith("requestUserInput"):
                    self._answer_user_input(message)
                else:
                    # Approvals are routed to `auto_review`, so anything that still
                    # reaches the client is something a headless run cannot judge;
                    # an empty result is the least-surprising refusal.
                    self._send({"jsonrpc": "2.0", "id": message["id"], "result": {}})
            elif "id" in message:
                with self._lock:
                    self.responses[message["id"]] = message

    # -- lifecycle --------------------------------------------------------
    def start(self) -> None:
        self.process = subprocess.Popen(
            [self.codex, "app-server"],
            cwd=str(self.cwd),
            env=self.environment,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=self.stderr_path.open("w", encoding="utf-8"),
            text=True,
            bufsize=1,
        )
        threading.Thread(target=self._reader, daemon=True).start()
        request_id = self._request(
            "initialize",
            {"clientInfo": {"name": "sctx-session-replay", "version": "0.1.0",
                            "title": "Shared Context session replay"}},
        )
        if self._await(request_id, 60) is None:
            raise ReplayError("codex app-server did not answer `initialize`")
        self._send({"jsonrpc": "2.0", "method": "initialized", "params": {}})

    def start_thread(self, original: Original, timeout: float = 180) -> str:
        params: dict[str, Any] = {
            "cwd": str(self.cwd),
            "sandbox": sandbox_flag(original),
            "approvalsReviewer": "auto_review",
            "ephemeral": False,
        }
        if original.approval_policy:
            params["approvalPolicy"] = original.approval_policy
        if original.model:
            params["model"] = original.model
        if original.thread_source:
            params["threadSource"] = original.thread_source
        response = self._await(self._request("thread/start", params), timeout)
        if response is None or "result" not in response:
            raise ReplayError(f"codex app-server refused `thread/start`: {response}")
        thread = response["result"].get("thread") or {}
        thread_id = thread.get("id") or response["result"].get("threadId")
        if not thread_id:
            raise ReplayError(f"codex app-server returned no thread id: {response}")
        return str(thread_id)

    def run_turn(
        self,
        thread_id: str,
        prompt: str,
        stream_path: pathlib.Path,
        timeout: int,
        max_interruptions: int = 0,
    ) -> TurnOutcome:
        with self._lock:
            self._sink = []
            self.stream_interruptions = 0
        stderr_offset = self.stderr_path.stat().st_size if self.stderr_path.exists() else 0
        errors: list[str] = []
        request_id = self._request(
            "turn/start",
            {"threadId": thread_id, "input": [{"type": "text", "text": prompt}]},
        )
        if self._await(request_id, min(timeout, 120)) is None:
            errors.append("codex app-server did not acknowledge `turn/start`")
        usage: dict[str, Any] | None = None
        question: str | None = None
        turn_id: str | None = None
        tool_calls = 0
        interrupted = False
        deadline = time.monotonic() + timeout
        seen = 0
        while time.monotonic() < deadline:
            with self._lock:
                batch = list(self._sink or [])[seen:]
                seen += len(batch)
                interruptions = self.stream_interruptions
            finished = False
            for message in batch:
                method = message.get("method")
                params = message.get("params") or {}
                if method == "item/completed":
                    item = params.get("item") or {}
                    if item.get("type") in TOOL_ITEM_TYPES_APP_SERVER:
                        tool_calls += 1
                    question = question or question_from_item(item)
                elif method == "thread/tokenUsage/updated":
                    # `turn/completed` does not carry usage in this protocol version;
                    # the running total does, and the last one before completion is
                    # the turn's.
                    usage = (params.get("tokenUsage") or {}).get("total") or usage
                elif method in {"turn/started", "turn/completed"}:
                    turn = params.get("turn") or {}
                    # The protocol does carry a per-turn identity; the replay used to discard it
                    # and label every turn with the thread id, so a three-turn manifest read as
                    # three records of one turn.
                    turn_id = turn.get("id") or turn.get("turnId") or turn_id
                    if method == "turn/completed":
                        usage = turn.get("usage") or usage
                        finished = True
                elif method in {"turn/failed", "error"}:
                    errors.append(json.dumps(params, ensure_ascii=False)[:400])
                    finished = True
            if finished:
                break
            if 0 < max_interruptions <= interruptions:
                errors.append(
                    f"the model stream disconnected or reconnected {interruptions} times in this "
                    "turn; aborting rather than producing an empty turn that looks replayed"
                )
                interrupted = True
                break
            if self.process is not None and self.process.poll() is not None:
                errors.append("codex app-server exited mid-turn")
                break
            time.sleep(0.1)
        else:
            errors.append(f"turn exceeded {timeout}s")
        with self._lock:
            batch = list(self._sink or [])
            self._sink = None
            interruptions = self.stream_interruptions
        # Codex writes its retry text to stderr too, and stderr is not read during the turn.
        interruptions += count_stream_interruptions(read_from(self.stderr_path, stderr_offset))
        if 0 < max_interruptions <= interruptions and not interrupted:
            errors.append(
                f"the model stream disconnected or reconnected {interruptions} times in this turn"
            )
            interrupted = True
        with stream_path.open("w", encoding="utf-8") as handle:
            for message in batch:
                handle.write(json.dumps(message, ensure_ascii=False) + "\n")
        if self.questions and question is None:
            question = self.questions[-1]
        return TurnOutcome(
            usage=usage,
            question=question,
            tool_calls=tool_calls,
            errors=errors,
            turn_id=turn_id,
            stream_interruptions=interruptions,
            aborted=interrupted,
        )

    def close(self) -> None:
        if self.process is None:
            return
        with contextlib.suppress(Exception):
            if self.process.stdin is not None:
                self.process.stdin.close()
        with contextlib.suppress(Exception):
            self.process.wait(timeout=10)
        if self.process.poll() is None:  # pragma: no cover - defensive
            self.process.terminate()
        with contextlib.suppress(Exception):
            self.transcript.close()


def wait_for_turn_stop(
    runtime_db: pathlib.Path, thread_id: str, timeout: int, probe_interval: float = 2.0
) -> bool:
    """Blocks until the Stop hook's `turn_stop` row lands for this thread.

    ``codex exec`` is synchronous, so by the time it returns the Stop hook has
    already been invoked; this barrier exists for the case where a future Codex
    detaches it. On installations where ``hook_event`` is not written live the
    caller detects the inert table once and stops paying for the wait.
    """
    if not runtime_db.exists():
        return False
    deadline = time.monotonic() + timeout
    while True:
        try:
            with contextlib.closing(sqlite3.connect(f"file:{runtime_db}?mode=ro", uri=True)) as db:
                found = db.execute(
                    "SELECT 1 FROM hook_event WHERE external_session_id = ? "
                    "AND event_kind = 'turn_stop' LIMIT 1",
                    (thread_id,),
                ).fetchone()
        except sqlite3.DatabaseError:
            return False
        if found:
            return True
        if time.monotonic() >= deadline:
            return False
        time.sleep(probe_interval)


# --------------------------------------------------------------------------
# Evidence
# --------------------------------------------------------------------------


def runtime_fingerprint(runtime_db: pathlib.Path) -> dict[str, int | None]:
    counts: dict[str, int | None] = {}
    if not runtime_db.exists():
        return counts
    try:
        with contextlib.closing(sqlite3.connect(f"file:{runtime_db}?mode=ro", uri=True)) as db:
            for table in RUNTIME_FINGERPRINT_TABLES:
                try:
                    counts[table] = db.execute(f"SELECT count(*) FROM {table}").fetchone()[0]
                except sqlite3.DatabaseError:
                    counts[table] = None
    except sqlite3.DatabaseError:
        return counts
    return counts


def state_fingerprint(root: pathlib.Path) -> dict[str, Any]:
    """A cheap, comparable summary of a Shared Context installation's durable state."""
    scopes = root / "state" / "authorized-session-scopes"
    return {
        "root": str(root),
        "authorized_session_scopes": len(list(scopes.glob("*.json"))) if scopes.is_dir() else None,
        "runtime_rows": runtime_fingerprint(root / "state" / "runtime.sqlite"),
    }


def replayed_thread_evidence(runtime_db: pathlib.Path, thread_id: str) -> dict[str, Any]:
    """Counts the durable Shared Context rows this thread produced in one installation.

    Run against both the isolated and the real state directory, this is the whole
    isolation argument in two numbers: the replay's rows exist over here and do
    not exist over there.
    """
    evidence: dict[str, Any] = {"external_session_key": thread_id, "external_session_id": None}
    if not runtime_db.exists():
        return evidence
    try:
        with contextlib.closing(sqlite3.connect(f"file:{runtime_db}?mode=ro", uri=True)) as db:
            row = db.execute(
                "SELECT external_session_id, active_task_session_id, active_task_id, "
                "checkpoint_reminder_count FROM external_session WHERE external_session_key = ?",
                (thread_id,),
            ).fetchone()
            if row is None:
                return evidence
            evidence["external_session_id"] = row[0]
            evidence["task_session_id"] = row[1]
            evidence["task_id"] = row[2]
            evidence["checkpoint_reminder_count"] = row[3]
            # Both tables hang off `task_id`, not the Task Session id: the Task
            # outlives any one Session, which is the point of the identity.
            for table in ("task_injection", "context_usage", "agent_checkpoint"):
                try:
                    evidence[f"{table}_rows"] = db.execute(
                        f"SELECT count(*) FROM {table} WHERE task_id = ?", (row[2],)
                    ).fetchone()[0]
                except sqlite3.DatabaseError:
                    evidence[f"{table}_rows"] = None
    except sqlite3.DatabaseError:
        return evidence
    return evidence


def collect_host_stderr(host_dir: pathlib.Path, max_lines: int = 6) -> list[str]:
    """Folds the host's stderr files into a handful of manifest warnings.

    Codex writes real problems to stderr and nothing else does -- unloadable
    skills, MCP servers that refused to start, config keys this build does not
    know. Left in a file nobody opens, those are exactly the differences that
    make a replay quietly incomparable to its original.
    """
    notes: list[str] = []
    for path in sorted(host_dir.glob("*.stderr")):
        try:
            lines = [line.strip() for line in path.read_text(
                encoding="utf-8", errors="replace").splitlines() if line.strip()]
        except OSError:  # pragma: no cover - defensive
            continue
        interesting = [
            line for line in lines
            if "ERROR" in line or "error" in line.lower() or "warn" in line.lower()
        ]
        if not interesting:
            continue
        # Repeated stack-ish noise collapses to its distinct lines; the count keeps
        # "this happened 40 times" visible without pasting it 40 times.
        seen: dict[str, int] = {}
        for line in interesting:
            seen[line[:300]] = seen.get(line[:300], 0) + 1
        for line, count in list(seen.items())[:max_lines]:
            suffix = f" (x{count})" if count > 1 else ""
            notes.append(f"{path.name}: {line}{suffix}")
        if len(seen) > max_lines:
            notes.append(f"{path.name}: ... and {len(seen) - max_lines} more distinct stderr lines")
    return notes


def find_replayed_rollout(home: pathlib.Path, thread_id: str) -> pathlib.Path | None:
    matches = sorted(
        pathlib.Path(path)
        for path in glob.glob(str(home / ROLLOUT_GLOB.format(thread_id=thread_id)))
    )
    return matches[-1] if matches else None


# --------------------------------------------------------------------------
# CLI
# --------------------------------------------------------------------------


def parse_turns(value: str) -> int | None:
    if value.strip().lower() == "all":
        return None
    try:
        count = int(value)
    except ValueError as error:
        raise argparse.ArgumentTypeError("--turns takes a positive integer or 'all'") from error
    if count < 1:
        raise argparse.ArgumentTypeError("--turns takes a positive integer or 'all'")
    return count


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Replay a real Codex session's human prompts into a fresh headless session."
    )
    parser.add_argument(
        "--agent",
        choices=("codex", "cursor"),
        default="codex",
        help="host that produced the session; `cursor` forwards every remaining "
        "argument to replay_cursor.py, which shares this script's snapshot, "
        "worktree and manifest machinery",
    )
    parser.add_argument("--session", required=True, help="original Codex thread id")
    parser.add_argument(
        "--turns", type=parse_turns, default=3, help="number of prompts to replay, or 'all'"
    )
    parser.add_argument(
        "--audit-root",
        default="~/.shared-context-audit",
        help="directory that holds replay homes and worktrees",
    )
    parser.add_argument(
        "--checkout",
        choices=("worktree", "inplace"),
        default="worktree",
        help="worktree: detached checkout at the original commit (default); "
        "inplace: run in the original working directory",
    )
    parser.add_argument(
        "--codex-home",
        choices=("isolated", "real"),
        default="isolated",
        help="isolated: a copied CODEX_HOME inside the audit directory (default); "
        "real: the operator's ~/.codex, with HOME still redirected",
    )
    parser.add_argument(
        "--driver",
        choices=("app-server", "exec-resume"),
        default="app-server",
        help="app-server: one `codex app-server` process for the whole replay, so the "
        "SessionStart hook fires once as it does interactively (default); "
        "exec-resume: one `codex exec [resume]` process per turn",
    )
    parser.add_argument(
        "--on-question",
        choices=("stop", "next-prompt"),
        default="stop",
        help="stop: end the replay after a turn in which the agent asked the operator "
        "something (default); next-prompt: answer with the next original prompt",
    )
    parser.add_argument(
        "--sctx-bin",
        default=None,
        help="replay a DEV BUILD instead of the installed sctx: the binary is copied into "
        "the isolated HOME's .shared-context/bin/current/sctx, the isolated hooks.json "
        "and mcp_servers command are pointed at it, matching Codex hook trust hashes are "
        "computed (see codex_trust.py), and the managed skill bundles are taken from the "
        "checkout it was built from. Omitted, nothing changes.",
    )
    parser.add_argument("--turn-timeout", type=int, default=1800, help="seconds per turn")
    parser.add_argument(
        "--turn-stop-timeout",
        type=int,
        default=120,
        help="seconds to wait for the Stop hook's turn_stop row between turns",
    )
    proxy = parser.add_mutually_exclusive_group()
    proxy.add_argument(
        "--proxy",
        default=None,
        metavar="URL",
        help="proxy for the Codex child process (sets HTTP_PROXY/HTTPS_PROXY/ALL_PROXY and their "
        "lowercase twins). Omitted, whatever proxy the calling shell exports is inherited -- "
        "which is how a replay silently fails: a proxy that serves HTTP fine can reset the "
        "app-server's websocket on every turn. Whatever is in force is recorded in "
        "manifest.environment.proxy.",
    )
    proxy.add_argument(
        "--no-proxy",
        action="store_true",
        help="strip every proxy variable from the Codex child's environment",
    )
    parser.add_argument(
        "--max-stream-interruptions",
        type=int,
        default=3,
        help="abort the run after this many stream disconnects or reconnects inside one turn "
        "(0 disables). A wedged connection otherwise produces a full set of empty turns and a "
        "manifest that looks like a completed replay",
    )
    parser.add_argument("--dry-run", action="store_true", help="print the plan and exit")
    return parser


def print_plan(original: Original, turns: int | None, replay_id: str, audit_root: pathlib.Path,
               args: argparse.Namespace, checkout: pathlib.Path) -> None:
    selected = original.prompts if turns is None else original.prompts[:turns]
    print("replay plan")
    print(f"  replay id           {replay_id}")
    print(f"  original thread     {original.thread_id}")
    print(f"  original rollout    {original.rollout_path}")
    print(f"  original cwd        {original.cwd}")
    print(f"  commit              {original.commit} ({original.branch})")
    print(f"  repository url      {original.repository_url}")
    print(f"  cli version         {original.cli_version}")
    print(f"  originator          {original.originator}")
    print(f"  thread source       {original.thread_source}")
    print(f"  model               {original.model}")
    print(f"  approval policy     {original.approval_policy} "
          f"(reviewer {original.approvals_reviewer})")
    print(f"  sandbox             {original.sandbox_type} "
          f"(network_access={original.sandbox_network_access})")
    print(f"  workspace roots     {original.workspace_roots}")
    print(f"  checkout mode       {args.checkout} -> {checkout}")
    print(f"  codex home mode     {args.codex_home}")
    print(f"  sctx binary         {args.sctx_bin or 'installed (~/.shared-context/bin/current)'}")
    print(f"  driver              {args.driver}")
    print(f"  on question         {args.on_question}")
    print(f"  audit root          {audit_root}")
    print(f"  human prompts       {len(original.prompts)} total, replaying {len(selected)}")
    for index, prompt in enumerate(selected, start=1):
        head = prompt.strip().splitlines()[0] if prompt.strip() else ""
        print(f"    turn {index}: {len(prompt)} chars, first line {len(head)} chars")
    print("  flags")
    print(f"    turn timeout      {args.turn_timeout}s")
    print(f"    turn stop timeout {args.turn_stop_timeout}s")


def _split_agent(argv: list[str]) -> tuple[str, list[str]]:
    """Pull `--agent <kind>` out of the argument list before parsing.

    Cursor replays are a different enough animal (no commit, no cwd, no model
    in the transcript; different flags on the host CLI) that they live in
    `replay_cursor.py`. Routing here rather than there keeps one entry point in
    muscle memory and one manifest schema on disk.
    """
    agent = "codex"
    remaining: list[str] = []
    index = 0
    while index < len(argv):
        token = argv[index]
        if token == "--agent" and index + 1 < len(argv):
            agent = argv[index + 1]
            index += 2
            continue
        if token.startswith("--agent="):
            agent = token.split("=", 1)[1]
            index += 1
            continue
        remaining.append(token)
        index += 1
    return agent, remaining


def main(argv: list[str] | None = None) -> int:
    agent, argv = _split_agent(list(sys.argv[1:] if argv is None else argv))
    if agent == "cursor":
        import replay_cursor  # local import: replay_cursor imports this module

        return replay_cursor.main(argv)
    if agent != "codex":
        raise ReplayError(f"unknown --agent {agent!r}; expected codex or cursor")
    args = build_parser().parse_args(argv)
    warnings: list[str] = []
    audit_root = pathlib.Path(os.path.expanduser(args.audit_root)).resolve()
    replay_id = (
        dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
        + "-"
        + args.session.split("-")[0]
    )
    codex = shutil.which("codex")
    if codex is None:
        raise ReplayError("codex is not on PATH")
    sctx_bin: pathlib.Path | None = None
    if args.sctx_bin:
        sctx_bin = pathlib.Path(os.path.expanduser(args.sctx_bin)).resolve()
        if not sctx_bin.is_file() or not os.access(sctx_bin, os.X_OK):
            raise ReplayError(f"--sctx-bin {sctx_bin} is not an executable file")

    rollout, index_row = locate_rollout(args.session)
    original = parse_original(args.session, rollout, index_row)
    checkout = (
        original.cwd
        if args.checkout == "inplace"
        else audit_root / "worktrees" / replay_id
    )

    if args.dry_run:
        print_plan(original, args.turns, replay_id, audit_root, args, checkout)
        return 0

    print_plan(original, args.turns, replay_id, audit_root, args, checkout)
    replay_dir = audit_root / replay_id
    private_mkdir(replay_dir)
    host_dir = replay_dir / "host"
    host_dir.mkdir(parents=True, exist_ok=True)

    if args.checkout == "inplace":
        print(
            "\n"
            "!! --checkout inplace: this replay runs inside the live working tree at\n"
            f"!! {original.cwd}\n"
            "!! The agent may create, modify or delete files there. Nothing in this script\n"
            "!! stashes, resets or restores that tree -- that is on you.\n",
            flush=True,
        )
        warn(warnings, f"replayed in place at {original.cwd}; the live working tree was writable")
        head = git_output(["-C", str(original.cwd), "rev-parse", "HEAD"])
        if original.commit and head != original.commit:
            warn(
                warnings,
                f"in-place tree is at {head}, not the original commit {original.commit}",
            )
        checkout_commit = head
        checkout_branch = git_output(["-C", str(original.cwd), "branch", "--show-current"]) or None
        checkout_record = {
            "branch_mode": "inplace" if checkout_branch else "inplace_detached",
            "attempts": [],
        }
        if checkout_branch is None:
            # The in-place path used to reach `git_branch: '-'` without saying anything at all.
            warn(
                warnings,
                f"{original.cwd} is a detached checkout; the replay records git_branch as '-', "
                "which is not what the original session saw",
            )
    else:
        checkout_commit, checkout_branch, checkout_record = prepare_worktree(
            original, checkout, replay_id, warnings
        )
        if checkout_branch is None:
            warn(
                warnings,
                "no branch could be created for the worktree, so it is detached and the replay "
                "records git_branch as '-'; see manifest.checkout.attempts for git's own reason",
            )

    real_home = pathlib.Path(os.path.expanduser("~"))
    home = replay_dir / "home"
    home_record = build_replay_home(
        home,
        real_home,
        warnings,
        overlay_exclusions=tuple(
            name for name in HOME_OVERLAY_EXCLUSIONS if name != ".cursor"
        ),
        skip_paths=(audit_root,),
        sctx_bin=sctx_bin,
    )
    binary = home / ".shared-context" / "bin" / "current" / "sctx"
    if not binary.exists():
        raise ReplayError(f"{binary} does not resolve; is Shared Context installed?")

    registration = register_checkout(home, binary, original.cwd, checkout, warnings)

    real_codex_home = codex_home()
    if args.codex_home == "isolated":
        effective_codex_home = replay_dir / "codex-home"
        codex_home_record = build_codex_home(
            effective_codex_home, real_codex_home, checkout, warnings, sctx_bin=sctx_bin
        )
    else:
        effective_codex_home = real_codex_home
        codex_home_record = {}
        warn(
            warnings,
            f"running against the real CODEX_HOME ({real_codex_home}); the replayed rollout "
            "lands beside the operator's own sessions",
        )
        if sctx_bin is not None:
            warn(
                warnings,
                "--sctx-bin with --codex-home real cannot retarget the hooks: the operator's "
                "~/.codex/hooks.json still names the INSTALLED binary, so the hooks run that "
                "one while the MCP server runs the dev build",
            )

    environment = dict(os.environ)
    environment["HOME"] = str(home)
    environment["CODEX_HOME"] = str(effective_codex_home)
    environment["NO_COLOR"] = "1"
    environment.pop("SCTX_LOGS_ROOT", None)
    environment_record = apply_proxy_policy(environment, args.proxy, args.no_proxy, warnings)

    collector = start_log_collector(
        binary if sctx_bin is not None else real_home / ".shared-context" / SCTX_BIN_RELATIVE,
        home / ".shared-context-logs",
        replay_dir / "log-collector.log",
    )
    if collector is None:
        warn(warnings, "the replay log collector did not start; hook diagnostics will be missing")

    real_before = state_fingerprint(real_home / ".shared-context")
    isolated_runtime = home / ".shared-context" / "state" / "runtime.sqlite"

    prompts = original.prompts if args.turns is None else original.prompts[: args.turns]
    results: list[TurnResult] = []
    replayed_thread: str | None = None
    hook_event_table_live = True
    stopped_on_question: str | None = None
    driver: AppServerDriver | None = None

    try:
        if args.driver == "app-server":
            driver = AppServerDriver(
                codex, environment, checkout, host_dir / "app-server.jsonl", warnings
            )
            driver.start()
            replayed_thread = driver.start_thread(original)
            log(f"thread {replayed_thread}")

        for index, prompt in enumerate(prompts, start=1):
            log(f"turn {index}/{len(prompts)} ({len(prompt)} chars)")
            stream_path = host_dir / f"turn-{index}.jsonl"
            stderr_path = host_dir / f"turn-{index}.stderr"
            started = dt.datetime.now(dt.timezone.utc).isoformat()
            answered = False
            turn_id: str | None = None
            interruptions = 0
            aborted = False
            if driver is not None:
                assert replayed_thread is not None
                if args.on_question == "next-prompt" and index < len(prompts):
                    driver.pending_answer = prompts[index]
                outcome = driver.run_turn(
                    replayed_thread,
                    prompt,
                    stream_path,
                    args.turn_timeout,
                    args.max_stream_interruptions,
                )
                usage, question, tool_calls, errors = (
                    outcome.usage,
                    outcome.question,
                    outcome.tool_calls,
                    outcome.errors,
                )
                turn_id = outcome.turn_id
                interruptions = outcome.stream_interruptions
                aborted = outcome.aborted
                answered = bool(question) and driver.pending_answer is not None
                driver.pending_answer = None
                status = 0 if not errors else 1
                thread_id = replayed_thread
                stderr_path = driver.stderr_path
            else:
                command = build_turn_command(
                    codex, original, prompt, checkout, home, binary, replayed_thread
                )
                status, thread_id, usage, errors, question, tool_calls = run_turn(
                    command, environment, checkout, stream_path, stderr_path, args.turn_timeout
                )
                interruptions = count_stream_interruptions(read_from(stderr_path, 0))
                if 0 < args.max_stream_interruptions <= interruptions:
                    errors = [
                        *errors,
                        "the model stream disconnected or reconnected "
                        f"{interruptions} times in this turn",
                    ]
                    status = status or 1
                    aborted = True
            finished = dt.datetime.now(dt.timezone.utc).isoformat()
            if thread_id and replayed_thread is None:
                replayed_thread = thread_id
            if status != 0:
                warn(warnings, f"turn {index} exited {status}; see {stderr_path}")
            for error in errors:
                warn(warnings, f"turn {index}: {error}")
            failed = status != 0 or bool(errors)

            waited = False
            if replayed_thread and hook_event_table_live and index < len(prompts):
                waited = wait_for_turn_stop(
                    isolated_runtime, replayed_thread, args.turn_stop_timeout
                )
                if not waited:
                    hook_event_table_live = False
                    warn(
                        warnings,
                        "no turn_stop row appeared in the isolated runtime.sqlite within "
                        f"{args.turn_stop_timeout}s; the Stop hook has already run by the time a "
                        "turn returns, and later turns skip this barrier",
                    )
            results.append(
                TurnResult(
                    prompt_index=index,
                    prompt_chars=len(prompt),
                    started=started,
                    finished=finished,
                    exit_code=status,
                    thread_id=thread_id,
                    usage=usage,
                    waited_for_turn_stop=waited,
                    stream_path=str(stream_path),
                    stderr_path=str(stderr_path),
                    errors=errors,
                    question=question,
                    answered_with_next_prompt=answered,
                    tool_calls=tool_calls,
                    turn_id=turn_id,
                    stream_interruptions=interruptions,
                    failed=failed,
                )
            )
            if aborted:
                # Every later turn would fail the same way and the manifest would describe a full
                # replay that produced nothing. Stop while the record still says what happened.
                warn(
                    warnings,
                    f"aborting after turn {index}: the model stream was interrupted "
                    f"{interruptions} times (see --max-stream-interruptions)",
                )
                break
            if replayed_thread is None:
                warn(warnings, "no thread id was reported; stopping before the resume turns")
                break
            if question and args.on_question == "stop":
                stopped_on_question = question
                warn(
                    warnings,
                    f"turn {index} ended with the agent asking the operator a question; "
                    "stopping (see manifest.stopped_on_question)",
                )
                break
    finally:
        if driver is not None:
            driver.close()

    if collector is not None:
        # The collector aggregates on a timer; give it a moment to drain the socket
        # before it is stopped, or the last turn's hook decisions never land.
        time.sleep(3)
        collector.terminate()
        with contextlib.suppress(Exception):
            collector.wait(timeout=10)

    for note in collect_host_stderr(host_dir):
        warn(warnings, note)

    diagnostics = home / ".shared-context-logs" / "state" / "hook-diagnostics.json"
    if not diagnostics.is_file():
        warn(warnings, f"no replay-side hook diagnostics at {diagnostics}")

    real_after = state_fingerprint(real_home / ".shared-context")
    isolated_after = state_fingerprint(home / ".shared-context")
    replay_evidence = (
        replayed_thread_evidence(isolated_runtime, replayed_thread) if replayed_thread else {}
    )
    real_evidence = (
        replayed_thread_evidence(
            real_home / ".shared-context" / "state" / "runtime.sqlite", replayed_thread
        )
        if replayed_thread
        else {}
    )
    replayed_rollout = (
        find_replayed_rollout(effective_codex_home, replayed_thread) if replayed_thread else None
    )
    if replayed_thread and replayed_rollout is None:
        warn(warnings, f"no rollout found for replayed thread {replayed_thread}")

    version = subprocess.run(
        [str(binary), "--version"], capture_output=True, text=True, check=False,
        env=dict(os.environ, HOME=str(home)),
    )
    codex_version = subprocess.run(
        [codex, "--version"], capture_output=True, text=True, check=False
    )

    deviations = [
        "approvals run headless (approvals_reviewer=auto_review) instead of the original "
        f"{original.approvals_reviewer or original.approval_policy!r} routing to a human",
        "no human thinking time between turns",
        "HOME is an overlay: everything but .shared-context/.shared-context-logs/.codex is a "
        "symlink to the real HOME, so a tool that writes through one writes to the real HOME",
        "CODEX_HOME is a copy when --codex-home isolated, so history and thread index start empty",
        "the worktree is the original commit, not the original working tree",
    ]
    if sctx_bin is not None:
        deviations.append(
            f"--sctx-bin: the replay ran the dev build at {sctx_bin} copied into the isolated "
            "HOME, not the operator's installed sctx; hooks.json and mcp_servers.shared-context "
            "were rewritten to name it and fresh Codex hook trust hashes were computed, and "
            "~/.agents/skills/{shared-context,sctx-review} came from that binary's own source "
            "tree rather than from the installed bundles"
        )
    if args.driver != "app-server":
        deviations.append(
            "session_start_per_turn: true -- `codex exec resume` starts a new process per turn, "
            "so the SessionStart hook (and the Shared Context activation marker) repeats on every "
            "turn, where an interactive original carries it once"
        )
    if stopped_on_question:
        deviations.append(
            "the replay stopped early because the agent asked the operator a question that "
            "nobody was there to answer"
        )

    # A run-level verdict, because there was none: three turns that all failed used to produce the
    # same manifest shape and the same exit code 0 as three clean ones, and the only difference
    # was buried in `warnings`.
    failed_turns = [result.prompt_index for result in results if result.failed]
    if not results:
        run_status = "no_turns"
    elif len(failed_turns) == len(results):
        run_status = "failed"
    elif failed_turns:
        run_status = "partial"
    elif stopped_on_question:
        run_status = "stopped_on_question"
    elif args.turns is None and len(results) < len(original.prompts):
        run_status = "incomplete"
    else:
        run_status = "completed"

    manifest = {
        "agent": "codex",
        "replay_id": replay_id,
        "created_at": dt.datetime.now(dt.timezone.utc).isoformat(),
        "run": {
            "status": run_status,
            "prompts_selected": len(prompts),
            "turns_attempted": len(results),
            "turns_failed": len(failed_turns),
            "failed_turn_indexes": failed_turns,
            "stream_interruptions": sum(result.stream_interruptions for result in results),
        },
        # The environment decides whether a replay can run at all -- a proxy that resets the
        # app-server's stream makes every turn produce nothing -- so it is part of the record.
        "environment": environment_record,
        "original": {
            "thread_id": original.thread_id,
            "rollout_path": str(original.rollout_path),
            "cwd": str(original.cwd),
            "commit": original.commit,
            "branch": original.branch,
            "repository_url": original.repository_url,
            "cli_version": original.cli_version,
            "originator": original.originator,
            "thread_source": original.thread_source,
            "model": original.model,
            "approval_policy": original.approval_policy,
            "approvals_reviewer": original.approvals_reviewer,
            "sandbox_type": original.sandbox_type,
            "sandbox_network_access": original.sandbox_network_access,
            "workspace_roots": original.workspace_roots,
            "human_prompt_count": len(original.prompts),
        },
        "replayed_thread_id": replayed_thread,
        "replayed_rollout_path": str(replayed_rollout) if replayed_rollout else None,
        "stopped_on_question": stopped_on_question,
        "checkout": {
            "mode": args.checkout,
            "path": str(checkout),
            "commit": checkout_commit,
            "branch": checkout_branch,
            "shared_context_repository_id": registration["repository_id"],
            "registered_path": registration["registered_path"],
            **checkout_record,
        },
        "codex_home": {
            "mode": args.codex_home,
            "path": str(effective_codex_home),
            "real_path": str(real_codex_home),
            **codex_home_record,
        },
        "home": home_record,
        "flags": {
            "driver": args.driver,
            "on_question": args.on_question,
            "turns_requested": "all" if args.turns is None else args.turns,
            # Attempted, not succeeded. `run.turns_failed` is the other half.
            "turns_replayed": len(results),
            "max_stream_interruptions": args.max_stream_interruptions,
            "turn_timeout_seconds": args.turn_timeout,
            "turn_stop_timeout_seconds": args.turn_stop_timeout,
            "sandbox": sandbox_flag(original),
            "approvals": (
                "--approve-for-me on turn 1"
                if sandbox_flag(original) == "workspace-write"
                else f"-s {sandbox_flag(original)} on turn 1"
            )
            + ", approvals_reviewer=auto_review on every turn",
            "config_overrides": shared_config_overrides(original, home, binary),
        },
        "turns": [result.__dict__ for result in results],
        "sctx": {
            "binary": str(binary),
            "resolved_binary": str(binary.resolve()) if binary.exists() else None,
            "version": version.stdout.strip() or version.stderr.strip(),
        },
        # Present only under --sctx-bin. `sctx.version` alone cannot tell a dev
        # build from the installed one (both print the workspace version), so the
        # sha256 is what actually identifies the bytes that ran.
        "sctx_bin": home_record.get("sctx_bin"),
        "skill_bundle": home_record.get("skill_bundle"),
        "codex": {
            "executable": codex,
            "version": codex_version.stdout.strip() or codex_version.stderr.strip(),
        },
        "isolation": {
            "real_shared_context_before": real_before,
            "real_shared_context_after": real_after,
            "real_shared_context_unchanged": real_before == real_after,
            "replay_shared_context_after": isolated_after,
            "replayed_thread_in_replay_state": replay_evidence,
            "replayed_thread_in_real_state": real_evidence,
        },
        "deviations": deviations,
        "warnings": warnings,
    }
    manifest_path = replay_dir / "manifest.json"
    manifest_path.write_text(json.dumps(manifest, indent=2, ensure_ascii=False), encoding="utf-8")
    log(f"manifest {manifest_path}")
    if replayed_rollout:
        log(f"rollout  {replayed_rollout}")
    log(f"run      {run_status} ({len(failed_turns)}/{len(results)} turns failed)")
    # A non-zero exit for a run that produced nothing. `main` used to return 0 unconditionally, so
    # a caller had no way to tell a replay worth auditing from a replay that never reached the
    # model.
    return 1 if run_status in {"failed", "no_turns"} else 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except ReplayError as error:
        print(f"[replay:error] {error}", file=sys.stderr)
        sys.exit(1)
    except KeyboardInterrupt:  # pragma: no cover
        print("[replay:error] interrupted", file=sys.stderr)
        sys.exit(130)
