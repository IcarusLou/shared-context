"""Rank local real coding-agent sessions as replay candidates.

## Why (project owner decision, 2026-09-10)

Replays are headless: the agent's questions get no answer. A replay
candidate must be a session where the FIRST human prompt already states the
task fully and the agent then does a long stretch of autonomous work.
Sessions that are a back-and-forth of clarification/alignment must be
avoided because they diverge in replay. The first end-to-end replay (Codex
thread 01a07bd1-5807-74b2-9b85-257b735ef58f, FE repo) failed exactly this
way: it depended on a Lark doc read via an external CLI and on user
corrections.

This script scans local real sessions (never copying transcript text into
the repo — only short heads/paths land in its output) and ranks them.

## Sources

- Codex: `~/.codex/state_5.sqlite` table
  `threads(id, rollout_path, cwd, cli_version, thread_source, git_branch,
  first_user_message)`, filtered to `thread_source='user'`. The db row is
  used ONLY to pick which rollouts to parse (id, path, a coarse `--since`
  timestamp) — every metric below is computed from the parsed
  `session_model.Session`, via `hosts/codex.py:parse_rollout`.
- Cursor: `~/.cursor/projects/*/agent-transcripts/*/*.jsonl`, via
  `hosts/cursor.py:load_session` (reconstructs injections/sctx activity from
  sctx's own state, per that module's docstring). The file's mtime is used
  as a coarse pre-parse `--since` filter only; metrics come from the Session.
  If `hosts/cursor.py` is not importable, this script falls back to Codex
  only and says so on stderr.

Read `hosts/codex.py` and `hosts/cursor.py` module docstrings for the exact
parsing rules (human-prompt detection, injection/marker detection, etc.).

## Metrics (all computed from the parsed Session — nothing else is read)

- `human_prompts`, `first_prompt_chars`, `first_prompt_has_url` (http(s):// or
  a Lark link), `first_prompt_mentions_files` (a path, a `.kt/.swift/.ts/.rs`
  filename, or a backticked identifier — a proxy for specificity),
  `first_prompt_trivial` (under 20 chars, a `/`-command, or a generic
  总结/看看/test phrase with no file mention — see `_first_prompt_is_trivial`).
- `turn1_tool_calls`, `total_tool_calls`, `turn1_tool_share` = turn1/total.
- `short_followups` = later human prompts under 40 chars (e.g. "继续",
  "确认 1、2"); `followup_short_ratio` = short_followups / (human_prompts-1).
- `agent_questions` = assistant message chunks ending in `?`/`？`, or whose
  last 200 chars contain 「请确认」「是否」「需要我」, plus tool calls whose
  name contains `request_user_input`.
- `external_deps` = tool calls whose `args_head` mentions a keyword needing
  credentials/network: lark, curl, `gh `, adb, xcodebuild, or a bare
  http(s):// / domain-like reference. `external_dep_keywords` lists which.
- `sctx_active` = `injections.with_marker > 0 and sctx_calls > 0`;
  `ctx_injected` = count of distinct ctx_ids across all injections.
- `repo_cwd`, `git_branch`, `cli_version`, `started_at`, `rollout_bytes`.
- `repo_registered` / `repo_id`: whether `repo_cwd` falls under one of the
  `paths` of a `[[repositories]]` entry in `~/.shared-context/config.toml`
  (parsed directly with `tomllib`, matched by path prefix), and that entry's
  `id` if so. An unregistered repo never receives a marker injection, so it
  is useless as a replay candidate regardless of everything else — this is
  therefore a HARD FILTER by default (owner's 2026-09-10 instruction), not
  just a score term: unregistered sessions are dropped before ranking unless
  `--include-unregistered` is passed, in which case the score's
  `REGISTERED_BONUS` still separates them from registered ones.

## Score (simple, explainable, tune the constants below if needed)

    score =
        + PROMPT_LEN_W  * min(first_prompt_chars, PROMPT_LEN_CAP) / PROMPT_LEN_CAP
        + FILES_BONUS   (if first_prompt_mentions_files)
        + T1_SHARE_W    * turn1_tool_share
        + WORK_VOLUME_W    * log1p(total_tool_calls)
        + T1_WORK_VOLUME_W * log1p(turn1_tool_calls)
        + SCTX_BONUS    (if sctx_active)
        + REGISTERED_BONUS (if repo_registered)
        - EXTRA_PROMPT_PENALTY * max(0, human_prompts - 1)
        - QUESTION_PENALTY     * agent_questions
        - SHORT_FOLLOWUP_PENALTY * short_followups
        - EXT_DEP_PENALTY      * external_deps
        - URL_PENALTY          (if first_prompt_has_url)
        - TRIVIAL_PROMPT_PENALTY (if first_prompt_trivial)

The two `log1p(tool_calls)` terms are the work-volume signal (owner's
2026-09-10 follow-up: a single-prompt session with 100% turn-1 *share* but
only 4 total tool calls was outscoring a 101-call session on share alone).
Log scale so 100 calls beats 4 decisively without a 20-call session dwarfing
everything once totals climb into the hundreds. `first_prompt_trivial` (see
`_first_prompt_is_trivial`) is true when the first prompt is under 20 chars,
starts with `/` (a slash command, not a task description), or is a generic
"总结/看看/test"-style phrase with no file/symbol/path mention — such a
prompt cannot have "stated the task fully" regardless of what followed, so
it is penalized independently of (and on top of) the length/files terms
above, which still reward a merely-short-but-specific prompt less harshly.

A long, specific, file-referencing first prompt that is followed by a large,
mostly-turn-1 volume of tool calls, few total prompts, an active sctx marker
and a registered repo scores high; agent questions, short "continue"-style
followups, external dependencies, a first prompt that leans on an external
link (Lark, etc.), and a trivial/non-self-contained first prompt all pull
the score down.

## CLI

    python3 candidates.py [--agent codex|cursor|all] [--since YYYY-MM-DD]
                       [--min-t1-tools N] [--min-total-tools N] [--top N]
                       [--json] [--include-unregistered] [--explain THREAD_ID]

`--min-total-tools` defaults to 15 (overridable, `0` disables it): a session
under that bar did not do a "long stretch" of anything, so it is dropped
before ranking rather than merely scored down, the same way an unregistered
repo is.

`--json` prints a machine-readable list of the same records the table shows
(for a replay driver to consume). `--explain THREAD_ID` resolves and parses
one session (ignoring every other filter) and prints its full raw metrics
dict, including `external_dep_keywords` and the raw tool/assistant hits used
for `agent_questions`.
"""
from __future__ import annotations

import argparse
import datetime as dt
import glob
import json
import math
import os
import re
import sqlite3
import sys
import tomllib
from pathlib import Path
from typing import Optional

sys.path.insert(0, str(Path(__file__).resolve().parent))
from hosts import codex as codex_host  # noqa: E402
from session_model import Session  # noqa: E402

try:
    from hosts import cursor as cursor_host  # noqa: E402

    HAVE_CURSOR = True
except ImportError:
    cursor_host = None  # type: ignore[assignment]
    HAVE_CURSOR = False

CODEX_HOME_DEFAULT = Path.home() / ".codex"
CURSOR_HOME_DEFAULT = Path.home() / ".cursor"

# --- score constants (see header docstring) --------------------------------
PROMPT_LEN_W = 2.0
PROMPT_LEN_CAP = 600
FILES_BONUS = 1.5
T1_SHARE_W = 3.0
WORK_VOLUME_W = 1.5
T1_WORK_VOLUME_W = 0.75
SCTX_BONUS = 1.5
REGISTERED_BONUS = 1.0
EXTRA_PROMPT_PENALTY = 0.6
QUESTION_PENALTY = 1.0
SHORT_FOLLOWUP_PENALTY = 1.0
EXT_DEP_PENALTY = 1.5
URL_PENALTY = 2.0
TRIVIAL_PROMPT_PENALTY = 3.0

# --- filter defaults ---------------------------------------------------------
DEFAULT_MIN_TOTAL_TOOLS = 15

# --- detection patterns ------------------------------------------------------
URL_OR_LARK_RE = re.compile(r"https?://|lark|feishu\.cn", re.IGNORECASE)
FILE_EXT_RE = re.compile(r"\.(kt|swift|ts|tsx|rs)\b")
PATH_RE = re.compile(r"(?:^|[\s(\"'])(?:[\w.-]+/)+[\w.-]+")
BACKTICK_RE = re.compile(r"`[^`\n]{1,80}`")
QUESTION_PHRASES = ("请确认", "是否", "需要我")
REQUEST_USER_INPUT_RE = re.compile(r"request_user_input", re.IGNORECASE)
EXTERNAL_DEP_KEYWORDS = ("lark", "curl", "gh ", "adb", "xcodebuild")
NETWORK_HOST_RE = re.compile(
    r"https?://[^\s\"']+|\b[a-zA-Z0-9-]+\.(?:com|cn|net|org|io|dev)\b", re.IGNORECASE
)
TRIVIAL_PROMPT_MIN_CHARS = 20
GENERIC_PROMPT_PHRASES = ("总结", "看看", "test")


def _first_prompt_has_url(text: str) -> bool:
    return bool(URL_OR_LARK_RE.search(text or ""))


def _first_prompt_mentions_files(text: str) -> bool:
    text = text or ""
    return bool(FILE_EXT_RE.search(text) or BACKTICK_RE.search(text) or PATH_RE.search(text))


def _first_prompt_is_trivial(text: str, mentions_files: bool) -> bool:
    """A first prompt too thin to have "stated the task fully" on its own:
    under `TRIVIAL_PROMPT_MIN_CHARS` chars, a slash command (`/statusline`,
    not a task description), or a generic phrase (总结/看看/test) carrying
    no file/symbol/path mention to anchor it."""
    stripped = (text or "").strip()
    if len(stripped) < TRIVIAL_PROMPT_MIN_CHARS:
        return True
    if stripped.startswith("/"):
        return True
    if not mentions_files and any(p in stripped.lower() for p in GENERIC_PROMPT_PHRASES):
        return True
    return False


def _external_dep_hits(args_head: str) -> list:
    hits = []
    lower = args_head or ""
    for kw in EXTERNAL_DEP_KEYWORDS:
        if kw in lower:
            hits.append(kw)
    if NETWORK_HOST_RE.search(lower):
        hits.append("network-host")
    return hits


def _is_agent_question(text: str) -> bool:
    stripped = (text or "").rstrip()
    if stripped.endswith("?") or stripped.endswith("？"):
        return True
    tail = stripped[-200:]
    return any(p in tail for p in QUESTION_PHRASES)


DEFAULT_SCTX_CONFIG = Path.home() / ".shared-context" / "config.toml"


def load_registered_repositories(config_path: Path = DEFAULT_SCTX_CONFIG) -> tuple:
    """Parse `~/.shared-context/config.toml`'s `[[repositories]]` table once
    and return `(entries, note)`.

    `entries` is a list of `(repository_id, path)` pairs — one per path in
    each entry's `paths = [...]` array, flattened so a plain prefix match
    against every entry is enough. `note` is `None` on success or a short
    error string if the config could not be read/parsed (in which case no
    repo is ever considered registered — see `--include-unregistered`).
    """
    if not config_path.exists():
        return [], f"no sctx config at {config_path}"
    try:
        with open(config_path, "rb") as f:
            data = tomllib.load(f)
    except (OSError, tomllib.TOMLDecodeError) as exc:
        return [], f"could not parse {config_path}: {exc}"
    entries = []
    for repo in data.get("repositories") or []:
        repo_id = repo.get("id")
        for p in repo.get("paths") or []:
            entries.append((repo_id, str(Path(p))))
    return entries, None


def _match_repository(cwd: str, entries: list) -> Optional[str]:
    """Return the `repository_id` of the entry whose path is a prefix of
    `cwd`, or `None` if `cwd` matches none of them."""
    if not cwd:
        return None
    try:
        norm = str(Path(cwd))
    except (OSError, ValueError):
        norm = cwd
    for repo_id, cp in entries:
        if norm == cp or norm.startswith(cp.rstrip("/") + "/"):
            return repo_id
    return None


def _short_id(thread_id: str) -> str:
    return (thread_id or "")[:8]


def compute_metrics(agent: str, thread_id: str, session: Session, rollout_bytes: Optional[int],
                     repo_entries: list) -> dict:
    turns = session.turns
    human_prompts = len(turns)
    first_text = turns[0].user_text if turns else ""

    all_tool_calls = [tc for t in turns for tc in t.tool_calls]
    turn1_tool_calls = len(turns[0].tool_calls) if turns else 0
    total_tool_calls = len(all_tool_calls)
    turn1_tool_share = (turn1_tool_calls / total_tool_calls) if total_tool_calls else 0.0

    short_followups = sum(
        1 for t in turns[1:] if len((t.user_text or "").strip()) < 40
    )
    followup_short_ratio = (
        short_followups / (human_prompts - 1) if human_prompts > 1 else 0.0
    )

    agent_questions = 0
    for t in turns:
        for txt in t.assistant_texts:
            if _is_agent_question(txt):
                agent_questions += 1
        for tc in t.tool_calls:
            if REQUEST_USER_INPUT_RE.search(tc.name or ""):
                agent_questions += 1

    ext_dep_hits = []
    for tc in all_tool_calls:
        ext_dep_hits.extend(_external_dep_hits(tc.args_head))
    external_deps = len(ext_dep_hits)

    all_injections = [inj for t in turns for inj in t.injections]
    all_sctx_calls = [sc for t in turns for sc in t.sctx_calls]
    with_marker = sum(1 for i in all_injections if i.has_marker)
    sctx_calls_count = len(all_sctx_calls)
    sctx_active = with_marker > 0 and sctx_calls_count > 0
    ctx_ids = set()
    for inj in all_injections:
        ctx_ids.update(inj.ctx_ids)

    m = session.meta
    repo_id = _match_repository(m.cwd, repo_entries)
    repo_registered = repo_id is not None
    mentions_files = _first_prompt_mentions_files(first_text)

    metrics = {
        "agent": agent,
        "thread_id": thread_id,
        "thread_id_short": _short_id(thread_id),
        "repo_cwd": m.cwd,
        "git_branch": m.git_branch,
        "cli_version": m.cli_version,
        "started_at": m.started_at,
        "rollout_bytes": rollout_bytes,
        "human_prompts": human_prompts,
        "first_prompt_chars": len(first_text or ""),
        "first_prompt_head": (first_text or "").replace("\n", " ")[:60],
        "first_prompt_has_url": _first_prompt_has_url(first_text),
        "first_prompt_mentions_files": mentions_files,
        "first_prompt_trivial": _first_prompt_is_trivial(first_text, mentions_files),
        "turn1_tool_calls": turn1_tool_calls,
        "total_tool_calls": total_tool_calls,
        "turn1_tool_share": turn1_tool_share,
        "short_followups": short_followups,
        "followup_short_ratio": followup_short_ratio,
        "agent_questions": agent_questions,
        "external_deps": external_deps,
        "external_dep_keywords": sorted(set(ext_dep_hits)),
        "sctx_active": sctx_active,
        "injections_with_marker": with_marker,
        "sctx_calls": sctx_calls_count,
        "ctx_injected": len(ctx_ids),
        "repo_registered": repo_registered,
        "repo_id": repo_id,
    }
    metrics["score"] = score_session(metrics)
    return metrics


def score_session(m: dict) -> float:
    prompt_len_component = PROMPT_LEN_W * min(m["first_prompt_chars"], PROMPT_LEN_CAP) / PROMPT_LEN_CAP
    score = prompt_len_component
    if m["first_prompt_mentions_files"]:
        score += FILES_BONUS
    score += T1_SHARE_W * m["turn1_tool_share"]
    # Work-volume signal on a log scale: a single prompt with 100% turn-1
    # share but only a handful of tool calls must not outrank a session that
    # did a long, mostly-turn-1 stretch of real work (owner's 2026-09-10
    # follow-up — share alone rewarded `test`/`/statusline`-sized sessions).
    score += WORK_VOLUME_W * math.log1p(m["total_tool_calls"])
    score += T1_WORK_VOLUME_W * math.log1p(m["turn1_tool_calls"])
    if m["sctx_active"]:
        score += SCTX_BONUS
    if m["repo_registered"]:
        score += REGISTERED_BONUS
    score -= EXTRA_PROMPT_PENALTY * max(0, m["human_prompts"] - 1)
    score -= QUESTION_PENALTY * m["agent_questions"]
    score -= SHORT_FOLLOWUP_PENALTY * m["short_followups"]
    score -= EXT_DEP_PENALTY * m["external_deps"]
    if m["first_prompt_has_url"]:
        score -= URL_PENALTY
    if m["first_prompt_trivial"]:
        score -= TRIVIAL_PROMPT_PENALTY
    return round(score, 3)


def _parse_since(since: Optional[str]) -> Optional[dt.datetime]:
    if not since:
        return None
    return dt.datetime.strptime(since, "%Y-%m-%d").replace(tzinfo=dt.timezone.utc)


def iter_codex_candidates(codex_home: Path, since: Optional[dt.datetime]):
    """Yield (thread_id, rollout_path, rollout_bytes) for thread_source='user'
    rows, pre-filtered by --since using the sqlite row's own timestamp (the
    db row is used only to pick candidates; every metric is computed later
    from the parsed Session)."""
    db_path = codex_home / "state_5.sqlite"
    if not db_path.exists():
        return
    since_ms = int(since.timestamp() * 1000) if since else None
    conn = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
    try:
        rows = conn.execute(
            "SELECT id, rollout_path, created_at_ms, created_at "
            "FROM threads WHERE thread_source = 'user'"
        ).fetchall()
    finally:
        conn.close()
    for thread_id, rollout_path, created_at_ms, created_at in rows:
        ts_ms = created_at_ms or (created_at * 1000 if created_at else None)
        if since_ms is not None and ts_ms is not None and ts_ms < since_ms:
            continue
        if not rollout_path or not Path(rollout_path).exists():
            continue
        size = Path(rollout_path).stat().st_size
        yield thread_id, Path(rollout_path), size


def iter_cursor_candidates(cursor_home: Path, since: Optional[dt.datetime]):
    """Yield (conversation_id, transcript_path, bytes) filtered by file mtime
    as a coarse pre-parse proxy for --since (the transcript itself has no
    session-level timestamp field — see hosts/cursor.py docstring)."""
    projects = cursor_home / "projects"
    if not projects.is_dir():
        return
    since_ts = since.timestamp() if since else None
    for path_str in glob.glob(str(projects / "*/agent-transcripts/*/*.jsonl")):
        path = Path(path_str)
        if since_ts is not None and path.stat().st_mtime < since_ts:
            continue
        conversation_id = path.stem
        yield conversation_id, path, path.stat().st_size


def evaluate_codex(thread_id: str, path: Path, size: int, repo_entries: list) -> Optional[dict]:
    try:
        session = codex_host.parse_rollout(path)
    except (OSError, json.JSONDecodeError):
        return None
    if not session.turns:
        return None
    return compute_metrics("codex", thread_id, session, size, repo_entries)


def evaluate_cursor(conversation_id: str, path: Path, size: int, repo_entries: list,
                     home: Path) -> Optional[dict]:
    if not HAVE_CURSOR:
        return None
    session, _ = cursor_host.load_session(conversation_id, home=home, transcript=path)
    if not session.turns:
        return None
    return compute_metrics("cursor", conversation_id, session, size, repo_entries)


def gather(agent_filter: str, since: Optional[dt.datetime], repo_entries: list) -> list:
    records = []
    if agent_filter in ("codex", "all"):
        for thread_id, path, size in iter_codex_candidates(CODEX_HOME_DEFAULT, since):
            rec = evaluate_codex(thread_id, path, size, repo_entries)
            if rec:
                records.append(rec)
    if agent_filter in ("cursor", "all"):
        if not HAVE_CURSOR:
            print(
                "note: hosts/cursor.py not importable — scanning Codex sessions only",
                file=sys.stderr,
            )
        else:
            for conversation_id, path, size in iter_cursor_candidates(CURSOR_HOME_DEFAULT, since):
                rec = evaluate_cursor(conversation_id, path, size, repo_entries, Path.home())
                if rec:
                    records.append(rec)
    return records


TABLE_COLUMNS = (
    "rank", "agent", "thread_id", "started", "repo", "prompts",
    "t1/total", "questions", "short_fu", "ext_deps", "marker", "score", "first_prompt",
)


def print_table(records: list) -> None:
    rows = []
    for i, r in enumerate(records, 1):
        started = (r["started_at"] or "")[:10]
        repo = r["repo_id"] or os.path.basename((r["repo_cwd"] or "").rstrip("/")) or "?"
        if not r["repo_registered"]:
            repo += " (unreg)"
        rows.append((
            str(i),
            r["agent"],
            r["thread_id_short"],
            started,
            repo,
            str(r["human_prompts"]),
            f"{r['turn1_tool_calls']}/{r['total_tool_calls']}",
            str(r["agent_questions"]),
            str(r["short_followups"]),
            str(r["external_deps"]),
            "yes" if r["sctx_active"] else "no",
            f"{r['score']:.2f}",
            r["first_prompt_head"],
        ))
    widths = [max(len(TABLE_COLUMNS[i]), *(len(row[i]) for row in rows)) if rows else len(TABLE_COLUMNS[i])
              for i in range(len(TABLE_COLUMNS))]
    def fmt(cols):
        return "  ".join(c.ljust(w) for c, w in zip(cols, widths))
    print(fmt(TABLE_COLUMNS))
    print(fmt(["-" * w for w in widths]))
    for row in rows:
        print(fmt(row))


def resolve_one(thread_id: str) -> Optional[dict]:
    repo_entries, _note = load_registered_repositories()
    path = codex_host.resolve_rollout_path(thread_id, codex_home=CODEX_HOME_DEFAULT)
    if path is not None:
        return evaluate_codex(thread_id, path, path.stat().st_size, repo_entries)
    if HAVE_CURSOR:
        path = cursor_host.resolve_transcript(thread_id, cursor_home=CURSOR_HOME_DEFAULT)
        if path is not None:
            return evaluate_cursor(thread_id, path, path.stat().st_size, repo_entries, Path.home())
    return None


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--agent", choices=["codex", "cursor", "all"], default="all")
    parser.add_argument("--since", default=None, help="YYYY-MM-DD, filters by session start")
    parser.add_argument("--min-t1-tools", type=int, default=0, dest="min_t1_tools")
    parser.add_argument(
        "--min-total-tools",
        type=int,
        default=DEFAULT_MIN_TOTAL_TOOLS,
        dest="min_total_tools",
        help=f"drop sessions with fewer than N total tool calls (default "
        f"{DEFAULT_MIN_TOTAL_TOOLS}; a session under this bar did not do a "
        f"'long stretch' of anything — pass 0 to disable)",
    )
    parser.add_argument("--top", type=int, default=20)
    parser.add_argument("--json", action="store_true")
    parser.add_argument(
        "--include-unregistered",
        action="store_true",
        help="do not drop sessions whose repo is unregistered in sctx (they still "
        "lose the score's REGISTERED_BONUS); by default this is a hard filter",
    )
    parser.add_argument("--explain", default=None, metavar="THREAD_ID")
    args = parser.parse_args(argv)

    if args.explain:
        rec = resolve_one(args.explain)
        if rec is None:
            print(f"error: could not resolve/parse session {args.explain}", file=sys.stderr)
            return 2
        print(json.dumps(rec, indent=2, ensure_ascii=False))
        return 0

    repo_entries, note = load_registered_repositories()
    if note:
        print(f"note: {note} (no repo will be treated as registered)", file=sys.stderr)

    since = _parse_since(args.since)
    records = gather(args.agent, since, repo_entries)
    scanned = len(records)
    with_marker = sum(1 for r in records if r["sctx_active"])
    registered = sum(1 for r in records if r["repo_registered"])

    # HARD FILTER by default (owner's 2026-09-10 instruction): a session
    # whose repo is not registered in sctx never gets a marker injection and
    # is useless as a replay candidate, so it is dropped before ranking
    # rather than merely scored down. `--include-unregistered` opts back in.
    dropped_unregistered = 0
    if not args.include_unregistered:
        before = len(records)
        records = [r for r in records if r["repo_registered"]]
        dropped_unregistered = before - len(records)

    before_tool_filters = len(records)
    records = [r for r in records if r["turn1_tool_calls"] >= args.min_t1_tools]
    records = [r for r in records if r["total_tool_calls"] >= args.min_total_tools]
    dropped_by_tool_filters = before_tool_filters - len(records)
    records.sort(key=lambda r: r["score"], reverse=True)
    top = records[: args.top]

    if args.json:
        print(json.dumps({
            "scanned": scanned,
            "with_marker": with_marker,
            "registered_repo": registered,
            "dropped_unregistered": dropped_unregistered,
            "dropped_by_tool_filters": dropped_by_tool_filters,
            "results": top,
        }, indent=2, ensure_ascii=False))
    else:
        print(
            f"scanned={scanned} sctx_active={with_marker} registered_repo={registered} "
            f"dropped_unregistered={dropped_unregistered} "
            f"dropped_by_tool_filters={dropped_by_tool_filters} "
            f"(min_t1_tools={args.min_t1_tools} min_total_tools={args.min_total_tools}) "
            f"(showing top {len(top)} of {len(records)})",
            file=sys.stderr,
        )
        print_table(top)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
