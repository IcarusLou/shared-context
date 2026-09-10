#!/usr/bin/env python3
"""Bundle a Codex or Cursor session (and, if replayed, its replay) plus sctx's
own DB rows into a compact, human- and agent-readable audit bundle.

Usage:
    python3 bundle.py --session <thread_id|conversation_id> [--replay-id <id>]
        [--agent auto|codex|cursor]
        [--audit-root ~/.shared-context-audit] [--home <HOME>]

`--agent auto` (the default) resolves the id against `~/.codex` first and
`~/.cursor` second, and reports both misses if neither host owns it. The two
hosts share the whole downstream pipeline — one `Session` model, one digest
renderer, one `facts.json` schema, one bundle layout — so `/session-review`
does not need to know which host produced a bundle. What it DOES need to know
is written into the digest header as `fidelity` notes, because a Cursor
transcript cannot show tool results or injections at all (see
`hosts/cursor.py`); an empty `### sctx calls` result on the Cursor side is a
missing record, not a failed call.

Without --replay-id, this bundles a real session that was never replayed:
writes only `original.md`, `facts.json`, `sctx/` under
`<audit-root>/orig-<short id>/`.

With --replay-id, reads `<audit-root>/<replay-id>/manifest.json` and
additionally writes `replay.md`, using the manifest's isolated HOME to
locate the replay side's `.shared-context/state/runtime.sqlite` and the
real (or --home-overridden) HOME for the original side.

Manifest field access is defensive about shape: replay.py (as of this
writing) nests the original session under `manifest["original"]` (with
`thread_id` / `rollout_path` keys) and writes `manifest["home"]` as a dict
(`{"path": ..., ...}`), not the flatter `original_thread_id` / plain-string
`home` shape this file originally assumed — both shapes are accepted.
`manifest["replayed_thread_id"]` and `manifest["replayed_rollout_path"]`
are top-level and match as expected.

See hosts/codex.py for the rollout parsing contract and sctx_facts.py for
the sqlite export contract; this module only orchestrates them and renders
the Markdown digest / facts.json.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import sctx_facts  # noqa: E402
import sctx_logs  # noqa: E402
from hosts import cursor as cursor_host  # noqa: E402
from hosts.codex import CTX_ID_RE, parse_rollout, resolve_rollout_path  # noqa: E402
from session_model import Session  # noqa: E402

DEFAULT_AUDIT_ROOT = Path("~/.shared-context-audit").expanduser()


def _fmt_int(n: int) -> str:
    return f"{n:,}"


def _fence(text: str, lang: str = "") -> str:
    # Pick a fence longer than any run of backticks already in the text so
    # arbitrary injected/tool content can never break out of the block.
    longest = 0
    run = 0
    for ch in text:
        if ch == "`":
            run += 1
            longest = max(longest, run)
        else:
            run = 0
    fence = "`" * max(3, longest + 1)
    return f"{fence}{lang}\n{text}\n{fence}"


def render_digest(session: Session, side_label: str) -> str:
    m = session.meta
    lines = []
    lines.append(f"# Session digest — {side_label}")
    lines.append("")
    lines.append(f"- host: {m.host}")
    lines.append(f"- thread_id: `{m.thread_id}`")
    lines.append(f"- cwd: `{m.cwd}`")
    lines.append(
        f"- git: commit={m.git_commit_hash or '-'} branch={m.git_branch or '-'} "
        f"repo={m.git_repository_url or '-'}"
    )
    lines.append(f"- cli_version: {m.cli_version}  originator: {m.originator}")
    lines.append(f"- started_at: {m.started_at}  ended_at: {m.ended_at}")
    lines.append(f"- compactions: {m.compaction_count}")
    lines.append(f"- turns: {len(session.turns)}")
    lines.append("")

    if m.fidelity_notes:
        # Printed before any turn so nobody reads an absence below as a
        # finding. On Cursor the absences are structural, not evidence.
        lines.append(f"## fidelity — what this host's transcript cannot show ({len(m.fidelity_notes)})")
        for note in m.fidelity_notes:
            lines.append(f"- {note}")
        lines.append("")

    if session.reconstruction:
        r = session.reconstruction
        lines.append("## reconstruction")
        lines.append(f"- source: {r.get('source')}")
        lines.append(f"- reason: {r.get('reason')}")
        lines.append(f"- runtime.sqlite: `{r.get('runtime_sqlite')}`")
        lines.append(f"- external_session_id: `{r.get('external_session_id')}`")
        lines.append(
            f"- task_injection: {r.get('task_injection_rows')} rows -> "
            f"{r.get('task_injection_events')} injection events"
        )
        if r.get("hook_diagnostics_available"):
            lines.append(
                f"- hook-diagnostics: {r.get('hook_diagnostics_events')} events matched "
                f"digest `{r.get('hook_diagnostics_session_digest')}`"
            )
        else:
            lines.append(
                f"- hook-diagnostics: {r.get('hook_diagnostics_source')} — the hook-event "
                "counts below are ABSENT, not zero"
            )
        lines.append(f"- windowing: {r.get('windowing')}")
        unwindowed = r.get("unwindowed") or {}
        lines.append(
            f"- not attributable to any turn: "
            f"{len(unwindowed.get('injection_events') or [])} injection events, "
            f"{unwindowed.get('hook_event_count')} hook events"
        )
        for event in unwindowed.get("injection_events") or []:
            lines.append(
                f"  - source={event.get('source')} at={event.get('injected_at_unix_seconds')} "
                f"ctx={len(event.get('ctx_ids') or [])}"
            )
        for event in unwindowed.get("hook_events") or []:
            lines.append(
                f"  - {event.get('operation')} outcome={event.get('outcome')} "
                f"reason={event.get('reason')} ×{event.get('count')}"
            )
        lines.append("")

    for turn in session.turns:
        stamp = f" · {turn.started_at}" if turn.started_at else ""
        lines.append(f"## Turn {turn.index}{stamp} · user ({len(turn.user_text)} chars)")
        lines.append(f"> {turn.user_text}")
        lines.append("")

        if turn.hook_injections:
            total_wire = sum(inj.wire_bytes for inj in turn.hook_injections)
            any_marker = any(inj.has_marker for inj in turn.hook_injections)
            lines.append(
                f"### hook injections ({len(turn.hook_injections)})  "
                f"marker={'yes' if any_marker else 'no'}  "
                f"wire={_fmt_int(total_wire)} B"
                "  (small marker/reminder channel — never carries the Context Pack)"
            )
            for inj in turn.hook_injections:
                lines.append(
                    f"- kind={inj.kind} marker={'yes' if inj.has_marker else 'no'}"
                    f"{' session_id=' + inj.marker_session_id if inj.marker_session_id else ''} "
                    f"wire={_fmt_int(inj.wire_bytes)} B est_tokens={inj.est_tokens}"
                )
                lines.append(_fence(inj.text, "json" if inj.text.strip().startswith("{") else ""))
            lines.append("")

        if turn.pack_deliveries:
            total_ctx = len(set(ctx for p in turn.pack_deliveries for ctx in p.ctx_ids))
            total_bytes = sum(p.bytes for p in turn.pack_deliveries)
            any_truncated = any(p.truncated for p in turn.pack_deliveries)
            reconstructed = sum(1 for p in turn.pack_deliveries if p.reconstructed_from)
            recon_note = (
                f"  reconstructed={reconstructed}/{len(turn.pack_deliveries)} (bytes/channel "
                "unmeasurable, not zero)"
                if reconstructed
                else ""
            )
            lines.append(
                f"### pack deliveries ({len(turn.pack_deliveries)})  "
                f"ctx={total_ctx}  bytes={_fmt_int(total_bytes)}  "
                f"truncated={'YES' if any_truncated else 'no'}{recon_note}"
            )
            for p in turn.pack_deliveries:
                origin = f" reconstructed_from={p.reconstructed_from}" if p.reconstructed_from else ""
                cap_str = f" cap={_fmt_int(p.channel_cap_bytes)}B" if p.channel_cap_bytes else ""
                delivered_str = (
                    f" delivered={_fmt_int(p.delivered_bytes)}B" if p.delivered_bytes is not None else ""
                )
                orig_tok_str = (
                    f" original_tokens={p.original_token_count}" if p.original_token_count is not None else ""
                )
                lines.append(
                    f"- tool={p.tool} channel={p.channel}{cap_str} items={p.items_count} "
                    f"ctx={len(p.ctx_ids)} bytes={_fmt_int(p.bytes)}{delivered_str} "
                    f"truncated={'YES' if p.truncated else 'no'}{orig_tok_str}{origin}"
                )
                lines.append(_fence(p.text, "json" if p.text.strip().startswith("{") else ""))
            lines.append("")

        if turn.reasoning_summaries:
            lines.append("### reasoning summaries")
            for s in turn.reasoning_summaries:
                lines.append(f"> {s}")
            lines.append("")

        if turn.assistant_texts:
            lines.append("### assistant")
            for a in turn.assistant_texts:
                lines.append(a)
                lines.append("")

        if turn.tool_calls:
            by_name: dict = {}
            for tc in turn.tool_calls:
                by_name[tc.name] = by_name.get(tc.name, 0) + 1
            counts_str = " ".join(f"{n}×{c}" for n, c in sorted(by_name.items(), key=lambda kv: -kv[1]))
            lines.append(f"### tools ({len(turn.tool_calls)})  {counts_str}")
            for tc in turn.tool_calls:
                lines.append(f"- {tc.name}: {tc.args_head}")
            lines.append(f"(tool outputs observed: {turn.tool_outputs_head_count}, not shown)")
            if turn.discard_wrapper_hits:
                lines.append(
                    f"(discard-wrapper pattern detected {len(turn.discard_wrapper_hits)}× "
                    "— sctx result likely never reached the model's own context)"
                )
                for hit in turn.discard_wrapper_hits:
                    one_line_snippet = " ".join(hit.snippet.split())
                    lines.append(f'  > kind={hit.kind} tool={hit.tool_name} "{one_line_snippet}"')
            lines.append("")

        if turn.sctx_calls:
            lines.append(f"### sctx calls ({len(turn.sctx_calls)})")
            for sc in turn.sctx_calls:
                claims = sc.arguments.get("claims") if isinstance(sc.arguments, dict) else None
                claims_str = f" claims={len(claims)}" if isinstance(claims, list) else ""
                err_str = f" error_code={sc.error_code}" if sc.error_code else ""
                lines.append(f"- {sc.tool} status={sc.status}{claims_str}{err_str}")
                lines.append("  arguments:")
                lines.append(_fence(json.dumps(sc.arguments, indent=2, ensure_ascii=False), "json"))
                if sc.result_text is None:
                    lines.append(
                        "  result: (not recorded by this host — the transcript "
                        "carries no tool results; check sctx/*.json instead)"
                    )
                else:
                    lines.append("  result:")
                    lines.append(_fence(sc.result_text, "json"))
            lines.append("")

        if turn.hook_events:
            total_hooks = sum(h.count for h in turn.hook_events)
            lines.append(
                f"### hook events ({total_hooks}, reconstructed from "
                "sctx hook-diagnostics — not in the host transcript)"
            )
            for h in turn.hook_events:
                lines.append(
                    f"- {h.operation} outcome={h.outcome} reason={h.reason} ×{h.count}"
                )
            lines.append("")

        u = turn.usage
        lines.append(
            f"### usage  in={_fmt_int(u.input_tokens)} cached={_fmt_int(u.cached_input_tokens)} "
            f"out={_fmt_int(u.output_tokens)}"
        )
        if turn.compaction:
            lines.append("(compaction occurred during/after this turn)")
        lines.append("")

    lines.append(f"## other injections ({len(session.other_injections)})")
    for oi in session.other_injections:
        lines.append(f"- {oi.text_head}")
    lines.append("")

    return "\n".join(lines)


def compute_facts(
    session: Session,
    sctx_db_summary,
    logs_summary=None,
    lease_summary=None,
    versions_summary=None,
) -> dict:
    m = session.meta
    all_hook_injections = [inj for t in session.turns for inj in t.hook_injections]
    all_pack_deliveries = [p for t in session.turns for p in t.pack_deliveries]
    all_sctx_calls = [sc for t in session.turns for sc in t.sctx_calls]
    all_tool_calls = [tc for t in session.turns for tc in t.tool_calls]
    all_discard_hits = [
        {"turn": t.index, "tool_name": h.tool_name, "kind": h.kind, "snippet": h.snippet}
        for t in session.turns
        for h in t.discard_wrapper_hits
    ]

    ctx_ids_injected = set()
    for p in all_pack_deliveries:
        ctx_ids_injected.update(p.ctx_ids)

    marker_ids = [inj.marker_session_id for inj in all_hook_injections if inj.marker_session_id]
    if marker_ids:
        marker_session_id_matches_thread = all(mid == m.thread_id for mid in marker_ids)
    else:
        marker_session_id_matches_thread = None

    reinjected_after_compaction = None
    if m.compaction_count > 0:
        # A turn's `compaction` flag marks the turn the compaction line
        # occurred *in* (chronologically at/near the end of that turn), so
        # "after" means strictly later turns, not the compacted turn itself.
        first_compacted_idx = next((t.index for t in session.turns if t.compaction), None)
        if first_compacted_idx is not None:
            reinjected_after_compaction = any(
                p for t in session.turns if t.index > first_compacted_idx for p in t.pack_deliveries
            )
        else:
            reinjected_after_compaction = False

    pack_delivery_rows = [
        {
            "turn": t.index,
            "tool": p.tool,
            "items_count": p.items_count,
            "ctx_ids_count": len(p.ctx_ids),
            "bytes": p.bytes,
            "channel": p.channel,
            "channel_cap_bytes": p.channel_cap_bytes,
            "delivered_bytes": p.delivered_bytes,
            "truncated": p.truncated,
            "original_token_count": p.original_token_count,
            "reconstructed_from": p.reconstructed_from,
        }
        for t in session.turns
        for p in t.pack_deliveries
    ]
    packs_by_tool: dict = {}
    channels_seen: dict = {}
    over_channel_cap_count = 0
    for p in all_pack_deliveries:
        packs_by_tool[p.tool] = packs_by_tool.get(p.tool, 0) + 1
        channels_seen[p.channel] = channels_seen.get(p.channel, 0) + 1
        exceeds_cap = (
            p.channel_cap_bytes is not None
            and p.delivered_bytes is not None
            and p.delivered_bytes >= p.channel_cap_bytes
        )
        if p.truncated or exceeds_cap:
            over_channel_cap_count += 1

    by_tool: dict = {}
    errors_by_code: dict = {}
    checkpoint_calls = 0
    checkpoint_empty_claims = 0
    claims_lens = []
    for sc in all_sctx_calls:
        by_tool[sc.tool] = by_tool.get(sc.tool, 0) + 1
        if sc.error_code:
            errors_by_code[sc.error_code] = errors_by_code.get(sc.error_code, 0) + 1
        if sc.tool == "task_checkpoint":
            checkpoint_calls += 1
            if isinstance(sc.arguments, dict) and sc.arguments.get("__arguments_unparsed__"):
                # Its claims are unknowable, so it is neither counted as empty
                # (which would read as a compliance failure) nor as carrying
                # claims. `arguments_unparsed` above is where it shows up.
                continue
            claims = sc.arguments.get("claims") if isinstance(sc.arguments, dict) else None
            n = len(claims) if isinstance(claims, list) else 0
            claims_lens.append(n)
            if n == 0:
                checkpoint_empty_claims += 1

    # A call whose recorded arguments could not be parsed (Cursor can record a
    # truncated arguments string — see hosts/cursor.py `_arguments`) says
    # nothing about whether the model passed the id verbatim, so it is counted
    # on its own instead of turning the check False for the wrong reason.
    arguments_unparsed = sum(
        1
        for sc in all_sctx_calls
        if isinstance(sc.arguments, dict) and sc.arguments.get("__arguments_unparsed__")
    )
    checkable = [
        sc
        for sc in all_sctx_calls
        if isinstance(sc.arguments, dict) and not sc.arguments.get("__arguments_unparsed__")
    ]
    external_session_id_verbatim = (
        all(sc.arguments.get("external_session_id") == m.thread_id for sc in checkable)
        if checkable
        else True
    )

    assistant_text_all = "\n".join(t for turn in session.turns for t in turn.assistant_texts)
    ctx_in_assistant = set(CTX_ID_RE.findall(assistant_text_all))
    args_text_all = "\n".join(
        json.dumps(sc.arguments, ensure_ascii=False) for sc in all_sctx_calls
    )
    ctx_in_sctx_args = set(CTX_ID_RE.findall(args_text_all))

    tools_by_name: dict = {}
    for tc in all_tool_calls:
        tools_by_name[tc.name] = tools_by_name.get(tc.name, 0) + 1

    per_turn_usage = []
    total_in = total_cached = total_out = 0
    for t in session.turns:
        u = t.usage
        per_turn_usage.append(
            {
                "turn": t.index,
                "input_tokens": u.input_tokens,
                "cached_input_tokens": u.cached_input_tokens,
                "output_tokens": u.output_tokens,
            }
        )
        total_in += u.input_tokens
        total_cached += u.cached_input_tokens
        total_out += u.output_tokens

    hook_events_by_operation: dict = {}
    for t in session.turns:
        for h in t.hook_events:
            key = f"{h.operation}|{h.outcome}|{h.reason}"
            hook_events_by_operation[key] = hook_events_by_operation.get(key, 0) + h.count

    reconstruction = session.reconstruction
    reconstruction_summary = None
    if reconstruction:
        unwindowed = reconstruction.get("unwindowed") or {}
        reconstruction_summary = {
            "source": reconstruction.get("source"),
            "external_session_id": reconstruction.get("external_session_id"),
            "task_injection_rows": reconstruction.get("task_injection_rows"),
            "task_injection_events": reconstruction.get("task_injection_events"),
            "hook_diagnostics_events": reconstruction.get("hook_diagnostics_events"),
            "hook_diagnostics_available": reconstruction.get("hook_diagnostics_available"),
            "hook_diagnostics_source": reconstruction.get("hook_diagnostics_source"),
            "windowing": reconstruction.get("windowing"),
            "unwindowed_injection_events": len(unwindowed.get("injection_events") or []),
            "unwindowed_hook_events": unwindowed.get("hook_event_count"),
        }

    return {
        "host": m.host,
        "fidelity_notes": list(m.fidelity_notes),
        "reconstruction": reconstruction_summary,
        "hook_events": {
            "count": sum(hook_events_by_operation.values()),
            "by_operation_outcome_reason": hook_events_by_operation,
        },
        "thread_id": m.thread_id,
        "turns": len(session.turns),
        "human_prompts": len(session.turns),
        "compactions": m.compaction_count,
        # Hook `additionalContext` injections ONLY — the marker/maintenance
        # channel. It never carries the Context Pack (see `pack_deliveries`
        # below); every value here is small on purpose (<=506 bytes observed
        # across every September rollout).
        "injections": {
            "count": len(all_hook_injections),
            "with_marker": sum(1 for i in all_hook_injections if i.has_marker),
            "marker_session_id_matches_thread": marker_session_id_matches_thread,
            "wire_bytes_total": sum(i.wire_bytes for i in all_hook_injections),
        },
        # The Context Pack itself, delivered as an sctx MCP tool result (see
        # hosts/codex.py's module docstring for the native_mcp vs
        # code_mode_script channel distinction). `truncated`/
        # `over_channel_cap_count` reflect that channel's own cap — NOT a
        # hook truncating, which cannot happen (hooks never carry the pack).
        "pack_deliveries": {
            "count": len(all_pack_deliveries),
            "by_tool": packs_by_tool,
            "channels_seen": channels_seen,
            "ctx_ids_total": len(ctx_ids_injected),
            "bytes_total": sum(p.bytes for p in all_pack_deliveries),
            "delivered_bytes_total": sum(
                p.delivered_bytes for p in all_pack_deliveries if p.delivered_bytes is not None
            ),
            "truncated_count": sum(1 for p in all_pack_deliveries if p.truncated),
            "over_channel_cap_count": over_channel_cap_count,
            "reconstructed_count": sum(1 for p in all_pack_deliveries if p.reconstructed_from),
            "reinjected_after_compaction": reinjected_after_compaction,
            "per_turn": pack_delivery_rows,
        },
        "sctx_calls": {
            "count": len(all_sctx_calls),
            "by_tool": by_tool,
            "errors_by_code": errors_by_code,
            "checkpoint_calls": checkpoint_calls,
            "checkpoint_empty_claims": checkpoint_empty_claims,
            "claims_total": sum(claims_lens),
            "max_claims_in_one_call": max(claims_lens) if claims_lens else 0,
            "external_session_id_verbatim": external_session_id_verbatim,
            "results_unavailable": sum(1 for sc in all_sctx_calls if sc.result_text is None),
            "arguments_unparsed": arguments_unparsed,
        },
        "pack_usage": {
            "ctx_ids_injected": len(ctx_ids_injected),
            "ctx_ids_cited_in_assistant": len(ctx_ids_injected & ctx_in_assistant),
            "ctx_ids_cited_in_sctx_calls": len(ctx_ids_injected & ctx_in_sctx_args),
        },
        "tools": {"total": len(all_tool_calls), "by_name": tools_by_name},
        "discard_wrapper": {"count": len(all_discard_hits), "hits": all_discard_hits},
        "usage": {
            "input_tokens": total_in,
            "cached_input_tokens": total_cached,
            "output_tokens": total_out,
            "per_turn": per_turn_usage,
        },
        "sctx_db": sctx_db_summary,
        "sctx_logs": logs_summary,
        "lease": lease_summary,
        "versions": versions_summary,
    }


def _short_id(thread_id: str) -> str:
    return thread_id.split("-")[0][:8]


_AGENT_VERSION_RE = re.compile(r"--agent-version\s+['\"]([^'\"]+)['\"]")


def get_versions_facts(
    cli_version: str, host_home: Path, sctx_home: Path, agent_kind: str = "codex"
) -> dict:
    """Version facts for one side's install: the host CLI version recorded in
    the transcript itself, the sctx hook's own `--agent-version` string as
    registered in `<host_home>/hooks.json`, the `bin/current` symlink target
    under `<sctx_home>/.shared-context/`, and that binary's own
    `sctx --version` output.

    `host_home` is `~/.codex` for Codex and `~/.cursor` for Cursor; both put a
    `hooks.json` there whose hook commands are the same
    `'.../sctx' hook --agent <kind> --agent-version '<version>'` string (Codex
    nests them under `hooks.PostToolUse[0].hooks[0].command`, Cursor under
    `hooks.postToolUse[0].command`), so one regex covers both. Cursor records
    no CLI version inside the transcript, so `host_cli_version` falls back to
    the `--agent-version` string, which is the CLI version sctx's installer
    stamped at hook-registration time.

    For a REAL (non-replay) bundle this reflects whatever is installed at
    BUNDLE time (when this script runs), which is not necessarily what was
    installed at SESSION time — sctx upgrades in place (see the many
    `bin/<version>/` directories next to `current`), so a session bundled
    long after it ran may show a newer version than actually ran the hooks.
    """
    hooks_agent_version = None
    hooks_json = host_home / "hooks.json"
    if hooks_json.is_file():
        try:
            raw = hooks_json.read_text(encoding="utf-8")
            m = _AGENT_VERSION_RE.search(raw)
            hooks_agent_version = m.group(1) if m else None
        except OSError:
            pass

    bin_current = sctx_home / ".shared-context" / "bin" / "current"
    bin_current_target = None
    if bin_current.is_symlink():
        try:
            bin_current_target = os.readlink(bin_current)
        except OSError:
            pass

    # `current` is a symlink to a version/arch *directory*
    # (e.g. "0.2.0-dev.9/arm64"), not the executable itself; the binary is
    # `current/sctx` inside it.
    sctx_version_output = None
    sctx_bin_sha256 = None
    sctx_bin = bin_current / "sctx"
    if sctx_bin.exists():
        try:
            proc = subprocess.run(
                [str(sctx_bin), "--version"], capture_output=True, text=True, timeout=10
            )
            sctx_version_output = (proc.stdout or proc.stderr or "").strip() or None
        except (OSError, subprocess.SubprocessError):
            pass
        # The version string cannot tell a `replay.py --sctx-bin` dev build from
        # the installed release -- both print the workspace version -- and
        # `sctx_bin_current_target` is null for a replay HOME, where
        # `bin/current` is a real directory holding a copy rather than the
        # installation's symlink. The digest is what identifies the bytes that
        # ran; compare it against `manifest.sctx_bin.sha256`.
        try:
            digest = hashlib.sha256()
            with sctx_bin.open("rb") as handle:
                for chunk in iter(lambda: handle.read(1024 * 1024), b""):
                    digest.update(chunk)
            sctx_bin_sha256 = digest.hexdigest()
        except OSError:
            pass

    return {
        "agent_kind": agent_kind,
        "host_cli_version": cli_version or hooks_agent_version,
        # Kept under its original name so an existing reader of a Codex
        # facts.json keeps working; it is the same value as host_cli_version
        # for Codex and absent for any other host.
        "codex_cli_version": cli_version if agent_kind == "codex" else None,
        "hooks_json_agent_version": hooks_agent_version,
        "sctx_bin_current_target": bin_current_target,
        "sctx_bin_sha256": sctx_bin_sha256,
        "sctx_version_output": sctx_version_output,
        "note": (
            "reflects the install at bundle time, not necessarily at session "
            "time for a real (non-replay) bundle"
        ),
    }


AGENT_CHOICES = ("auto", "codex", "cursor")


def resolve_side(
    agent: str,
    session_id: str,
    home: Path,
    explicit_path: Path | None = None,
    host_home: Path | None = None,
) -> tuple[str, Path]:
    """Pick the host that owns `session_id` and return `(agent_kind, path)`.

    With `--agent auto` the id is looked up in Codex's thread index/rollout
    tree first and Cursor's transcript tree second. The two id spaces are both
    UUID-shaped and do not collide in practice, but Codex wins a tie because
    its rollout is the richer record.
    """
    if explicit_path is not None:
        if agent == "auto":
            raise ValueError("an explicit transcript path needs an explicit --agent")
        return agent, explicit_path
    attempts: list[str] = []
    if agent in ("auto", "codex"):
        codex_home = host_home if agent == "codex" and host_home else home / ".codex"
        found = resolve_rollout_path(session_id, codex_home=codex_home)
        if found is not None:
            return "codex", found
        attempts.append(f"codex: no rollout under {codex_home}")
    if agent in ("auto", "cursor"):
        cursor_home = host_home if agent == "cursor" and host_home else home / ".cursor"
        found = cursor_host.resolve_transcript(session_id, cursor_home=cursor_home)
        if found is not None:
            return "cursor", found
        attempts.append(f"cursor: no transcript under {cursor_home}")
    raise FileNotFoundError(
        f"could not resolve session {session_id} (" + "; ".join(attempts) + ")"
    )


def load_side(agent_kind: str, session_id: str, path: Path, home: Path) -> Session:
    if agent_kind == "cursor":
        session, _ = cursor_host.load_session(session_id, home=home, transcript=path)
        return session
    return parse_rollout(path)


def build_side(
    agent_kind: str,
    session_id: str,
    path: Path,
    home: Path,
    host_home: Path,
    out_dir: Path,
    side_label: str,
) -> tuple[Session, str, dict]:
    """Parse one side and compute everything `facts.json` needs for it."""
    session = load_side(agent_kind, session_id, path, home)
    sctx_db = home / ".shared-context" / "state" / "runtime.sqlite"
    sctx_summary = sctx_facts.get_session_facts(sctx_db, agent_kind, session_id, out_dir)
    logs_summary = sctx_logs.get_logs_facts(
        home / ".shared-context-logs", agent_kind, session_id, out_dir
    )
    lease_summary = sctx_facts.get_lease_facts(home, agent_kind, session_id, out_dir)
    versions_summary = get_versions_facts(
        session.meta.cli_version, host_home, home, agent_kind=agent_kind
    )
    digest = render_digest(session, side_label)
    facts = compute_facts(session, sctx_summary, logs_summary, lease_summary, versions_summary)
    facts["transcript_path"] = str(path)
    if session.reconstruction is not None:
        with open(out_dir / "reconstruction.json", "w", encoding="utf-8") as handle:
            json.dump(session.reconstruction, handle, indent=2, ensure_ascii=False, sort_keys=True)
    return session, digest, facts


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--session", required=True, help="original thread/conversation id")
    parser.add_argument("--replay-id", default=None)
    parser.add_argument(
        "--agent",
        choices=AGENT_CHOICES,
        default="auto",
        help="host that produced the session (default: auto-detect from where the id resolves)",
    )
    parser.add_argument("--audit-root", default=str(DEFAULT_AUDIT_ROOT))
    parser.add_argument(
        "--home",
        default=None,
        help="HOME whose .shared-context to read for the original side (default: real HOME)",
    )
    args = parser.parse_args(argv)

    t0 = time.time()
    audit_root = Path(args.audit_root).expanduser()
    original_home = Path(args.home).expanduser() if args.home else Path.home()

    manifest = None
    if args.replay_id:
        bundle_dir = audit_root / args.replay_id
        manifest_path = bundle_dir / "manifest.json"
        if not manifest_path.exists():
            print(f"error: manifest not found at {manifest_path}", file=sys.stderr)
            return 2
        with open(manifest_path, "r", encoding="utf-8") as f:
            manifest = json.load(f)
    else:
        bundle_dir = audit_root / f"orig-{_short_id(args.session)}"

    bundle_dir.mkdir(parents=True, exist_ok=True)

    # replay.py's actual manifest.json nests the original session under an
    # "original" object (original.thread_id, original.rollout_path, ...)
    # rather than a flat "original_thread_id" key, and "home" is a dict
    # ({"path": ..., "snapshot_taken_at": ..., ...}) rather than a plain
    # path string. Accept both shapes defensively. replay_cursor.py writes the
    # same schema with agent="cursor" and a `cursor_home` block where the
    # Codex path writes `codex_home`.
    manifest_original = (manifest or {}).get("original") or {}
    original_thread_id = (
        (manifest or {}).get("original_thread_id")
        or manifest_original.get("thread_id")
        or args.session
    )
    agent = args.agent
    if agent == "auto" and (manifest or {}).get("agent") in ("codex", "cursor"):
        agent = manifest["agent"]
    manifest_original_rollout_path = manifest_original.get("rollout_path") or manifest_original.get(
        "transcript_path"
    )
    explicit_path = (
        Path(manifest_original_rollout_path)
        if manifest_original_rollout_path and Path(manifest_original_rollout_path).exists()
        else None
    )
    try:
        agent_kind, original_path = resolve_side(
            agent,
            original_thread_id,
            original_home,
            explicit_path=explicit_path if agent != "auto" else None,
        )
    except (FileNotFoundError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 2

    original_host_home = original_home / (".cursor" if agent_kind == "cursor" else ".codex")
    original_session, original_md, original_facts = build_side(
        agent_kind,
        original_thread_id,
        original_path,
        original_home,
        original_host_home,
        bundle_dir / "sctx" / "original",
        "original",
    )
    (bundle_dir / "original.md").write_text(original_md, encoding="utf-8")
    facts = {"agent": agent_kind, "original": original_facts}

    replayed_thread_id = (manifest or {}).get("replayed_thread_id")
    if replayed_thread_id:
        manifest_home = (manifest or {}).get("home")
        home_path = manifest_home.get("path") if isinstance(manifest_home, dict) else manifest_home
        if not home_path:
            print("error: manifest has replayed_thread_id but no usable home path", file=sys.stderr)
            return 2
        replay_home = Path(home_path).expanduser()
        replayed_rollout_path = manifest.get("replayed_rollout_path") or manifest.get(
            "replayed_transcript_path"
        )
        manifest_host_home = (manifest or {}).get("cursor_home") or (manifest or {}).get(
            "codex_home"
        )
        host_home_path = (
            manifest_host_home.get("path")
            if isinstance(manifest_host_home, dict)
            else manifest_host_home
        )
        replay_host_home = (
            Path(host_home_path).expanduser()
            if host_home_path
            else replay_home / (".cursor" if agent_kind == "cursor" else ".codex")
        )
        replay_path = (
            Path(replayed_rollout_path)
            if replayed_rollout_path and Path(replayed_rollout_path).exists()
            else None
        )
        if replay_path is None:
            try:
                _, replay_path = resolve_side(
                    agent_kind, replayed_thread_id, replay_home, host_home=replay_host_home
                )
            except (FileNotFoundError, ValueError) as error:
                print(f"warning: {error}", file=sys.stderr)
                replay_path = None
        if replay_path is None:
            print(
                f"warning: could not resolve replay transcript (tried {replayed_rollout_path})",
                file=sys.stderr,
            )
        else:
            _replay_session, replay_md, replay_facts = build_side(
                agent_kind,
                replayed_thread_id,
                Path(replay_path),
                replay_home,
                replay_host_home,
                bundle_dir / "sctx" / "replay",
                "replay",
            )
            (bundle_dir / "replay.md").write_text(replay_md, encoding="utf-8")
            facts["replay"] = replay_facts

    with open(bundle_dir / "facts.json", "w", encoding="utf-8") as f:
        json.dump(facts, f, indent=2, ensure_ascii=False, sort_keys=True)

    elapsed = time.time() - t0
    print(f"bundle written to {bundle_dir} ({elapsed:.1f}s, agent={agent_kind})")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
