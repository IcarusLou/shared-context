"""Read sctx's local telemetry/log-service state for one external session.

On current installs, hook decisions are NOT durably recorded per-session in
`runtime.sqlite`'s `hook_event` table any more (see `sctx_facts.py`'s module
docstring and `README.md`'s replay section, which independently hit the same
fact) — they go to `<HOME>/.shared-context-logs/state/hook-diagnostics.json`,
a rolling in-memory-style aggregate maintained by the log collector service.
This module reads that file (plus the collector/upload status sidecars and
the outbound spool) and, where possible, filters `hook-diagnostics.json`
down to just this session's events.

## Verified on a real install (`~/.shared-context-logs/state/`)

`hook-diagnostics.json` is a dict: `schema_version`, `observed_from_unix_ms`,
`updated_at_unix_ms`, `total`, `counts` (list of {decision, reason, count}
aggregated over the whole rolling window), `recent_events` (a bounded list —
874 entries observed against a 151-total-since-`observed_from` counter, so
this is a ring buffer, not the full history), `hourly`.

Each `recent_events` entry has `operation` (e.g. `"hook.codex.turn_stop"`,
`"hook.codex.session_end"`, `"hook.codex.undecodable"`), `outcome`
(`success`/`fail_open`/`degraded`/...), `reason`, `occurred_at_unix_ms`, and
`session_digest`. Session-keying works: `session_digest` is
`sha256(agent_kind_bytes + b"\\x00" + external_session_id_bytes)`, matching
`telemetry_session_digest()` in `crates/cli/src/main.rs`. Filtering
`01a08017-95a0-7841-8ab9-09b582c62cb1`'s 27 matching events out of the 874
in the buffer reproduces exactly its turn sequence (session_start ->
prompt_submit -> post_tool_use* -> turn_stop, twice), ending in a
`turn_stop`, with NO `session_end` entry among them.

**This is also exactly why "SessionEnd payload undecodable" is undecidable
by session-digest alone**: `hook.codex.undecodable` events (the payload
failed to decode before the session id could even be read out of it) always
carry `session_digest: null` — there is structurally no way to attribute an
undecodable event to a session via this field. What IS decidable: whether
THIS session's own event history (by digest) contains a `session_end`
operation at all. If it does not, and the session's isolation lease file
still exists (see `sctx_facts.get_lease_facts`), that is strong indirect
evidence the SessionEnd hook never successfully attributed itself to this
session, whether because it failed to decode or for some other reason.

`collector-status.json`: `schema_version`, `accepted`, `persisted`,
`observable_dropped`, `invalid_frames`, `last_heartbeat_unix_ms`,
`storage_pressure`, `last_error`.

`upload-status.json`: `schema_version`, `last_attempt_unix_ms`,
`last_success_unix_ms`, `last_error`, `last_error_stage`, `last_error_code`,
`last_error_detail`, `last_error_retryable`, `last_attempt_uploaded_batches`,
`last_attempt_uploaded_bytes`, `next_retry_unix_ms`,
`consecutive_retryable_failures`, `automatic_retry_blocked`,
`target_digest`, `configured_target_digest`, `blocked_stream_id`. A
"logs_sync failure" is visible as `last_error` non-null, or
`automatic_retry_blocked: true`, or `consecutive_retryable_failures > 0`.

`spool/ready/` holds finalized batches waiting to be uploaded (an empty
directory means nothing is backed up); `spool/active/` holds the
currently-being-written batch and is NOT counted as "ready".

All of this is GLOBAL install state, not scoped to one session — the
`upload_status`/`collector_status`/`spool_ready_batch_count` fields reflect
the state of the whole log pipeline at bundle time, which for a real
(non-replay) bundle may be long after the session itself ran.
"""
from __future__ import annotations

import hashlib
import json
from pathlib import Path
from typing import Optional


def telemetry_session_digest(agent_kind: str, external_session_id: str) -> Optional[str]:
    """Reproduces `telemetry_session_digest()` in crates/cli/src/main.rs:
    sha256(agent_kind_bytes + b"\\x00" + external_session_id_bytes), gated
    the same way (agent_kind in {"cursor","codex"}, non-empty, <=256 chars).
    """
    if agent_kind not in ("cursor", "codex"):
        return None
    if not external_session_id or len(external_session_id) > 256:
        return None
    h = hashlib.sha256()
    h.update(agent_kind.encode("utf-8"))
    h.update(b"\x00")
    h.update(external_session_id.encode("utf-8"))
    return h.hexdigest()


def _read_json(path: Path):
    if not path.is_file():
        return None
    try:
        with open(path, "r", encoding="utf-8") as f:
            return json.load(f)
    except (json.JSONDecodeError, OSError):
        return None


def get_logs_facts(
    logs_root: Path, agent_kind: str, external_session_id: str, out_dir: Path
) -> dict:
    """Returns the `sctx_logs` facts.json block for one session, and writes
    the matched (or, if unmatched, full) diagnostics data to `out_dir` for
    audit. `logs_root` is `<HOME>/.shared-context-logs`.
    """
    out_dir.mkdir(parents=True, exist_ok=True)
    diagnostics = _read_json(logs_root / "state" / "hook-diagnostics.json")
    collector_status = _read_json(logs_root / "state" / "collector-status.json")
    upload_status = _read_json(logs_root / "state" / "upload-status.json")
    ready_dir = logs_root / "spool" / "ready"
    spool_ready_batch_count = len(list(ready_dir.iterdir())) if ready_dir.is_dir() else None

    result = {
        "diagnostics_file_found": diagnostics is not None,
        "collector_status": collector_status,
        "upload_status": upload_status,
        "spool_ready_batch_count": spool_ready_batch_count,
        "logs_sync_failure": _is_sync_failure(upload_status),
    }

    if diagnostics is None:
        result["keyed_by_session"] = False
        result["reason"] = f"{logs_root}/state/hook-diagnostics.json not found or unreadable"
        _dump(out_dir, "hook_diagnostics_matched_events.json", [])
        return result

    result["diagnostics_totals"] = {
        "schema_version": diagnostics.get("schema_version"),
        "total": diagnostics.get("total"),
        "counts": diagnostics.get("counts"),
        "observed_from_unix_ms": diagnostics.get("observed_from_unix_ms"),
        "updated_at_unix_ms": diagnostics.get("updated_at_unix_ms"),
    }

    digest = telemetry_session_digest(agent_kind, external_session_id)
    events = diagnostics.get("recent_events") or []
    matched = [e for e in events if digest and e.get("session_digest") == digest]

    if not matched:
        # Not an error: the session may simply have aged out of the rolling
        # `recent_events` buffer, or ran under a different HOME/agent_kind.
        result["keyed_by_session"] = False
        result["session_digest"] = digest
        result["reason"] = (
            "no recent_events row matched this session's digest (buffer holds "
            f"{len(events)} events total); falling back to the whole file's summary"
        )
        _dump(out_dir, "hook_diagnostics_matched_events.json", [])
        return result

    matched_sorted = sorted(matched, key=lambda e: e.get("occurred_at_unix_ms", 0))
    by_operation_outcome_reason: dict = {}
    for e in matched_sorted:
        key = f"{e.get('operation')}|{e.get('outcome')}|{e.get('reason')}"
        by_operation_outcome_reason[key] = by_operation_outcome_reason.get(key, 0) + 1
    session_end_found = any(e.get("operation", "").endswith(".session_end") for e in matched_sorted)
    undecodable_total_in_buffer = sum(
        1 for e in events if e.get("operation", "").endswith(".undecodable")
    )
    last = matched_sorted[-1]
    nearest_undecodable_after_seconds = _nearest_undecodable_after(
        events, last.get("occurred_at_unix_ms")
    )

    result.update(
        {
            "keyed_by_session": True,
            "session_digest": digest,
            "event_count": len(matched_sorted),
            "by_operation_outcome_reason": by_operation_outcome_reason,
            "first_event_at_unix_ms": matched_sorted[0].get("occurred_at_unix_ms"),
            "last_event_at_unix_ms": last.get("occurred_at_unix_ms"),
            "last_event_operation": last.get("operation"),
            "last_event_outcome": last.get("outcome"),
            "session_end_event_found": session_end_found,
            "undecodable_events_total_in_buffer": undecodable_total_in_buffer,
            "nearest_undecodable_after_last_event_seconds": nearest_undecodable_after_seconds,
        }
    )
    _dump(out_dir, "hook_diagnostics_matched_events.json", matched_sorted)
    return result


def _nearest_undecodable_after(events: list, after_ms) -> Optional[float]:
    if after_ms is None:
        return None
    after = [
        e.get("occurred_at_unix_ms")
        for e in events
        if e.get("operation", "").endswith(".undecodable") and e.get("occurred_at_unix_ms", 0) > after_ms
    ]
    if not after:
        return None
    return (min(after) - after_ms) / 1000.0


def _is_sync_failure(upload_status: Optional[dict]) -> Optional[bool]:
    if upload_status is None:
        return None
    return bool(
        upload_status.get("last_error")
        or upload_status.get("automatic_retry_blocked")
        or (upload_status.get("consecutive_retryable_failures") or 0) > 0
    )


def _dump(out_dir: Path, name: str, data) -> None:
    with open(out_dir / name, "w", encoding="utf-8") as f:
        json.dump(data, f, indent=2, ensure_ascii=False, sort_keys=True)
