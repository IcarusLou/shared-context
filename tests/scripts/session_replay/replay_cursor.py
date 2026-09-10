#!/usr/bin/env python3
"""Replay a real Cursor Agent session's human prompts into a fresh headless one.

The Codex sibling (``replay.py``) states the bar: a replay is only worth
auditing when the run it produces could plausibly have been the original. This
script keeps that bar and reuses ``replay.py``'s snapshot machinery verbatim --
the same 0700 ``HOME`` snapshot, the same Repository re-registration, the same
detached worktree, the same before/after fingerprints, the same manifest schema
-- so one ``bundle.py`` and one ``/session-review`` read both hosts.

Usage::

    python3 tests/scripts/session_replay/replay_cursor.py --session <conversation_id> \
        [--turns N|all] [--audit-root ~/.shared-context-audit] \
        [--checkout worktree|inplace] [--cursor-home isolated|real] \
        [--cwd PATH] [--commit SHA] [--model MODEL] [--dry-run]

``replay.py --agent cursor ...`` forwards here, so either entry point works.

What Cursor forces this script to do differently from the Codex path, all of it
recorded in ``manifest.json`` under ``deviations``:

- **The commit is a guess.** A Cursor transcript records no git metadata at all
  -- no commit, no branch, no repository url, not even the cwd. The commit is
  therefore resolved as ``git -C <cwd> log -1 --before=<turn 1's timestamp>``
  on whatever branch the checkout is on *now*, and the manifest carries
  ``commit_source`` saying exactly that. ``--commit`` overrides it when the
  operator knows better.
- **The cwd is recovered, not read.** See ``hosts.cursor.resolve_cwd``: sctx's
  own activation lease stores ``startup_cwd`` verbatim, and if the lease has
  been reclaimed the transcript's project-directory slug is matched against the
  registered Shared Context checkouts. ``--cwd`` overrides both.
- **There is no hook-trust handshake to repair.** Codex refuses to run a hook
  unless ``config.toml`` carries a path-keyed ``trusted_hash``; Cursor has no
  equivalent -- ``~/.cursor/hooks.json`` is ``{"hooks": {...}, "version": 1}``
  with no hashes, and nothing under ``~/.cursor`` records per-hook trust. An
  isolated ``CURSOR_CONFIG_DIR`` whose ``hooks.json`` names the snapshot binary
  simply runs it. (Verified end to end -- see the README's cursor section.)
- **Hook payloads are teed through a recorder.** Cursor's hooks are plain
  commands, so each is registered as a two-line ``sh`` wrapper that appends the
  payload to ``host/hook-input.jsonl``, runs the *real* snapshot ``sctx hook``
  binary with the same arguments and the same stdin, appends its reply to
  ``host/hook-output.jsonl`` and prints that reply unchanged. This is the only
  way to see what sctx was handed, because Cursor writes hook traffic into
  neither the transcript nor its own logs.
- **Authentication is shared, never copied.** Cursor authenticates against the
  macOS keychain under ``$HOME/Library/Keychains``; the snapshot HOME symlinks
  that directory and copies only the ``authInfo``/``version`` keys of
  ``cli-config.json``. No token is read, printed or passed as an argument.
"""

from __future__ import annotations

import argparse
import contextlib
import datetime as dt
import json
import os
import pathlib
import re
import shutil
import subprocess
import sys
import time
from dataclasses import dataclass, field
from typing import Any

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

import replay as codex_replay  # noqa: E402
from hosts import cursor as cursor_host  # noqa: E402
from replay import (  # noqa: E402
    ReplayError,
    build_replay_home,
    git_output,
    log,
    prepare_worktree,
    private_mkdir,
    register_checkout,
    replayed_thread_evidence,
    state_fingerprint,
    warn,
)

CURSOR_AGENT_DEFAULT = "cursor-agent"

# `cli-config.json` keys that are safe and necessary to carry into the isolated
# config directory. `authInfo` is the credential handle Cursor resolves against
# the keychain; nothing here is a secret in itself and none of it is logged.
CLI_CONFIG_KEYS = ("version", "authInfo", "model", "permissions")


@dataclass
class CursorOriginal:
    """Everything recoverable about a real Cursor session, and where from."""

    conversation_id: str
    transcript_path: pathlib.Path
    cwd: pathlib.Path
    cwd_source: str
    commit: str | None
    commit_source: str
    branch: str | None
    model: str | None
    model_source: str
    agent_version: str | None
    started_at: str | None
    ended_at: str | None
    prompts: list[str]
    prompt_started_at: list[str | None]
    host_prompts_skipped: int
    sctx_tool_calls: list[str] = field(default_factory=list)


def _transcript_mtime(path: pathlib.Path) -> dt.datetime:
    return dt.datetime.fromtimestamp(path.stat().st_mtime, dt.timezone.utc)


def resolve_commit(
    cwd: pathlib.Path, before: dt.datetime | None, explicit: str | None
) -> tuple[str | None, str, str | None]:
    """Return ``(commit, how, branch)`` for a session that records neither."""
    if not (cwd / ".git").exists():
        return None, f"{cwd} is not a git checkout", None
    try:
        branch = git_output(["-C", str(cwd), "rev-parse", "--abbrev-ref", "HEAD"])
    except ReplayError:
        branch = None
    if explicit:
        return explicit, "operator-supplied --commit", branch
    if before is None:
        head = git_output(["-C", str(cwd), "rev-parse", "HEAD"])
        return head, (
            f"GUESS: transcript records no commit and no timestamp; used HEAD of "
            f"branch {branch!r} at replay time"
        ), branch
    stamp = before.astimezone(dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%S+00:00")
    found = git_output(
        ["-C", str(cwd), "log", "-1", "--format=%H", f"--before={stamp}"]
    )
    if not found:
        return None, f"no commit on branch {branch!r} predates {stamp}", branch
    return found, (
        f"GUESS: transcript records no commit; last commit on branch {branch!r} "
        f"before the transcript's own first-turn timestamp {stamp}"
    ), branch


def resolve_original(
    conversation_id: str,
    home: pathlib.Path,
    cursor_home: pathlib.Path,
    explicit_cwd: str | None,
    explicit_commit: str | None,
    explicit_model: str | None,
) -> CursorOriginal:
    transcript = cursor_host.resolve_transcript(conversation_id, cursor_home=cursor_home)
    if transcript is None:
        raise ReplayError(
            f"no Cursor transcript for {conversation_id} under {cursor_home / 'projects'}"
        )
    session = cursor_host.parse_transcript(transcript, conversation_id=conversation_id)
    if not session.turns:
        raise ReplayError(f"{transcript} carries no human prompts to replay")

    if explicit_cwd:
        cwd, cwd_source = explicit_cwd, "operator-supplied --cwd"
    else:
        cwd, cwd_source = cursor_host.resolve_cwd(
            transcript, home=home, external_session_id=conversation_id
        )
    if not cwd:
        raise ReplayError(
            f"cannot recover the startup directory of {conversation_id} ({cwd_source}); "
            "pass --cwd"
        )

    first_stamp = session.turns[0].started_at
    before = dt.datetime.fromisoformat(first_stamp) if first_stamp else _transcript_mtime(transcript)
    if not first_stamp:
        before_source = "transcript file mtime (no <timestamp> tag in the transcript)"
    else:
        before_source = "turn 1's <timestamp> tag"
    commit, commit_source, branch = resolve_commit(
        pathlib.Path(cwd), before, explicit_commit
    )
    commit_source = f"{commit_source} [clock: {before_source}]"

    agent_version = None
    hooks_json = cursor_home / "hooks.json"
    if hooks_json.is_file():
        match = re.search(r"--agent-version\s+['\"]([^'\"]+)['\"]", hooks_json.read_text())
        agent_version = match.group(1) if match else None

    # Cursor records no model in the transcript, so there is nothing faithful to
    # pass. `--model` is therefore left OFF unless the operator supplies one,
    # and the run inherits whatever the copied `cli-config.json` names as the
    # account default -- which is exactly what an interactive session would have
    # done. (Passing the config's `model.modelId` explicitly is worse, not
    # better: it is a display alias like `grok-4.6`, which `--model` on a
    # `--resume` turn rejects with "Cannot use this model", so a replay that
    # looked fine on turn 1 would die on turn 2.)
    config_default = None
    config = cursor_home / "cli-config.json"
    if config.is_file():
        try:
            document = json.loads(config.read_text(encoding="utf-8"))
            config_default = (document.get("model") or {}).get("modelId")
        except (json.JSONDecodeError, OSError):
            pass
    if explicit_model:
        model, model_source = explicit_model, "operator-supplied --model"
    else:
        model, model_source = None, (
            "NOT RECORDED by Cursor and NOT overridden: no --model flag is "
            "passed, so the run uses the account default the copied "
            f"cli-config.json names ({config_default or 'unknown'})"
        )

    return CursorOriginal(
        conversation_id=conversation_id,
        transcript_path=transcript,
        cwd=pathlib.Path(cwd),
        cwd_source=cwd_source,
        commit=commit,
        commit_source=commit_source,
        branch=branch,
        model=model,
        model_source=model_source,
        agent_version=agent_version,
        started_at=session.meta.started_at,
        ended_at=session.meta.ended_at,
        prompts=[turn.user_text for turn in session.turns],
        prompt_started_at=[turn.started_at for turn in session.turns],
        host_prompts_skipped=len(session.other_injections),
        sctx_tool_calls=[call.tool for turn in session.turns for call in turn.sctx_calls],
    )


# --------------------------------------------------------------------------
# The isolated Cursor configuration
# --------------------------------------------------------------------------


HOOK_WRAPPER = """#!/bin/sh
# Written by tests/scripts/session_replay/replay_cursor.py. Tees one Cursor
# hook payload to disk and hands it, byte for byte, to the snapshot's real
# `sctx hook`; prints that reply unchanged so Cursor sees exactly what it would
# have seen without this wrapper.
payload=$(cat)
printf '%s\\n' "$payload" >> {input_log}
reply=$(printf '%s' "$payload" | {binary} hook --agent cursor --agent-version {version})
status=$?
printf '%s\\n' "$reply" >> {output_log}
printf '%s' "$reply"
exit $status
"""


def build_cursor_home(
    cursor_dir: pathlib.Path,
    real_cursor_home: pathlib.Path,
    home: pathlib.Path,
    binary: pathlib.Path,
    agent_version: str,
    host_dir: pathlib.Path,
    warnings: list[str],
) -> dict[str, Any]:
    """Create the replay's ``CURSOR_CONFIG_DIR``: hooks, MCP, credentials.

    Only three files matter. ``cli-config.json`` carries the credential handle
    (and the model/permission defaults, so the replay is approved the same way
    the original was). ``hooks.json`` registers all six sctx hook events against
    the recorder wrapper. ``mcp.json`` registers the ``shared-context`` stdio
    server against the snapshot binary with ``HOME`` pinned to the snapshot, so
    the MCP side and the hook side agree on which installation they are in.

    Unlike Codex there is no trust state to repair: Cursor's ``hooks.json``
    holds no hashes and nothing else under ``~/.cursor`` gates hook execution,
    so a freshly written file in a fresh config directory is honoured as-is.
    """
    private_mkdir(cursor_dir)
    hook_dir = private_mkdir(cursor_dir.parent / ".sctx-replay-hooks")

    real_config = real_cursor_home / "cli-config.json"
    config: dict[str, Any] = {}
    if real_config.is_file():
        try:
            document = json.loads(real_config.read_text(encoding="utf-8"))
            config = {key: document[key] for key in CLI_CONFIG_KEYS if key in document}
        except (json.JSONDecodeError, OSError) as error:
            warn(warnings, f"{real_config} is unreadable ({error}); Cursor may not be logged in")
    if "authInfo" not in config:
        warn(
            warnings,
            f"{real_config} carries no authInfo; the replay may not be authenticated "
            "in the isolated config directory",
        )
    (cursor_dir / "cli-config.json").write_text(json.dumps(config, indent=2), encoding="utf-8")
    (cursor_dir / "cli-config.json").chmod(0o600)

    input_log = host_dir / "hook-input.jsonl"
    output_log = host_dir / "hook-output.jsonl"
    wrapper = hook_dir / "record-hook.sh"
    wrapper.write_text(
        HOOK_WRAPPER.format(
            input_log=_sh_quote(str(input_log)),
            output_log=_sh_quote(str(output_log)),
            binary=_sh_quote(str(binary)),
            version=_sh_quote(agent_version),
        ),
        encoding="utf-8",
    )
    wrapper.chmod(0o700)

    real_hooks = real_cursor_home / "hooks.json"
    events = [
        "sessionStart",
        "beforeSubmitPrompt",
        "postToolUse",
        "stop",
        "preCompact",
        "sessionEnd",
    ]
    hooks_trust_note = "cursor registers hooks by plain command; no trusted-hash state exists"
    if real_hooks.is_file():
        try:
            document = json.loads(real_hooks.read_text(encoding="utf-8"))
            events = sorted((document.get("hooks") or {}).keys()) or events
            if any(
                key not in ("hooks", "version") for key in document
            ):  # pragma: no cover - future-proofing
                hooks_trust_note = (
                    f"{real_hooks} carries keys beyond hooks/version "
                    f"({sorted(document)}); check whether a trust mechanism was added"
                )
        except (json.JSONDecodeError, OSError):
            warn(warnings, f"{real_hooks} is unreadable; using the default cursor hook event list")
    hooks = {
        "hooks": {event: [{"command": str(wrapper)}] for event in events},
        "version": 1,
    }
    (cursor_dir / "hooks.json").write_text(json.dumps(hooks, indent=2), encoding="utf-8")

    mcp = {
        "mcpServers": {
            "shared-context": {
                "type": "stdio",
                "command": str(binary),
                "args": ["mcp", "serve", "--client", "cursor"],
                "env": {"HOME": str(home)},
            }
        }
    }
    (cursor_dir / "mcp.json").write_text(json.dumps(mcp, indent=2), encoding="utf-8")

    # Cursor resolves its stored credential through the macOS keychain under
    # $HOME/Library/Keychains. The directory is symlinked, never copied, and
    # nothing in this script reads it.
    keychains = pathlib.Path.home() / "Library" / "Keychains"
    keychain_linked = False
    if keychains.is_dir():
        target = home / "Library"
        target.mkdir(parents=True, exist_ok=True)
        if not (target / "Keychains").exists():
            os.symlink(keychains, target / "Keychains", target_is_directory=True)
        keychain_linked = True
    else:
        warn(warnings, f"{keychains} is missing; Cursor may not authenticate in the replay")

    for name in ("skills", "skills-cursor", "plugins", "extensions", "sandbox-policies"):
        source = real_cursor_home / name
        if source.exists() and not (cursor_dir / name).exists():
            os.symlink(source, cursor_dir / name)

    return {
        "mode": "isolated",
        "path": str(cursor_dir),
        "real_path": str(real_cursor_home),
        "hook_events": events,
        "hook_wrapper": str(wrapper),
        "hook_input_log": str(input_log),
        "hook_output_log": str(output_log),
        "hook_trust": hooks_trust_note,
        "mcp_server": "shared-context (stdio, snapshot binary, HOME pinned to the snapshot)",
        "keychain_symlinked": keychain_linked,
        "cli_config_keys": sorted(config),
    }


def _sh_quote(value: str) -> str:
    return "'" + value.replace("'", "'\\''") + "'"


# --------------------------------------------------------------------------
# Driving the turns
# --------------------------------------------------------------------------


@dataclass
class CursorTurnResult:
    prompt_index: int
    prompt_chars: int
    started: str
    finished: str
    exit_code: int
    conversation_id: str | None
    resumed: bool
    turn_ended_status: str | None
    stream_path: str
    stderr_path: str
    errors: list[str] = field(default_factory=list)


def build_turn_command(
    agent: str,
    original: CursorOriginal,
    prompt: str,
    checkout: pathlib.Path,
    resume_id: str | None,
) -> list[str]:
    command = [
        agent,
        "--print",
        "--output-format",
        "stream-json",
        # The original ran interactively with a human (or Cursor's own smart
        # reviewer) answering approvals. Headless has no keyboard, so the
        # closest available stand-in is the same server-side auto reviewer plus
        # blanket MCP approval; `--force`/`--yolo` is deliberately NOT used,
        # because it would let the replay run commands the original's reviewer
        # would have stopped.
        "--auto-review",
        "--approve-mcps",
        "--trust",
        "--workspace",
        str(checkout),
    ]
    if original.model:
        command += ["--model", original.model]
    if resume_id:
        command += ["--resume", resume_id]
    command.append(prompt)
    return command


def run_turn(
    command: list[str],
    environment: dict[str, str],
    checkout: pathlib.Path,
    stream_path: pathlib.Path,
    stderr_path: pathlib.Path,
    timeout: int,
) -> tuple[int, str | None, str | None, list[str]]:
    """Run one headless Cursor turn, teeing its stream-json to disk.

    The conversation id arrives on the ``{"type":"system","subtype":"init"}``
    event as ``session_id`` -- the same id that names the transcript directory,
    that the hooks report as ``conversation_id``, and that the model passes to
    sctx as ``external_session_id``.
    """
    conversation_id: str | None = None
    turn_status: str | None = None
    errors: list[str] = []
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
            start_new_session=True,
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
                if event.get("type") == "system" and event.get("subtype") == "init":
                    if event.get("session_id") and conversation_id is None:
                        conversation_id = event["session_id"]
                        log(f"conversation {conversation_id}")
                elif event.get("type") == "result":
                    turn_status = event.get("subtype") or event.get("status")
                    if event.get("is_error"):
                        errors.append(json.dumps(event)[:400])
                elif event.get("type") == "error":
                    errors.append(json.dumps(event)[:400])
                if time.monotonic() > deadline:
                    with contextlib.suppress(ProcessLookupError):
                        os.killpg(process.pid, 15)
                    errors.append(f"turn exceeded {timeout}s and was killed")
                    break
        try:
            status = process.wait(timeout=max(1.0, deadline - time.monotonic()))
        except subprocess.TimeoutExpired:
            with contextlib.suppress(ProcessLookupError):
                os.killpg(process.pid, 15)
            status = 124
            errors.append(f"turn exceeded {timeout}s and was killed")
    return status, conversation_id, turn_status, errors


def transcript_inventory(cursor_home: pathlib.Path) -> list[str]:
    projects = cursor_home / "projects"
    if not projects.is_dir():
        return []
    return sorted(
        path.parent.name
        for path in projects.glob("*/agent-transcripts/*/*.jsonl")
    )


def hook_marker_evidence(host_dir: pathlib.Path, conversation_id: str | None) -> dict[str, Any]:
    """Prove the hooks fired inside the isolated HOME, from their own payloads.

    ``hook-input.jsonl`` is what Cursor handed sctx; ``hook-output.jsonl`` is
    what sctx replied. A ``<shared-context-active ...>`` marker in a reply is
    the activation itself, and it names the session it activated -- so the two
    files together show both that the hook ran and that it ran for this
    conversation.
    """
    evidence: dict[str, Any] = {
        "hook_input_lines": 0,
        "hook_output_lines": 0,
        "events": {},
        "conversation_ids": [],
        "marker_replies": 0,
        "marker_session_ids": [],
    }
    inputs = host_dir / "hook-input.jsonl"
    outputs = host_dir / "hook-output.jsonl"
    if inputs.is_file():
        for line in inputs.read_text(encoding="utf-8", errors="replace").splitlines():
            if not line.strip():
                continue
            evidence["hook_input_lines"] += 1
            try:
                payload = json.loads(line)
            except json.JSONDecodeError:
                continue
            name = payload.get("hook_event_name") or "unknown"
            evidence["events"][name] = evidence["events"].get(name, 0) + 1
            found = payload.get("conversation_id") or payload.get("session_id")
            if found and found not in evidence["conversation_ids"]:
                evidence["conversation_ids"].append(found)
    if outputs.is_file():
        body = outputs.read_text(encoding="utf-8", errors="replace")
        for line in body.splitlines():
            if not line.strip():
                continue
            evidence["hook_output_lines"] += 1
            if "shared-context-active" in line:
                evidence["marker_replies"] += 1
                # The marker travels inside the hook reply's JSON
                # `additionalContext` string, so its quotes arrive escaped as
                # `\"` — matching a bare `"` finds the marker but never its id.
                for match in re.findall(r'external_session_id=\\?"([^"\\]+)', line):
                    if match not in evidence["marker_session_ids"]:
                        evidence["marker_session_ids"].append(match)
    evidence["marker_names_replayed_conversation"] = bool(
        conversation_id and conversation_id in evidence["marker_session_ids"]
    )
    return evidence


# --------------------------------------------------------------------------
# CLI
# --------------------------------------------------------------------------


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Replay a real Cursor session's human prompts into a fresh headless session."
    )
    parser.add_argument("--session", required=True, help="original Cursor conversation id")
    parser.add_argument(
        "--turns", type=codex_replay.parse_turns, default=2, help="prompts to replay, or 'all'"
    )
    parser.add_argument("--audit-root", default="~/.shared-context-audit")
    parser.add_argument("--checkout", choices=("worktree", "inplace"), default="worktree")
    parser.add_argument(
        "--cursor-home",
        choices=("isolated", "real"),
        default="isolated",
        help="isolated: a fresh CURSOR_CONFIG_DIR inside the replay HOME (default); "
        "real: the operator's ~/.cursor, with HOME still redirected",
    )
    parser.add_argument("--cwd", default=None, help="override the recovered startup directory")
    parser.add_argument("--commit", default=None, help="override the guessed commit")
    parser.add_argument("--model", default=None, help="override the model")
    parser.add_argument("--agent-bin", default=CURSOR_AGENT_DEFAULT)
    parser.add_argument("--turn-timeout", type=int, default=1800, help="seconds per turn")
    parser.add_argument("--dry-run", action="store_true", help="print the plan and exit")
    return parser


def print_plan(original: CursorOriginal, turns: int | None, replay_id: str, args, checkout) -> None:
    selected = original.prompts if turns is None else original.prompts[:turns]
    print("cursor replay plan")
    print(f"  replay id           {replay_id}")
    print(f"  conversation        {original.conversation_id}")
    print(f"  transcript          {original.transcript_path}")
    print(f"  cwd                 {original.cwd}")
    print(f"    recovered via     {original.cwd_source}")
    print(f"  commit              {original.commit} (branch {original.branch})")
    print(f"    resolved via      {original.commit_source}")
    print(f"  model               {original.model}")
    print(f"    resolved via      {original.model_source}")
    print(f"  hook agent version  {original.agent_version}")
    print(f"  session window      {original.started_at} .. {original.ended_at}")
    print(f"  checkout mode       {args.checkout} -> {checkout}")
    print(f"  cursor home mode    {args.cursor_home}")
    print(f"  sctx calls in orig  {len(original.sctx_tool_calls)} {sorted(set(original.sctx_tool_calls))}")
    print(f"  host prompts skipped {original.host_prompts_skipped}")
    print(f"  human prompts       {len(original.prompts)} total, replaying {len(selected)}")
    for index, prompt in enumerate(selected, start=1):
        head = prompt.strip().splitlines()[0] if prompt.strip() else ""
        stamp = original.prompt_started_at[index - 1]
        print(f"    turn {index}: {len(prompt)} chars, first line {len(head)} chars, at {stamp}")


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    warnings: list[str] = []
    audit_root = pathlib.Path(os.path.expanduser(args.audit_root)).resolve()
    replay_id = (
        dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
        + "-cursor-"
        + args.session.split("-")[0]
    )
    agent = shutil.which(args.agent_bin) or shutil.which("agent")
    if agent is None:
        raise ReplayError(f"{args.agent_bin} is not on PATH")

    real_home = pathlib.Path(os.path.expanduser("~"))
    real_cursor_home = pathlib.Path(
        os.environ.get("CURSOR_CONFIG_DIR") or real_home / ".cursor"
    )
    original = resolve_original(
        args.session, real_home, real_cursor_home, args.cwd, args.commit, args.model
    )
    checkout = (
        original.cwd if args.checkout == "inplace" else audit_root / "worktrees" / replay_id
    )

    print_plan(original, args.turns, replay_id, args, checkout)
    if args.dry_run:
        return 0

    replay_dir = audit_root / replay_id
    private_mkdir(replay_dir)
    host_dir = replay_dir / "host"
    host_dir.mkdir(parents=True, exist_ok=True)

    if args.checkout == "inplace":
        print(
            f"\n!! --checkout inplace: this replay runs inside the live tree at {original.cwd}\n"
            "!! Nothing here stashes, resets or restores it.\n",
            flush=True,
        )
        warn(warnings, f"replayed in place at {original.cwd}; the live working tree was writable")
        checkout_commit = git_output(["-C", str(original.cwd), "rev-parse", "HEAD"])
        checkout_branch = (
            git_output(["-C", str(original.cwd), "branch", "--show-current"]) or None
        )
    else:
        # `prepare_worktree` only reads `.thread_id`, `.cwd`, `.commit` and
        # `.branch`, so the Codex-shaped record is filled in just far enough.
        checkout_commit, checkout_branch = prepare_worktree(
            codex_replay.Original(
                thread_id=original.conversation_id,
                rollout_path=original.transcript_path,
                cwd=original.cwd,
                commit=original.commit,
                branch=original.branch,
                repository_url=None,
                cli_version=original.agent_version,
                originator="cursor-agent",
                thread_source=None,
                model=original.model,
                approval_policy=None,
                approvals_reviewer=None,
                sandbox_type=None,
                sandbox_network_access=None,
                workspace_roots=[],
                prompts=original.prompts,
            ),
            checkout,
            replay_id,
        )
        if checkout_branch is None:
            warn(warnings, "the worktree is detached; the replay records git_branch as '-'")

    home = replay_dir / "home"
    home_record = build_replay_home(
        home, real_home, warnings, skip_paths=(audit_root,)
    )
    binary = home / ".shared-context" / "bin" / "current" / "sctx"
    if not binary.exists():
        raise ReplayError(f"{binary} does not resolve; is Shared Context installed?")

    registration = register_checkout(home, binary, original.cwd, checkout, warnings)

    if args.cursor_home == "isolated":
        cursor_dir = home / ".cursor"
        cursor_record = build_cursor_home(
            cursor_dir,
            real_cursor_home,
            home,
            binary,
            original.agent_version or "unknown",
            host_dir,
            warnings,
        )
    else:
        cursor_dir = real_cursor_home
        cursor_record = {"mode": "real", "path": str(cursor_dir), "real_path": str(cursor_dir)}
        warn(
            warnings,
            f"running against the real CURSOR_CONFIG_DIR ({cursor_dir}); the replayed "
            "transcript lands beside the operator's own sessions and its hooks are the "
            "operator's, pointing at the REAL ~/.shared-context",
        )

    environment = dict(os.environ)
    environment["HOME"] = str(home)
    environment["CURSOR_CONFIG_DIR"] = str(cursor_dir)
    environment["CURSOR_DATA_DIR"] = str(cursor_dir)
    environment["CURSOR_AGENT_STORE_DIR"] = str(replay_dir / "agent-store")
    environment["NO_COLOR"] = "1"
    environment.pop("SCTX_LOGS_ROOT", None)

    real_before = state_fingerprint(real_home / ".shared-context")
    real_transcripts_before = transcript_inventory(real_cursor_home)
    isolated_runtime = home / ".shared-context" / "state" / "runtime.sqlite"

    prompts = original.prompts if args.turns is None else original.prompts[: args.turns]
    results: list[CursorTurnResult] = []
    replayed_conversation: str | None = None

    for index, prompt in enumerate(prompts, start=1):
        log(f"turn {index}/{len(prompts)} ({len(prompt)} chars)")
        command = build_turn_command(agent, original, prompt, checkout, replayed_conversation)
        stream_path = host_dir / f"turn-{index}.jsonl"
        stderr_path = host_dir / f"turn-{index}.stderr"
        started = dt.datetime.now(dt.timezone.utc).isoformat()
        status, conversation_id, turn_status, errors = run_turn(
            command, environment, checkout, stream_path, stderr_path, args.turn_timeout
        )
        finished = dt.datetime.now(dt.timezone.utc).isoformat()
        resumed = replayed_conversation is not None
        if conversation_id and replayed_conversation is None:
            replayed_conversation = conversation_id
        elif conversation_id and conversation_id != replayed_conversation:
            warn(
                warnings,
                f"turn {index} reported conversation {conversation_id}, not the resumed "
                f"{replayed_conversation}; --resume did not continue the same session",
            )
        if status != 0:
            warn(warnings, f"turn {index} exited {status}; see {stderr_path}")
        for error in errors:
            warn(warnings, f"turn {index}: {error}")
        results.append(
            CursorTurnResult(
                prompt_index=index,
                prompt_chars=len(prompt),
                started=started,
                finished=finished,
                exit_code=status,
                conversation_id=conversation_id,
                resumed=resumed,
                turn_ended_status=turn_status,
                stream_path=str(stream_path),
                stderr_path=str(stderr_path),
                errors=errors,
            )
        )
        if replayed_conversation is None:
            warn(warnings, "no conversation id was reported; stopping before the resume turns")
            break

    real_after = state_fingerprint(real_home / ".shared-context")
    real_transcripts_after = transcript_inventory(real_cursor_home)
    isolated_after = state_fingerprint(home / ".shared-context")
    replay_evidence = (
        replayed_thread_evidence(isolated_runtime, replayed_conversation)
        if replayed_conversation
        else {}
    )
    real_evidence = (
        replayed_thread_evidence(
            real_home / ".shared-context" / "state" / "runtime.sqlite", replayed_conversation
        )
        if replayed_conversation
        else {}
    )
    replayed_transcript = (
        cursor_host.resolve_transcript(replayed_conversation, cursor_home=cursor_dir)
        if replayed_conversation
        else None
    )
    if replayed_conversation and replayed_transcript is None:
        warn(warnings, f"no transcript found for replayed conversation {replayed_conversation}")

    version = subprocess.run(
        [str(binary), "--version"], capture_output=True, text=True, check=False,
        env=dict(os.environ, HOME=str(home)),
    )
    agent_version = subprocess.run(
        [agent, "--version"], capture_output=True, text=True, check=False, env=environment
    )

    manifest = {
        "replay_id": replay_id,
        "agent": "cursor",
        "created_at": dt.datetime.now(dt.timezone.utc).isoformat(),
        "original": {
            "thread_id": original.conversation_id,
            "conversation_id": original.conversation_id,
            "transcript_path": str(original.transcript_path),
            "rollout_path": str(original.transcript_path),
            "cwd": str(original.cwd),
            "cwd_source": original.cwd_source,
            "commit": original.commit,
            "commit_source": original.commit_source,
            "branch": original.branch,
            "repository_url": None,
            "cli_version": original.agent_version,
            "originator": "cursor-agent",
            "thread_source": None,
            "model": original.model,
            "model_source": original.model_source,
            "approval_policy": None,
            "approvals_reviewer": None,
            "sandbox_type": None,
            "sandbox_network_access": None,
            "workspace_roots": [str(original.cwd)],
            "human_prompt_count": len(original.prompts),
            "host_prompts_skipped": original.host_prompts_skipped,
            "started_at": original.started_at,
            "ended_at": original.ended_at,
            "sctx_tool_calls": original.sctx_tool_calls,
        },
        "replayed_thread_id": replayed_conversation,
        "replayed_rollout_path": str(replayed_transcript) if replayed_transcript else None,
        "replayed_transcript_path": str(replayed_transcript) if replayed_transcript else None,
        "checkout": {
            "mode": args.checkout,
            "path": str(checkout),
            "commit": checkout_commit,
            "branch": checkout_branch,
            "shared_context_repository_id": registration["repository_id"],
            "registered_path": registration["registered_path"],
        },
        "cursor_home": cursor_record,
        "home": home_record,
        "flags": {
            "turns_requested": "all" if args.turns is None else args.turns,
            "turns_replayed": len(results),
            "turn_timeout_seconds": args.turn_timeout,
            "approvals": "--auto-review --approve-mcps --trust (no --force/--yolo)",
            "command_shape": build_turn_command(
                agent, original, "<prompt>", checkout, "<conversation-id>"
            ),
        },
        "turns": [result.__dict__ for result in results],
        "sctx": {
            "binary": str(binary),
            "resolved_binary": str(binary.resolve()) if binary.exists() else None,
            "version": version.stdout.strip() or version.stderr.strip(),
        },
        "cursor": {
            "executable": agent,
            "version": agent_version.stdout.strip() or agent_version.stderr.strip(),
        },
        "isolation": {
            "real_shared_context_before": real_before,
            "real_shared_context_after": real_after,
            "real_shared_context_unchanged": real_before == real_after,
            "replay_shared_context_after": isolated_after,
            "replayed_thread_in_replay_state": replay_evidence,
            "replayed_thread_in_real_state": real_evidence,
            "real_cursor_transcripts_before": len(real_transcripts_before),
            "real_cursor_transcripts_after": len(real_transcripts_after),
            "real_cursor_unchanged": real_transcripts_before == real_transcripts_after,
            "real_cursor_new_transcripts": sorted(
                set(real_transcripts_after) - set(real_transcripts_before)
            ),
            "hooks": hook_marker_evidence(host_dir, replayed_conversation),
        },
        "deviations": [
            "the original commit is a GUESS: Cursor transcripts record no git "
            f"metadata at all ({original.commit_source})",
            f"the startup directory is recovered, not recorded ({original.cwd_source})",
            f"the model is not recorded by Cursor ({original.model_source})",
            "approvals run headless (--auto-review --approve-mcps --trust) instead of a "
            "human answering Cursor's prompts interactively",
            "no human thinking time between turns",
            "HOME points at the replay snapshot, so Shared Context state is a copy",
            "CURSOR_CONFIG_DIR is a freshly written directory when --cursor-home isolated, "
            "so chat history, workspace trust and per-project state start empty",
            "each sctx hook runs behind a two-line sh recorder that tees the payload to "
            "host/hook-input.jsonl before invoking the real binary with the same stdin",
            "Cursor's own auto follow-up prompts (see hosts/cursor.py "
            "HOST_QUERY_PREFIXES) are not replayed; only human prompts are",
        ],
        "warnings": warnings,
    }
    manifest_path = replay_dir / "manifest.json"
    manifest_path.write_text(json.dumps(manifest, indent=2, ensure_ascii=False), encoding="utf-8")
    log(f"manifest {manifest_path}")
    if replayed_transcript:
        log(f"transcript {replayed_transcript}")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except ReplayError as error:
        print(f"[replay:error] {error}", file=sys.stderr)
        sys.exit(1)
    except KeyboardInterrupt:  # pragma: no cover
        print("[replay:error] interrupted", file=sys.stderr)
        sys.exit(130)
