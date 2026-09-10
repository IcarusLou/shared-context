"""Host-agnostic data model for one parsed coding-agent session.

A "turn" is one human prompt and everything the host recorded until the next
human prompt (or end of session). Host-specific parsers (see hosts/codex.py)
build a `Session` from their own log format; `bundle.py` renders it to
Markdown and derives `facts.json` without caring which host produced it.

All dataclasses are plain and JSON-friendly (`dataclasses.asdict` works on
every one of them) so callers can dump intermediate state for debugging
without writing custom serializers.
"""
from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Optional


@dataclass
class ToolCall:
    name: str
    args_head: str  # first 160 chars of the call's argument text
    call_id: Optional[str] = None


@dataclass
class SctxCall:
    tool: str
    arguments: dict = field(default_factory=dict)  # FULL, never truncated
    status: Optional[str] = None
    # FULL, never truncated. `None` means "the host's log does not record tool
    # results at all" (Cursor) — semantically different from `""`, which means
    # the host recorded an empty result. Renderers must handle both.
    result_text: Optional[str] = ""
    error_code: Optional[str] = None


@dataclass
class Injection:
    kind: str  # session_start | prompt_submit | post_tool_use | compact | unknown
    text: str  # FULL, never truncated
    has_marker: bool = False
    marker_session_id: Optional[str] = None
    ctx_ids: list = field(default_factory=list)  # unique ctx_ ids, sorted
    wire_bytes: int = 0
    est_tokens: int = 0
    truncated: bool = False
    # None: read straight out of the host transcript (Codex). Otherwise a short
    # tag naming the out-of-band source this Injection was rebuilt from because
    # the host transcript does not record injections at all (Cursor), e.g.
    # "sctx:task_injection". A reconstructed Injection knows WHICH context ids
    # were injected and WHEN, but not the injected text, so `text` carries a
    # synthetic one-line summary and `wire_bytes`/`est_tokens` stay 0.
    reconstructed_from: Optional[str] = None


@dataclass
class OtherInjection:
    text_head: str  # first 80 chars


@dataclass
class DiscardWrapperHit:
    """A `.then(...)` shape inside a FULL exec/tool-call args text that looks
    like it discards an sctx MCP result before the model ever renders it
    (e.g. `.then(r=>({isError:r.isError}))`). Detected against the full args
    text, not the 160-char `args_head` truncation (see hosts/codex.py).
    """

    kind: str  # isError_wrapper | single_key_passthrough
    tool_name: str
    snippet: str  # ~120 chars of context around the match, not truncated further


@dataclass
class Usage:
    input_tokens: int = 0
    cached_input_tokens: int = 0
    output_tokens: int = 0
    reasoning_output_tokens: int = 0
    total_tokens: int = 0


@dataclass
class HookEvent:
    """One aggregated group of sctx hook-diagnostics events attributed to a
    turn. Only populated for hosts whose transcripts hide hook activity
    (Cursor); always reconstructed from sctx's own telemetry, never from the
    host log."""

    operation: str
    outcome: str
    reason: Optional[str] = None
    count: int = 1
    first_at_unix_ms: Optional[int] = None
    last_at_unix_ms: Optional[int] = None


@dataclass
class Turn:
    index: int
    user_text: str = ""
    assistant_texts: list = field(default_factory=list)
    reasoning_summaries: list = field(default_factory=list)
    tool_calls: list = field(default_factory=list)  # list[ToolCall]
    tool_outputs_head_count: int = 0
    sctx_calls: list = field(default_factory=list)  # list[SctxCall]
    injections: list = field(default_factory=list)  # list[Injection]
    discard_wrapper_hits: list = field(default_factory=list)  # list[DiscardWrapperHit]
    compaction: bool = False
    usage: Usage = field(default_factory=Usage)
    # ISO-8601 start of this turn, when the host records one (Cursor stamps
    # every user message; Codex rollouts do not stamp per-turn boundaries).
    started_at: Optional[str] = None
    hook_events: list = field(default_factory=list)  # list[HookEvent]


@dataclass
class SessionMeta:
    host: str = "codex"  # codex | cursor
    thread_id: str = ""
    cwd: str = ""
    git_commit_hash: Optional[str] = None
    git_branch: Optional[str] = None
    git_repository_url: Optional[str] = None
    cli_version: str = ""
    originator: str = ""
    started_at: Optional[str] = None
    ended_at: Optional[str] = None
    compaction_count: int = 0
    # Human-readable statements of what this host's transcript CANNOT show, so
    # a reader of the digest never mistakes an absence for a negative finding.
    fidelity_notes: list = field(default_factory=list)


@dataclass
class Session:
    meta: SessionMeta = field(default_factory=SessionMeta)
    turns: list = field(default_factory=list)  # list[Turn]
    other_injections: list = field(default_factory=list)  # list[OtherInjection]
    # Set by hosts whose injections/hook activity had to be rebuilt from sctx's
    # own state rather than read from the transcript. Carries the sources used,
    # the per-turn time windows, and any rows that fell outside every window.
    reconstruction: Optional[dict] = None
