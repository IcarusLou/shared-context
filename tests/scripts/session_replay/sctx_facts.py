"""Query sctx's own `runtime.sqlite` for the rows belonging to one external
(host) session, export them per-table to `sctx/<table>.json`, and produce a
compact summary dict for `facts.json`'s `sctx_db` field.

Schema is NOT hardcoded blindly: every SELECT is `SELECT *` and columns are
read back via `PRAGMA table_info` / `cursor.description`, so a superset or
reordering of columns in a future migration still exports correctly. The
*join keys* connecting tables to one session, however, are necessarily
hardcoded — they were verified against `crates/task-runtime/src/lib.rs`
(`CREATE TABLE` statements, schema version 19) and spot-checked against a
real `runtime.sqlite` on this machine for session 01a08017-95a0-7841-8ab9-
09b582c62cb1:

  external_session(agent_kind, external_session_key=<thread_id>)
    --external_session_id (xss_...)--> task_session(external_session_id)
      (a session can have MULTIPLE task_session rows over its life — one per
       task_boundary="new"; 01a08017 has two, task_ordinal 0 and 1 — so we
       fetch ALL of them, not just external_session.active_task_session_id)
    --task_session_id / task_id--> task_intent_revision, task_signal,
      work_episode, agent_checkpoint, candidate_build, checkpoint_operation,
      candidate_review, task_injection(task_id), context_usage(task_id)
    --build_id--> candidate_build_item, candidate_build_duplicate
    --candidate_id--> candidate_confirmation_operation

  hook_event.external_session_id and auto_confirm_rejection.external_session_id
  are the RAW host thread id (== external_session.external_session_key),
  NOT the xss_... id — verified: hook_event rows for this DB include values
  like "01a08017-95a0-..." (36 chars, no "xss_" prefix) alongside a few
  non-UUID placeholder ids ("thr_real_shape_01", ""), so we filter on the
  raw thread id directly.

Known surprise (see report): `hook_event` had ZERO rows for session
01a08017 even though `checkpoint_reminder_count=2` on `external_session` —
the reminder counter and the hook_event audit log are populated by
different code paths for this host session, so hook_event-based anomaly
detection is empty for this fixture even though the session was otherwise
fully instrumented.

## The SessionEnd lease (`get_lease_facts`)

Neither `external_session` nor any other `runtime.sqlite` table records
whether a session's `SessionEnd` cleanup ran — that state lives in a
FILE, not the database: `crates/local-state/src/session_scope.rs`'s
`AuthorizedSessionScopeStore` persists one per-session activation lease at
`<HOME>/.shared-context/state/authorized-session-scopes/scope-<digest>.json`,
where `<digest>` is `locator_digest()`: sha256 of
`be_u64(len(agent_kind)) || agent_kind || be_u64(len(external_session_id)) ||
external_session_id` (length-prefixed — NOT the same digest formula as
`sctx_logs.telemetry_session_digest`, which is null-byte-separated with no
length prefix; the two are computed by different code paths for different
purposes and must not be confused). The file's own doc comment says it "is
reclaimed by age, by `SessionEnd`, or by capacity pressure" — so if it still
exists, `SessionEnd` did not clean it up (or the 30-day
`ORPHAN_LEASE_MAX_AGE` reclamation hasn't run yet either).

Verified on session 01a08017-95a0-7841-8ab9-09b582c62cb1: the lease file
`scope-bf0ec27c...json` still exists (checked two days after the session),
`issued_at_unix_seconds` matches the session's own start time, and — cross-
checked against `sctx_logs.get_logs_facts` — that session's own
session-digest-matched hook_event history ends in `turn_stop` with no
`session_end` entry at all. Together these make the human review's
"SessionEnd payload undecodable, lease not cleaned" finding mechanically
decidable from the bundle instead of only observable by a human reading the
raw session.
"""
from __future__ import annotations

import hashlib
import json
import sqlite3
import struct
from pathlib import Path
from typing import Optional


def _connect_ro(db_path: Path) -> sqlite3.Connection:
    conn = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
    conn.row_factory = sqlite3.Row
    return conn


def _table_exists(conn: sqlite3.Connection, table: str) -> bool:
    row = conn.execute(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?", (table,)
    ).fetchone()
    return row is not None


def _select_all(conn: sqlite3.Connection, table: str, where: str, params: tuple) -> list:
    if not _table_exists(conn, table):
        return None  # signals "table not found" to the caller
    cur = conn.execute(f"SELECT * FROM {table} WHERE {where}", params)  # noqa: S608
    return [dict(r) for r in cur.fetchall()]


def _in_clause(values: list) -> str:
    if not values:
        return "(NULL)"
    return "(" + ",".join("?" for _ in values) + ")"


def get_session_facts(
    db_path: Path, agent_kind: str, external_session_key: str, out_dir: Path
) -> Optional[dict]:
    """Return the sctx_db summary dict for facts.json, or None if the
    session has no `external_session` row at all (nothing to link).
    Writes `sctx/<table>.json` raw exports into out_dir as a side effect.
    """
    if not db_path.exists():
        return None
    out_dir.mkdir(parents=True, exist_ok=True)
    unlinked_tables: list = []

    conn = _connect_ro(db_path)
    try:
        ext_rows = _select_all(
            conn,
            "external_session",
            "agent_kind = ? AND external_session_key = ?",
            (agent_kind, external_session_key),
        )
        _dump(out_dir, "external_session", ext_rows or [])
        if not ext_rows:
            return {
                "linked": False,
                "reason": "no external_session row for this (agent_kind, external_session_key)",
            }
        xss_id = ext_rows[0]["external_session_id"]

        task_sessions = _select_all(
            conn, "task_session", "external_session_id = ?", (xss_id,)
        ) or []
        _dump(out_dir, "task_session", task_sessions)
        task_session_ids = [r["task_session_id"] for r in task_sessions]
        task_ids = [r["task_id"] for r in task_sessions]

        task_injection = _select_all(
            conn, "task_injection", f"task_id IN {_in_clause(task_ids)}", tuple(task_ids)
        )
        _dump(out_dir, "task_injection", task_injection or [])

        context_usage = _select_all(
            conn, "context_usage", f"task_id IN {_in_clause(task_ids)}", tuple(task_ids)
        )
        _dump(out_dir, "context_usage", context_usage or [])

        task_intent_revision = _select_all(
            conn,
            "task_intent_revision",
            f"task_session_id IN {_in_clause(task_session_ids)}",
            tuple(task_session_ids),
        )
        _dump(out_dir, "task_intent_revision", task_intent_revision or [])

        task_signal = _select_all(
            conn, "task_signal", f"task_session_id IN {_in_clause(task_session_ids)}", tuple(task_session_ids)
        )
        _dump(out_dir, "task_signal", task_signal or [])

        work_episode = _select_all(
            conn, "work_episode", f"task_session_id IN {_in_clause(task_session_ids)}", tuple(task_session_ids)
        )
        _dump(out_dir, "work_episode", work_episode or [])

        agent_checkpoint = _select_all(
            conn, "agent_checkpoint", f"task_session_id IN {_in_clause(task_session_ids)}", tuple(task_session_ids)
        )
        _dump(out_dir, "agent_checkpoint", agent_checkpoint or [])

        candidate_build = _select_all(
            conn, "candidate_build", f"task_session_id IN {_in_clause(task_session_ids)}", tuple(task_session_ids)
        )
        _dump(out_dir, "candidate_build", candidate_build or [])
        build_ids = [r["build_id"] for r in (candidate_build or [])]

        checkpoint_operation = _select_all(
            conn, "checkpoint_operation", f"task_session_id IN {_in_clause(task_session_ids)}", tuple(task_session_ids)
        )
        _dump(out_dir, "checkpoint_operation", checkpoint_operation or [])

        candidate_build_item = _select_all(
            conn, "candidate_build_item", f"build_id IN {_in_clause(build_ids)}", tuple(build_ids)
        )
        _dump(out_dir, "candidate_build_item", candidate_build_item or [])

        candidate_review = _select_all(
            conn, "candidate_review", f"task_session_id IN {_in_clause(task_session_ids)}", tuple(task_session_ids)
        )
        _dump(out_dir, "candidate_review", candidate_review or [])
        candidate_ids = [r["candidate_id"] for r in (candidate_review or [])]

        candidate_confirmation_operation = _select_all(
            conn,
            "candidate_confirmation_operation",
            f"candidate_id IN {_in_clause(candidate_ids)}",
            tuple(candidate_ids),
        )
        _dump(out_dir, "candidate_confirmation_operation", candidate_confirmation_operation or [])

        hook_event = _select_all(
            conn, "hook_event", "agent_kind = ? AND external_session_id = ?", (agent_kind, external_session_key)
        )
        _dump(out_dir, "hook_event", hook_event or [])

        auto_confirm_rejection = _select_all(
            conn, "auto_confirm_rejection", "external_session_id = ?", (external_session_key,)
        )
        _dump(out_dir, "auto_confirm_rejection", auto_confirm_rejection or [])

        for name, rows in (
            ("task_injection", task_injection),
            ("context_usage", context_usage),
            ("task_intent_revision", task_intent_revision),
            ("task_signal", task_signal),
            ("work_episode", work_episode),
            ("agent_checkpoint", agent_checkpoint),
            ("candidate_build", candidate_build),
            ("checkpoint_operation", checkpoint_operation),
            ("candidate_build_item", candidate_build_item),
            ("candidate_review", candidate_review),
            ("candidate_confirmation_operation", candidate_confirmation_operation),
            ("hook_event", hook_event),
            ("auto_confirm_rejection", auto_confirm_rejection),
        ):
            if rows is None:
                unlinked_tables.append(name)

        summary = {
            "linked": True,
            "external_session_id": xss_id,
            "task_session_count": len(task_sessions),
            "task_ids": task_ids,
            "checkpoint_reminder_count": ext_rows[0]["checkpoint_reminder_count"],
            "activity_since_checkpoint_reminder": ext_rows[0]["activity_since_checkpoint_reminder"],
            "hook_event": _summarize_hook_event(hook_event or []),
            "task_injection": _summarize_injection(task_injection or []),
            "context_usage": _summarize_context_usage(context_usage or []),
            "candidate_review": _summarize_candidate_review(candidate_review or []),
            "auto_confirm_rejection_count": len(auto_confirm_rejection or []),
            "unlinked_tables": unlinked_tables,
        }
        return summary
    finally:
        conn.close()


def _dump(out_dir: Path, table: str, rows: list) -> None:
    with open(out_dir / f"{table}.json", "w", encoding="utf-8") as f:
        json.dump(rows, f, indent=2, ensure_ascii=False, sort_keys=True)


def _summarize_hook_event(rows: list) -> dict:
    by_kind_decision_reason: dict = {}
    anomalous = []
    same_second_starts: dict = {}
    for r in rows:
        key = f"{r.get('event_kind')}|{r.get('decision')}|{r.get('reason')}"
        by_kind_decision_reason[key] = by_kind_decision_reason.get(key, 0) + 1
        if (
            r.get("decision") == "fail_open"
            or r.get("event_kind") == "undecodable"
            or r.get("reason") not in ("ok", None)
        ):
            anomalous.append(r)
        if r.get("event_kind") == "session_start":
            second = int(r.get("recorded_at_unix_ms", 0)) // 1000
            same_second_starts[second] = same_second_starts.get(second, 0) + 1
    duplicate_session_starts = {k: v for k, v in same_second_starts.items() if v > 1}
    return {
        "count": len(rows),
        "by_event_kind_decision_reason": by_kind_decision_reason,
        "anomalous_count": len(anomalous),
        "anomalous_rows": anomalous,
        "duplicate_session_start_seconds": duplicate_session_starts,
    }


def _summarize_injection(rows: list) -> dict:
    by_source: dict = {}
    for r in rows:
        src = r.get("source")
        by_source[src] = by_source.get(src, 0) + 1
    return {"count": len(rows), "by_source": by_source}


def _summarize_context_usage(rows: list) -> dict:
    by_outcome: dict = {}
    for r in rows:
        o = r.get("outcome")
        by_outcome[o] = by_outcome.get(o, 0) + 1
    return {"count": len(rows), "by_outcome": by_outcome}


def _key(value) -> str:
    """JSON object keys must be strings; coerce None (and any non-str) to a
    stable string so dicts here are always sortable/serializable."""
    return value if isinstance(value, str) else json.dumps(value)


def _summarize_candidate_review(rows: list) -> dict:
    by_status: dict = {}
    by_decision_source: dict = {}
    by_top_relation: dict = {}
    for r in rows:
        by_status[_key(r.get("status"))] = by_status.get(_key(r.get("status")), 0) + 1
        by_decision_source[_key(r.get("decision_source"))] = (
            by_decision_source.get(_key(r.get("decision_source")), 0) + 1
        )
        by_top_relation[_key(r.get("top_relation"))] = by_top_relation.get(_key(r.get("top_relation")), 0) + 1
    return {
        "count": len(rows),
        "by_status": by_status,
        "by_decision_source": by_decision_source,
        "by_top_relation": by_top_relation,
    }


def _locator_digest(agent_kind: str, external_session_id: str) -> str:
    """Reproduces `locator_digest()` in
    crates/local-state/src/session_scope.rs: sha256 of the agent_kind and
    external_session_id, each big-endian-u64-length-prefixed."""
    h = hashlib.sha256()
    agent_bytes = agent_kind.encode("utf-8")
    session_bytes = external_session_id.encode("utf-8")
    h.update(struct.pack(">Q", len(agent_bytes)))
    h.update(agent_bytes)
    h.update(struct.pack(">Q", len(session_bytes)))
    h.update(session_bytes)
    return h.hexdigest()


def get_lease_facts(home: Path, agent_kind: str, external_session_id: str, out_dir: Optional[Path] = None) -> dict:
    """Returns the `lease` facts.json block for one session: whether its
    `AuthorizedSessionScope` activation-lease file still exists under
    `<home>/.shared-context/state/authorized-session-scopes/`, and its
    contents if so. If `out_dir` is given, a copy of the lease file (when
    found) is written there as `authorized_session_scope.json` for audit.
    """
    digest = _locator_digest(agent_kind, external_session_id)
    scope_dir = home / ".shared-context" / "state" / "authorized-session-scopes"
    scope_path = scope_dir / f"scope-{digest}.json"
    exists = scope_path.is_file()
    result = {
        "locator_digest": digest,
        "lease_file_exists": exists,
        "lease_scope_dir_found": scope_dir.is_dir(),
    }
    if not exists:
        return result
    content = None
    try:
        with open(scope_path, "r", encoding="utf-8") as f:
            content = json.load(f)
    except (json.JSONDecodeError, OSError):
        result["lease_file_unreadable"] = True
        return result
    result.update(
        {
            "lease_issued_at_unix_seconds": content.get("issued_at_unix_seconds"),
            "lease_decision_kind": (content.get("decision") or {}).get("kind"),
            "lease_repository_ids": (content.get("decision") or {}).get("repository_ids"),
            "lease_startup_cwd": content.get("startup_cwd"),
            "lease_intent_bootstrap_notified": content.get("intent_bootstrap_notified"),
        }
    )
    if out_dir is not None:
        out_dir.mkdir(parents=True, exist_ok=True)
        with open(out_dir / "authorized_session_scope.json", "w", encoding="utf-8") as f:
            json.dump(content, f, indent=2, ensure_ascii=False, sort_keys=True)
    return result
