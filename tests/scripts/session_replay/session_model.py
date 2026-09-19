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
class HookInjection:
    """A hook-pushed `hooks.additional_context` developer message: the
    marker banner, maintenance reminders, and similar small text sctx's hook
    writes back into the model's own turn.

    This channel does NOT carry the Context Pack. Investigation across every
    September 2026 rollout established every sctx hook output is <=506
    bytes (`PromptSubmit` itself emits `{}` plus, at most, an optional
    `systemMessage`) — see `hosts/codex.py`'s module docstring and
    `PackDelivery` below for where the pack actually travels.
    """

    kind: str  # session_start | prompt_submit | post_tool_use | compact | unknown
    text: str  # FULL, never truncated
    has_marker: bool = False
    marker_session_id: Optional[str] = None
    wire_bytes: int = 0
    est_tokens: int = 0


@dataclass
class PackDelivery:
    """One Context Pack delivered as the RESULT of an sctx MCP tool call
    (`task_intent_update` / `task_context` / ...) — not over the hook
    `additionalContext` channel (see `HookInjection`). See
    `hosts/codex.py`'s module docstring for the wire-path evidence and the
    `native_mcp` vs `code_mode_script` channel distinction, and its meaning
    for `truncated`/`channel_cap_bytes`/`delivered_bytes`.
    """

    tool: str
    items_count: int = 0
    ctx_ids: list = field(default_factory=list)  # unique ctx_ ids, sorted
    # Compact-JSON byte size of the FULL/authoritative pack sctx computed
    # (json.dumps with no whitespace) — the pack size sctx intended to send,
    # independent of how much of it actually crossed the channel.
    bytes: int = 0
    channel: str = "unknown"  # native_mcp | code_mode_script | unknown
    # code_mode_script's ~10,000-token (~40,000-byte) script-output cap, when
    # the channel is known to have one; None when no cap is known to apply.
    channel_cap_bytes: Optional[int] = None
    # Bytes actually observed crossing the channel (e.g. the paired
    # `custom_tool_call_output` text), when that evidence exists; None when
    # unmeasurable (no pairing found, or the channel is `unknown`/Cursor).
    delivered_bytes: Optional[int] = None
    # Parsed specifically from a channel-level truncation marker (e.g.
    # Codex's "Warning: truncated output (original token count: N)" on
    # `custom_tool_call_output`) — NOT a heuristic over the pack JSON itself,
    # and NOT the same thing as a hook truncating (hooks never carry the pack
    # at all, see HookInjection).
    truncated: bool = False
    # The "original token count: N" (or equivalent) the truncation marker
    # itself reports, when `truncated` is True and the marker states it.
    original_token_count: Optional[int] = None
    text: str = ""  # FULL text for the digest: the pack JSON (Codex) or a synthetic summary (Cursor)
    # None: read straight out of the host transcript/MCP record (Codex).
    # Otherwise a short tag naming the out-of-band source this PackDelivery
    # was rebuilt from because the host transcript does not record MCP
    # results at all (Cursor), e.g. "sctx:task_injection". A reconstructed
    # PackDelivery knows WHICH context ids were injected and WHEN, but not
    # the delivered bytes/channel, so those stay 0/None/"unknown", not a
    # guess.
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
    hook_injections: list = field(default_factory=list)  # list[HookInjection]
    pack_deliveries: list = field(default_factory=list)  # list[PackDelivery]
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
