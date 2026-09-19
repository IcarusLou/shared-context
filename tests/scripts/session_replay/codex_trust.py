#!/usr/bin/env python3
"""Reproduce Codex's hook *trust hash*, so a replay can trust a hooks.json it wrote itself.

Codex will not run a hook unless ``config.toml`` carries a
``[hooks.state."<key source>:<event>:<group index>:<handler index>"]`` entry
whose ``trusted_hash`` equals the hash Codex recomputes from the hook entry at
discovery time. `replay.py` used to sidestep this by copying the operator's
``hooks.json`` byte for byte and rewriting only the path prefix inside those
keys -- which works exactly as long as the hook entries are unchanged, and so
pins every replay to the ``sctx`` binary the operator has *installed*. To replay
a dev build the driver has to write its own ``hooks.json`` (different command
path) and therefore has to be able to compute the hash itself.

The algorithm is public. Read from openai/codex at commit
``c7c824dce4da186e5142af5d9a1587ae553efe46`` (2026-09-01, "Treat bundled cleanup
hooks as built-ins (#42110)"):

- ``codex-rs/hooks/src/engine/discovery.rs`` -- ``hook_hash`` builds a
  ``NormalizedHookIdentity { event_name, #[serde(flatten)] group: MatcherGroup }``
  where the group's ``matcher`` is the event-adjusted matcher and its ``hooks``
  is the single *normalized* handler, serializes it to a ``toml::Value``, and
  hands that to ``version_for_toml``. ``append_matcher_groups`` right above it is
  where the normalization happens (command/timeout/additionalContextLimit).
- ``codex-rs/hooks/src/lib.rs`` -- ``hook_event_key_label`` (the snake_case event
  label used in the state key) and ``hook_key`` (``"{key_source}:{label}:{gi}:{hi}"``).
- ``codex-rs/hooks/src/events/common.rs`` -- ``matcher_pattern_for_event``:
  UserPromptSubmit / Stop / Interrupt drop their matcher entirely.
- ``codex-rs/hooks/src/events/session_end.rs`` -- SessionEnd/Interrupt timeouts
  default to 1s and clamp to [1, 3]; every other event defaults to 600s.
- ``codex-rs/config/src/hook_config.rs`` -- the serde shape of ``HooksFile`` /
  ``MatcherGroup`` / ``HookHandlerConfig`` (field renames, and which fields are
  skipped when ``None``).
- ``codex-rs/config/src/fingerprint.rs`` -- ``version_for_toml``: TOML value ->
  ``serde_json::Value`` -> recursively key-sorted -> compact ``serde_json``
  bytes -> sha256 -> ``"sha256:<hex>"``.

Two Rust behaviours are load-bearing and easy to miss when reimplementing:

- ``toml::Value::try_from`` **drops** map entries whose value is ``None``, so an
  absent matcher / ``commandWindows`` / ``statusMessage`` contributes no key at
  all rather than a null.
- ``timeout`` is normalized *before* hashing and is always ``Some``, so a hook
  entry that states no timeout still hashes as if it said ``600``.

Proof rather than assertion: ``python3 codex_trust.py --config ~/.codex/config.toml``
recomputes every ``[hooks.state]`` entry whose key source is a hooks file that
exists on disk and reports how many match the stored ``trusted_hash``.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import pathlib
import re
import sys
from typing import Any, Iterator, NamedTuple

# The commit these rules were read at. Quoted in the module docstring too; kept
# here so a drift check has something machine-readable to compare against.
CODEX_SOURCE_COMMIT = "c7c824dce4da186e5142af5d9a1587ae553efe46"

# hooks.json spells events in PascalCase (`HookEventsToml`'s serde renames);
# the persisted state key spells them snake_case (`hook_event_key_label`).
EVENT_KEY_LABELS: dict[str, str] = {
    "PreToolUse": "pre_tool_use",
    "PermissionRequest": "permission_request",
    "PostToolUse": "post_tool_use",
    "PreCompact": "pre_compact",
    "PostCompact": "post_compact",
    "SessionStart": "session_start",
    "SessionEnd": "session_end",
    "UserPromptSubmit": "user_prompt_submit",
    "SubagentStart": "subagent_start",
    "SubagentStop": "subagent_stop",
    "Stop": "stop",
    "Interrupt": "interrupt",
}

# `matcher_pattern_for_event`: these three events ignore whatever matcher the
# group states, so the hashed identity must drop it too.
EVENTS_WITHOUT_MATCHER = frozenset({"UserPromptSubmit", "Stop", "Interrupt"})

# `normalize_command_hook`.
SESSION_END_DEFAULT_TIMEOUT_SEC = 1
SESSION_END_MAX_TIMEOUT_SEC = 3
SHORT_TIMEOUT_EVENTS = frozenset({"SessionEnd", "Interrupt"})
DEFAULT_TIMEOUT_SEC = 600

# Only these events can emit `additionalContext`, so only they keep an
# `additionalContextLimit`; and a limit equal to the default is dropped.
ADDITIONAL_CONTEXT_EVENTS = frozenset(
    {"PreToolUse", "PostToolUse", "SessionStart", "UserPromptSubmit", "SubagentStart"}
)
DEFAULT_HOOK_OUTPUT_TOKEN_LIMIT = 2500

# Handler kinds Codex refuses to load at all: they `continue` before ever being
# hashed, so they own a handler index but no state entry.
UNSUPPORTED_HANDLER_TYPES = frozenset({"prompt", "agent"})


class TrustError(RuntimeError):
    """A hooks.json Codex itself would reject, or a shape this module cannot hash."""


class HookHandlerRef(NamedTuple):
    """One handler in a hooks.json, addressed exactly as Codex addresses it."""

    event: str  # PascalCase, as written in hooks.json
    group_index: int
    handler_index: int
    matcher: str | None  # already event-adjusted
    handler: dict[str, Any]  # the raw entry, before normalization


def hook_key(key_source: str, event: str, group_index: int, handler_index: int) -> str:
    """`hook_key` in codex-rs/hooks/src/lib.rs."""
    label = EVENT_KEY_LABELS.get(event)
    if label is None:
        raise TrustError(f"unknown hook event {event!r}")
    return f"{key_source}:{label}:{group_index}:{handler_index}"


def _normalize_timeout(event: str, timeout: Any) -> int:
    if timeout is not None and (isinstance(timeout, bool) or not isinstance(timeout, int)):
        raise TrustError(f"non-integer timeout {timeout!r} in a {event} hook")
    if event in SHORT_TIMEOUT_EVENTS:
        value = SESSION_END_DEFAULT_TIMEOUT_SEC if timeout is None else timeout
        return max(1, min(value, SESSION_END_MAX_TIMEOUT_SEC))
    return max(1, DEFAULT_TIMEOUT_SEC if timeout is None else timeout)


def normalize_handler(event: str, handler: dict[str, Any]) -> dict[str, Any] | None:
    """The `HookHandlerConfig` Codex hashes, as a plain dict of the fields it serializes.

    Returns ``None`` for a handler Codex skips (empty command, prompt/agent
    hooks, MCP hooks on SessionEnd) -- those consume a handler index but get no
    hash and no state entry.
    """
    kind = handler.get("type")
    if kind in UNSUPPORTED_HANDLER_TYPES:
        return None
    if kind == "command":
        # `command_windows.unwrap_or(command)` only on Windows; the replay driver
        # and every host it drives are POSIX, so the plain `command` wins and
        # `commandWindows` is normalized away to None (and therefore dropped).
        command = handler.get("command") or ""
        if not command.strip():
            return None
        config: dict[str, Any] = {
            "type": "command",
            "command": command,
            "timeout": _normalize_timeout(event, handler.get("timeout")),
            "async": bool(handler.get("async", False)),
        }
        status = handler.get("statusMessage")
        if status is not None:
            config["statusMessage"] = status
        limit = handler.get("additionalContextLimit")
        if (
            limit is not None
            and event in ADDITIONAL_CONTEXT_EVENTS
            and limit != DEFAULT_HOOK_OUTPUT_TOKEN_LIMIT
        ):
            config["additionalContextLimit"] = limit
        return config
    if kind == "mcp_tool":
        if event == "SessionEnd":
            return None
        server = handler.get("server") or ""
        tool = handler.get("tool") or ""
        if not server.strip() or not tool.strip():
            return None
        config = {
            "type": "mcp_tool",
            "server": server,
            "tool": tool,
            # `input` carries no skip_serializing_if, so an empty map is still a key.
            "input": handler.get("input") or {},
            "timeout": _normalize_timeout(event, handler.get("timeout")),
        }
        status = handler.get("statusMessage")
        if status is not None:
            config["statusMessage"] = status
        return config
    raise TrustError(f"unsupported hook handler type {kind!r} in a {event} hook")


def _canonical(value: Any) -> Any:
    if isinstance(value, dict):
        return {key: _canonical(value[key]) for key in sorted(value)}
    if isinstance(value, list):
        return [_canonical(item) for item in value]
    return value


def version_for_toml(value: Any) -> str:
    """`version_for_toml` in codex-rs/config/src/fingerprint.rs."""
    serialized = json.dumps(
        _canonical(value), separators=(",", ":"), ensure_ascii=False
    ).encode("utf-8")
    return "sha256:" + hashlib.sha256(serialized).hexdigest()


def hook_hash(event: str, matcher: str | None, handler: dict[str, Any]) -> str | None:
    """`hook_hash` in codex-rs/hooks/src/engine/discovery.rs.

    ``matcher`` is the *raw* group matcher; this applies
    ``matcher_pattern_for_event`` itself. Returns ``None`` when Codex would skip
    the handler instead of hashing it.
    """
    if event not in EVENT_KEY_LABELS:
        raise TrustError(f"unknown hook event {event!r}")
    normalized = normalize_handler(event, handler)
    if normalized is None:
        return None
    identity: dict[str, Any] = {"event_name": EVENT_KEY_LABELS[event]}
    effective_matcher = None if event in EVENTS_WITHOUT_MATCHER else matcher
    # `toml::Value::try_from` drops a None-valued map entry entirely.
    if effective_matcher is not None:
        identity["matcher"] = effective_matcher
    identity["hooks"] = [normalized]
    return version_for_toml(identity)


def iter_hook_handlers(hooks_file: dict[str, Any]) -> Iterator[HookHandlerRef]:
    """Walks a parsed hooks.json in Codex's own order, yielding addressable handlers.

    Group and handler indices come from position, not from what survives
    normalization: Codex enumerates first and skips second, so a skipped entry
    still owns its index.
    """
    events = hooks_file.get("hooks") or {}
    if not isinstance(events, dict):
        raise TrustError("hooks.json's `hooks` is not an object")
    for event in EVENT_KEY_LABELS:
        groups = events.get(event) or []
        for group_index, group in enumerate(groups):
            matcher = group.get("matcher")
            if event in EVENTS_WITHOUT_MATCHER:
                matcher = None
            for handler_index, handler in enumerate(group.get("hooks") or []):
                yield HookHandlerRef(event, group_index, handler_index, matcher, handler)


def hook_state_entries(hooks_file: dict[str, Any], key_source: str) -> dict[str, str]:
    """Every ``[hooks.state]`` key this hooks.json needs, mapped to its trusted hash."""
    entries: dict[str, str] = {}
    for ref in iter_hook_handlers(hooks_file):
        digest = hook_hash(ref.event, ref.matcher, ref.handler)
        if digest is None:
            continue
        entries[hook_key(key_source, ref.event, ref.group_index, ref.handler_index)] = digest
    return entries


def load_hooks_file(path: pathlib.Path) -> dict[str, Any]:
    body = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(body, dict):
        raise TrustError(f"{path} is not a JSON object")
    return body


def render_hook_state(entries: dict[str, str]) -> str:
    """The ``[hooks.state]`` TOML block for the given key -> hash mapping.

    Written as fully-qualified `[hooks.state."<key>"]` tables so the block can be
    appended to a config.toml that already has other tables after its own
    `[hooks.state]` header without being swallowed by the wrong parent.
    """
    lines: list[str] = []
    for key, digest in entries.items():
        # A TOML basic string and a JSON string escape identically for the
        # characters a filesystem path can hold.
        lines.append(f"[hooks.state.{json.dumps(key)}]")
        lines.append(f"trusted_hash = {json.dumps(digest)}")
        lines.append("")
    return "\n".join(lines)


# --------------------------------------------------------------------------
# Verification against a real config.toml
# --------------------------------------------------------------------------

_STATE_TABLE = re.compile(r'^\[hooks\.state\."((?:[^"\\]|\\.)*)"\]\s*$')
_TRUSTED_HASH = re.compile(r'^\s*trusted_hash\s*=\s*"([^"]*)"\s*$')


def read_config_hook_states(config: pathlib.Path) -> dict[str, str]:
    """Extracts ``key -> trusted_hash`` from a Codex config.toml.

    Parsed line-wise rather than with ``tomllib`` because the state keys are
    absolute paths whose quoting round-trips more predictably this way, and
    because this has to work on the operator's real file, not a fixture.
    """
    states: dict[str, str] = {}
    current: str | None = None
    for line in config.read_text(encoding="utf-8").splitlines():
        match = _STATE_TABLE.match(line)
        if match:
            current = json.loads(f'"{match.group(1)}"')
            continue
        if line.startswith("["):
            current = None
            continue
        if current is None:
            continue
        hash_match = _TRUSTED_HASH.match(line)
        if hash_match:
            states[current] = hash_match.group(1)
    return states


def verify_config(config: pathlib.Path) -> dict[str, Any]:
    """Recomputes every state entry whose key source is a hooks file on disk.

    Plugin-provided hook sources (``<plugin>@<pack>:hooks/hooks.json``) name no
    readable path and are reported as unresolved rather than counted as misses.
    """
    states = read_config_hook_states(config)
    by_source: dict[str, list[str]] = {}
    unresolved: list[str] = []
    for key in states:
        # `hook_key` is "<key source>:<event>:<group index>:<handler index>", and a
        # key source may itself contain colons, so the split is from the right.
        source = key.rsplit(":", 3)[0]
        if source.startswith("/") and pathlib.Path(source).is_file():
            by_source.setdefault(source, []).append(key)
        else:
            unresolved.append(key)

    matched: list[str] = []
    mismatched: list[dict[str, str]] = []
    missing: list[str] = []
    for source, keys in sorted(by_source.items()):
        computed = hook_state_entries(load_hooks_file(pathlib.Path(source)), source)
        for key in sorted(keys):
            if key not in computed:
                missing.append(key)
            elif computed[key] == states[key]:
                matched.append(key)
            else:
                mismatched.append(
                    {"key": key, "stored": states[key], "computed": computed[key]}
                )
    return {
        "config": str(config),
        "state_entries": len(states),
        "sources_resolved": sorted(by_source),
        "matched": matched,
        "mismatched": mismatched,
        "missing_from_hooks_file": missing,
        "unresolved_key_sources": sorted(unresolved),
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--config",
        default="~/.codex/config.toml",
        help="Codex config.toml whose [hooks.state] entries are recomputed",
    )
    parser.add_argument("--json", action="store_true", help="print the full report as JSON")
    args = parser.parse_args(argv)
    report = verify_config(pathlib.Path(args.config).expanduser())
    if args.json:
        print(json.dumps(report, indent=2, ensure_ascii=False))
    else:
        print(f"config                 {report['config']}")
        print(f"[hooks.state] entries  {report['state_entries']}")
        print(f"resolved hook sources  {len(report['sources_resolved'])}")
        for source in report["sources_resolved"]:
            print(f"  {source}")
        print(f"matched                {len(report['matched'])}")
        print(f"mismatched             {len(report['mismatched'])}")
        for row in report["mismatched"]:
            print(f"  {row['key']}\n    stored   {row['stored']}\n    computed {row['computed']}")
        print(f"missing from hooks.json {len(report['missing_from_hooks_file'])}")
        for key in report["missing_from_hooks_file"]:
            print(f"  {key}")
        print(f"unresolved key sources {len(report['unresolved_key_sources'])} (plugins)")
    return 1 if report["mismatched"] else 0


if __name__ == "__main__":
    sys.exit(main())
