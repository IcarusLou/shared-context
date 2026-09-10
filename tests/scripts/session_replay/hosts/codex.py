"""Parse a Codex CLI rollout `.jsonl` file into a `session_model.Session`.

Verified against two real rollout files on this machine (read only — never
copied into this repo):
  - ~/.codex/sessions/2026/09/08/rollout-...-01a08017-....jsonl
    (2 human turns, iOS repo, used as the S2 validation fixture)
  - ~/.codex/sessions/2026/09/04/rollout-...-01a06b3e-....jsonl
    (14-prompt session, ~64 MB, exercises the `compacted` top-level line type)

## Rollout line shape

Each line is `{"timestamp", "ordinal", "type", "payload"}`. `type` observed
in the two real files: `session_meta`, `turn_context`, `world_state`,
`response_item`, `event_msg`, `token_usage_record`, and (large file only)
`compacted`. The audit plan also lists `inter_agent_communication_metadata`
and a `response_item.payload.type == "compaction"` variant; neither was
observed in either sample file. This parser recognizes both the documented
`compaction` response_item and the observed top-level `compacted` line as
compaction markers, and silently ignores any other unrecognized `type`
values so it degrades gracefully on sessions this was not tested against.

## Human prompt / hook injection detection (verified on 01a08017)

A human prompt is a `response_item` message with `role == "user"` whose
joined text does not start with `# AGENTS.md instructions` or `<`.

A hook injection is a `response_item` message with `role == "developer"`
whose `payload.internal_chat_message_metadata_passthrough.content_item_kinds`
contains `"hooks.additional_context"` — confirmed on the real file at the
`<shared-context-active ...>` session-start banner. NOTE (schema surprise):
by this literal rule, an unrelated third-party "this session is collected by
an AI hook" notice in the same file carries the *same* content_item_kinds
marker, so it is also classified as an injection rather than folded into
other_injections, even though the plan's prose lists "third-party hook
notices" as an other_injections example. We follow the literal
content_item_kinds rule since it is the only reliable signal in the data.

## The sctx "pack" (verified on 01a08017)

sctx MCP calls are invoked by the model from inside an `exec` tool call as
`tools.mcp__shared_context__<tool>({...})` — there is no native structured
tool-call item for them. The authoritative, complete record is the
`event_msg` line with `payload.type == "item_completed"`,
`payload.item.type == "McpToolCall"`, `item.server == "shared-context"`.
`item.result.content[0].text` is the FULL JSON response text sctx computed,
independent of whatever the model's exec sandbox actually surfaced back into
its own context.

We treat any such call whose parsed JSON result contains a list-valued
`items` key as a context "pack" delivered to the model, and additionally
emit a synthetic `Injection` for it (kind `prompt_submit` if it is the
turn's first sctx call, else `post_tool_use`). This reproduces the ground
truth that turn 1 and turn 2 of 01a08017 "carried 16 and 15 context items"
(`len(result['items'])`), which is otherwise invisible if you only look at
hook-pushed developer messages (there are none carrying ctx_ ids in that
file).

Because the MCP call is wrapped in `exec`, what the model *actually saw* can
differ sharply from what sctx computed: on 01a08017 turn 1 the paired
`custom_tool_call_output` contains a host "Warning: truncated output"
banner (the pack was cut by exec's max_output_tokens), while on turn 2 the
model's own wrapper script was `.then(r=>({isError:r.isError}))`, so it
discarded the pack itself and the paired output is just `{"isError":false}`.
We pair each pack-bearing MCP call to its wrapping `custom_tool_call` /
`custom_tool_call_output` by matching, within the same turn and in order of
appearance, an exec `input` script that contains
`mcp__shared_context__<tool>(` for the Nth call to that tool. `wire_bytes`
and `truncated` are computed from that *paired, actually-delivered* text
(falling back to the full sctx-side text if no pairing is found); `ctx_ids`
and the injection `text` field use the FULL sctx-side record, since that is
the only way to recover the true pack size (16 / 15) when the model
discarded or never rendered its copy. This means `truncated=True` reflects
a genuine host-side truncation (turn 1), while a model-side self-discard
(turn 2) shows up as small `wire_bytes` with `truncated=False` — these are
different failure modes and the digest keeps them distinguishable.

`item.started` variants of the McpToolCall event were not observed in
either sample file; only `item_completed` is handled.
"""
from __future__ import annotations

import json
import re
import sqlite3
from pathlib import Path
from typing import Optional

import sys

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
from session_model import (  # noqa: E402
    DiscardWrapperHit,
    Injection,
    OtherInjection,
    SctxCall,
    Session,
    SessionMeta,
    ToolCall,
    Turn,
    Usage,
)

CTX_ID_RE = re.compile(r"ctx_[0-9a-fA-F-]+")
MARKER_SESSION_ID_RE = re.compile(r'external_session_id="([^"]+)"')
TRUNCATION_MARKERS = ("Warning: truncated output",)

# Discard-wrapper detection (verified against 01a08017 turn 2, where the
# model wrapped its own task_intent_update MCP call as
# `.then(r=>{store("mergeIntent",r);return {isError:r.isError};})` and never
# text()'d the actual pack back into its own context). Both patterns are
# deliberately open-ended (no required closing `}`/`)`) so they still match
# inside a longer multi-statement arrow body, not just a bare
# `.then(r=>({key:r.key}))` expression. `args_head` is capped at 160 chars
# for the digest, so this pattern is matched against the FULL args text
# before truncation, at the point the ToolCall is built.
DISCARD_WRAPPER_PATTERNS = (
    ("isError_wrapper", re.compile(r"\.then\(\s*\(?\w+\)?\s*=>\s*\(?\{[^}]*isError")),
    (
        "single_key_passthrough",
        re.compile(r"\.then\(\s*\(?(\w+)\)?\s*=>\s*\(?\{\s*\w+\s*:\s*\1\.\w+"),
    ),
)


def find_discard_wrapper_hits(tool_name: str, full_args_text: str) -> list:
    """Scan FULL (untruncated) tool-call args text for a `.then(...)` shape
    that looks like it discards an sctx MCP result before rendering it.
    Returns a list[DiscardWrapperHit] with ~120 chars of context per match.
    """
    hits = []
    for kind, pattern in DISCARD_WRAPPER_PATTERNS:
        for m in pattern.finditer(full_args_text):
            start = max(0, m.start() - 30)
            end = min(len(full_args_text), m.end() + 90)
            snippet = full_args_text[start:end]
            hits.append(DiscardWrapperHit(kind=kind, tool_name=tool_name, snippet=snippet))
    return hits


def _joined_text(content: list) -> str:
    return "".join(c.get("text", "") for c in (content or []) if isinstance(c, dict))


def is_human_prompt(text: str) -> bool:
    return not (text.startswith("# AGENTS.md instructions") or text.startswith("<"))


def is_hook_injection(payload: dict) -> bool:
    meta = payload.get("internal_chat_message_metadata_passthrough") or {}
    kinds = meta.get("content_item_kinds") or []
    return "hooks.additional_context" in kinds


def estimate_tokens(text: str) -> int:
    """Heuristic token estimate: chars/3 for CJK-heavy text, else chars/4.

    "CJK-heavy" = >20% of non-whitespace characters fall in the common CJK
    unicode ranges. This is a rough estimate for digest display only, not a
    tokenizer.
    """
    if not text:
        return 0
    non_ws = [c for c in text if not c.isspace()]
    if not non_ws:
        return 0
    cjk = sum(1 for c in non_ws if _is_cjk(c))
    ratio = cjk / len(non_ws)
    divisor = 3 if ratio > 0.2 else 4
    return max(1, round(len(text) / divisor))


def _is_cjk(ch: str) -> bool:
    cp = ord(ch)
    return (
        0x4E00 <= cp <= 0x9FFF
        or 0x3400 <= cp <= 0x4DBF
        or 0x3040 <= cp <= 0x30FF
        or 0xAC00 <= cp <= 0xD7A3
    )


def _looks_truncated(text: str) -> bool:
    if any(m in text for m in TRUNCATION_MARKERS):
        return True
    if "<shared-context-active" in text and "</shared-context-active>" not in text:
        return True
    if text.count("{") != text.count("}"):
        return True
    if text.count("[") != text.count("]"):
        return True
    return False


def resolve_rollout_path(thread_id: str, codex_home: Optional[Path] = None) -> Optional[Path]:
    """Resolve a Codex thread id to its rollout .jsonl path.

    Looks up `<codex_home>/state_5.sqlite` table `threads(id, rollout_path)`
    first; falls back to globbing `<codex_home>/sessions/**/*<thread_id>*.jsonl`.
    `codex_home` defaults to `~/.codex` (this is the Codex host's own state,
    independent of the sctx `--home` flag).
    """
    home = codex_home or (Path.home() / ".codex")
    db_path = home / "state_5.sqlite"
    if db_path.exists():
        try:
            conn = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
            try:
                row = conn.execute(
                    "SELECT rollout_path FROM threads WHERE id = ?", (thread_id,)
                ).fetchone()
                if row and row[0]:
                    p = Path(row[0])
                    if p.exists():
                        return p
            finally:
                conn.close()
        except sqlite3.Error:
            pass
    sessions_dir = home / "sessions"
    if sessions_dir.exists():
        matches = sorted(sessions_dir.glob(f"**/*{thread_id}*.jsonl"))
        if matches:
            return matches[0]
    return None


def _read_lines(path: Path):
    with open(path, "r", encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                yield json.loads(line)
            except json.JSONDecodeError:
                continue


class _TurnBuilder:
    """Mutable accumulation state for the turn currently being built."""

    def __init__(self, index: int, user_text: str):
        self.turn = Turn(index=index, user_text=user_text)
        self._sctx_calls_seen = 0
        self._saw_tool_since_prompt = False


def parse_rollout(path: Path) -> Session:
    session = Session()
    meta = session.meta

    pending_injections: list = []  # before first human prompt
    builder: Optional[_TurnBuilder] = None
    # SESSION-WIDE (not per-turn) occurrence counter per sctx tool name, used
    # only to pick out the Nth `mcp__shared_context__<tool>(` exec wrapper
    # for output pairing in _find_paired_exec_output, which scans the whole
    # file. Deliberately separate from _TurnBuilder._sctx_calls_seen, which
    # resets every turn and drives prompt_submit/post_tool_use classification.
    mcp_tool_global_seen: dict = {}
    # Set True the moment a compaction line is seen; used only to classify
    # the *next* hook injection's kind as "compact". It does NOT propagate
    # compaction=True to later turns — that flag marks only the turn during
    # which the compaction line itself appeared.
    just_compacted = False

    def flush_injection(inj: Injection):
        if builder is None:
            pending_injections.append(inj)
        else:
            builder.turn.injections.append(inj)

    def flush_other(head: str):
        session.other_injections.append(OtherInjection(text_head=head[:80]))

    def current_turn() -> Optional[Turn]:
        return builder.turn if builder else None

    lines = list(_read_lines(path))

    for line in lines:
        ltype = line.get("type")
        payload = line.get("payload") or {}

        if ltype == "session_meta":
            meta.thread_id = payload.get("id") or payload.get("session_id") or ""
            meta.cwd = payload.get("cwd", "")
            meta.cli_version = payload.get("cli_version", "")
            meta.originator = payload.get("originator", "")
            meta.started_at = payload.get("timestamp") or line.get("timestamp")
            git = payload.get("git") or {}
            meta.git_commit_hash = git.get("commit_hash")
            meta.git_branch = git.get("branch")
            meta.git_repository_url = git.get("repository_url")
            meta.ended_at = line.get("timestamp")
            continue

        meta.ended_at = line.get("timestamp") or meta.ended_at

        if ltype == "compacted":
            just_compacted = True
            meta.compaction_count += 1
            if builder is not None:
                builder.turn.compaction = True
            continue

        if ltype == "token_usage_record":
            usage = payload.get("usage") or {}
            if builder is not None:
                u = builder.turn.usage
                u.input_tokens = usage.get("input_tokens", u.input_tokens)
                u.cached_input_tokens = usage.get("cached_input_tokens", u.cached_input_tokens)
                u.output_tokens = usage.get("output_tokens", u.output_tokens)
                u.reasoning_output_tokens = usage.get(
                    "reasoning_output_tokens", u.reasoning_output_tokens
                )
                u.total_tokens = usage.get("total_tokens", u.total_tokens)
            continue

        if ltype == "event_msg":
            etype = payload.get("type")
            if etype == "item_completed":
                item = payload.get("item") or {}
                if item.get("type") == "McpToolCall" and item.get("server") == "shared-context":
                    tool = item.get("tool", "")
                    args = item.get("arguments") or {}
                    status = item.get("status")
                    result = item.get("result") or {}
                    content = result.get("content") or []
                    result_text = content[0].get("text", "") if content else ""
                    error_code = None
                    parsed = None
                    try:
                        parsed = json.loads(result_text) if result_text else None
                    except json.JSONDecodeError:
                        parsed = None
                    if isinstance(parsed, dict):
                        if "error_code" in parsed:
                            error_code = parsed.get("error_code")
                        elif isinstance(parsed.get("error"), dict):
                            error_code = parsed["error"].get("code")
                    call = SctxCall(
                        tool=tool,
                        arguments=args,
                        status=status,
                        result_text=result_text,
                        error_code=error_code,
                    )
                    if builder is not None:
                        builder.turn.sctx_calls.append(call)
                        occ = mcp_tool_global_seen.get(tool, 0)
                        mcp_tool_global_seen[tool] = occ + 1
                        if isinstance(parsed, dict) and isinstance(parsed.get("items"), list):
                            kind = (
                                "prompt_submit"
                                if builder._sctx_calls_seen == 0
                                else "post_tool_use"
                            )
                            builder._sctx_calls_seen += 1
                            # Use items[i].context_id directly rather than a
                            # blanket ctx_ regex over the whole result text:
                            # the pack JSON also carries "omitted" entries
                            # (considered but NOT injected) and nested
                            # evidence/condition references that also match
                            # `ctx_[hex]`, which inflates the count above the
                            # true injected-item total (verified on
                            # 01a08017: regex over the full pack text found
                            # 20/21 unique ids for turn 1/2 vs the correct
                            # 16/15 from items[i].context_id).
                            ctx_ids = sorted(
                                {
                                    it.get("context_id")
                                    for it in parsed["items"]
                                    if isinstance(it, dict) and it.get("context_id")
                                }
                            )
                            marker_match = MARKER_SESSION_ID_RE.search(result_text)
                            delivered = _find_paired_exec_output(
                                lines, tool, occ, line
                            )
                            wire_text = delivered if delivered is not None else result_text
                            inj = Injection(
                                kind=kind,
                                text=result_text,
                                has_marker="<shared-context-active" in result_text,
                                marker_session_id=marker_match.group(1) if marker_match else None,
                                ctx_ids=ctx_ids,
                                wire_bytes=len(wire_text.encode("utf-8")),
                                est_tokens=estimate_tokens(result_text),
                                truncated=_looks_truncated(wire_text),
                            )
                            builder.turn.injections.append(inj)
            continue

        if ltype != "response_item":
            continue

        ptype = payload.get("type")

        if ptype == "compaction":
            just_compacted = True
            meta.compaction_count += 1
            if builder is not None:
                builder.turn.compaction = True
            continue

        if ptype == "message":
            role = payload.get("role")
            text = _joined_text(payload.get("content"))

            if role == "user":
                if is_human_prompt(text):
                    builder = _TurnBuilder(len(session.turns) + 1, text)
                    session.turns.append(builder.turn)
                    just_compacted = False
                    for inj in pending_injections:
                        builder.turn.injections.append(inj)
                    pending_injections.clear()
                    continue
                else:
                    # Non-human user-role text (e.g. the one-time embedded
                    # AGENTS.md instructions block). Not a human prompt and
                    # not a developer-role hook/other injection either;
                    # inert preamble, intentionally not tracked anywhere.
                    continue

            if role == "assistant":
                if text:
                    if builder is not None:
                        builder.turn.assistant_texts.append(text)
                continue

            if role == "developer":
                if is_hook_injection(payload):
                    kind = _infer_injection_kind(text, builder, just_compacted)
                    just_compacted = False
                    marker_match = MARKER_SESSION_ID_RE.search(text)
                    inj = Injection(
                        kind=kind,
                        text=text,
                        has_marker="<shared-context-active" in text,
                        marker_session_id=marker_match.group(1) if marker_match else None,
                        ctx_ids=sorted(set(CTX_ID_RE.findall(text))),
                        wire_bytes=len(text.encode("utf-8")),
                        est_tokens=estimate_tokens(text),
                        truncated=_looks_truncated(text),
                    )
                    flush_injection(inj)
                else:
                    flush_other(text)
                continue
            continue

        if ptype == "reasoning":
            summaries = payload.get("summary") or []
            texts = [s.get("text", "") for s in summaries if isinstance(s, dict) and s.get("text")]
            if texts and builder is not None:
                builder.turn.reasoning_summaries.extend(texts)
            continue

        if ptype in ("function_call", "custom_tool_call"):
            name = payload.get("name", "")
            if ptype == "function_call":
                args_text = payload.get("arguments", "") or ""
                call_id = payload.get("call_id")
            else:
                args_text = payload.get("input", "") or ""
                call_id = payload.get("call_id")
            if builder is not None:
                builder.turn.tool_calls.append(
                    ToolCall(name=name, args_head=args_text[:160], call_id=call_id)
                )
                builder.turn.discard_wrapper_hits.extend(
                    find_discard_wrapper_hits(name, args_text)
                )
                builder._saw_tool_since_prompt = True
            continue

        if ptype in ("function_call_output", "custom_tool_call_output"):
            if builder is not None:
                builder.turn.tool_outputs_head_count += 1
            continue

        # Other response_item types (world_state-like inline items, etc.)
        # are intentionally ignored; they carry no auditable signal here.

    return session


def _find_paired_exec_output(
    lines: list, tool: str, occurrence_index: int, mcp_event_line: dict
) -> Optional[str]:
    """Find the text actually delivered to the model for the Nth
    `mcp__shared_context__<tool>(` exec wrapper call, by scanning
    `custom_tool_call` / `custom_tool_call_output` pairs in file order and
    matching the occurrence_index-th call whose `input` invokes that tool.

    Returns None if no such pairing is found (falls back to the full
    sctx-side text in the caller).
    """
    needle = f"mcp__shared_context__{tool}("
    seen = 0
    target_call_id = None
    for line in lines:
        if line.get("type") != "response_item":
            continue
        payload = line.get("payload") or {}
        if payload.get("type") == "custom_tool_call":
            input_text = payload.get("input", "") or ""
            if needle in input_text:
                if seen == occurrence_index:
                    target_call_id = payload.get("call_id")
                    break
                seen += 1
    if target_call_id is None:
        return None
    for line in lines:
        if line.get("type") != "response_item":
            continue
        payload = line.get("payload") or {}
        if (
            payload.get("type") == "custom_tool_call_output"
            and payload.get("call_id") == target_call_id
        ):
            chunks = payload.get("output") or []
            return "".join(c.get("text", "") for c in chunks if isinstance(c, dict))
    return None


def _infer_injection_kind(text: str, builder: Optional[_TurnBuilder], just_compacted: bool) -> str:
    if "<shared-context-active" in text:
        return "session_start"
    if just_compacted:
        return "compact"
    if builder is None:
        return "session_start"
    if not builder._saw_tool_since_prompt and not builder.turn.sctx_calls:
        return "prompt_submit"
    return "post_tool_use"
