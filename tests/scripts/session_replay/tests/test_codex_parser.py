"""Unit tests for hosts/codex.py against a small SYNTHETIC rollout built
in-line in this file. No real transcript text is used anywhere here.

Run with:
    python3 -m unittest discover -s tests/scripts/session_replay/tests
"""
from __future__ import annotations

import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
from hosts.codex import estimate_tokens, find_discard_wrapper_hits, parse_rollout  # noqa: E402


def _line(ordinal: int, ltype: str, payload: dict, ts: str = "2026-01-01T00:00:00.000Z") -> str:
    return json.dumps({"timestamp": ts, "ordinal": ordinal, "type": ltype, "payload": payload})


def build_synthetic_rollout() -> list:
    """One synthetic session: 2 human turns, one compaction between them,
    covering every payload type the parser handles."""
    lines = []

    lines.append(
        _line(
            0,
            "session_meta",
            {
                "id": "syn-thread-0001",
                "cwd": "/tmp/example-repo",
                "cli_version": "0.0.0-test",
                "originator": "codex-tui",
                "git": {
                    "commit_hash": "deadbeef",
                    "branch": "test-branch",
                    "repository_url": "git@example.com:example/repo.git",
                },
            },
        )
    )

    # AGENTS.md instructions (excluded from human-prompt detection).
    lines.append(
        _line(
            1,
            "response_item",
            {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "# AGENTS.md instructions\nsynthetic repo rules"}],
            },
        )
    )

    # A skills/other-injection developer message (no hooks.additional_context marker).
    lines.append(
        _line(
            2,
            "response_item",
            {
                "type": "message",
                "role": "developer",
                "content": [{"type": "input_text", "text": "<skills_instructions>synthetic skills list</skills_instructions>"}],
                "internal_chat_message_metadata_passthrough": {"content_item_kinds": ["host_skills.instructions"]},
            },
        )
    )

    # The sctx session-start hook injection (marker banner).
    lines.append(
        _line(
            3,
            "response_item",
            {
                "type": "message",
                "role": "developer",
                "content": [
                    {
                        "type": "input_text",
                        "text": '<shared-context-active external_session_id="syn-thread-0001">Shared Context is authorized.</shared-context-active>',
                    }
                ],
                "internal_chat_message_metadata_passthrough": {"content_item_kinds": ["hooks.additional_context"]},
            },
        )
    )

    # Turn 1 human prompt.
    lines.append(
        _line(
            4,
            "response_item",
            {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "synthetic prompt one"}],
            },
        )
    )

    # An exec tool call wrapping a sctx MCP call (turn 1).
    lines.append(
        _line(
            5,
            "response_item",
            {
                "type": "custom_tool_call",
                "call_id": "call_1",
                "name": "exec",
                "input": 'text(await tools.mcp__shared_context__task_intent_update({goal:"synthetic"}));',
            },
        )
    )

    # The authoritative sctx MCP call record: a pack with 2 context items.
    lines.append(
        _line(
            6,
            "event_msg",
            {
                "type": "item_completed",
                "item": {
                    "type": "McpToolCall",
                    "server": "shared-context",
                    "tool": "task_intent_update",
                    "status": "ok",
                    "arguments": {"agent_kind": "codex", "external_session_id": "syn-thread-0001"},
                    "result": {
                        "content": [
                            {
                                "text": json.dumps(
                                    {
                                        "items": [
                                            {"context_id": "ctx_0000000a-1111-2222-3333-444444444444"},
                                            {"context_id": "ctx_0000000b-1111-2222-3333-444444444444"},
                                        ]
                                    }
                                )
                            }
                        ]
                    },
                },
            },
        )
    )

    # The exec output actually delivered to the model — simulate a host
    # truncation banner so `truncated` should come back True.
    lines.append(
        _line(
            7,
            "response_item",
            {
                "type": "custom_tool_call_output",
                "call_id": "call_1",
                "output": [{"type": "input_text", "text": "Warning: truncated output (original token count: 999)\n{...}"}],
            },
        )
    )

    # Reasoning item.
    lines.append(
        _line(
            8,
            "response_item",
            {"type": "reasoning", "summary": [{"type": "summary_text", "text": "synthetic reasoning"}]},
        )
    )

    # Assistant reply for turn 1.
    lines.append(
        _line(
            9,
            "response_item",
            {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "synthetic answer citing ctx_0000000a-1111-2222-3333-444444444444"}],
            },
        )
    )

    # token_usage_record for turn 1.
    lines.append(
        _line(
            10,
            "token_usage_record",
            {"usage": {"input_tokens": 100, "cached_input_tokens": 10, "output_tokens": 20, "reasoning_output_tokens": 0, "total_tokens": 120}},
        )
    )

    # Compaction marker (top-level `compacted` line, as observed in the real
    # 14-prompt fixture).
    lines.append(_line(11, "compacted", {"message": ""}))

    # Turn 2 human prompt.
    lines.append(
        _line(
            12,
            "response_item",
            {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "synthetic prompt two"}],
            },
        )
    )

    # A plain function_call/function_call_output pair for turn 2.
    lines.append(
        _line(
            13,
            "response_item",
            {"type": "function_call", "name": "apply_patch", "arguments": "*** synthetic patch ***", "call_id": "call_2"},
        )
    )
    lines.append(
        _line(
            14,
            "response_item",
            {"type": "function_call_output", "call_id": "call_2", "output": "ok"},
        )
    )

    # A sctx checkpoint call with an error code, no pack (turn 2).
    lines.append(
        _line(
            15,
            "event_msg",
            {
                "type": "item_completed",
                "item": {
                    "type": "McpToolCall",
                    "server": "shared-context",
                    "tool": "task_checkpoint",
                    "status": "error",
                    "arguments": {"agent_kind": "codex", "external_session_id": "syn-thread-0001", "claims": []},
                    "result": {"content": [{"text": json.dumps({"error_code": "SYNTHETIC_ERROR"})}]},
                },
            },
        )
    )

    lines.append(
        _line(
            16,
            "token_usage_record",
            {"usage": {"input_tokens": 50, "cached_input_tokens": 5, "output_tokens": 10, "reasoning_output_tokens": 0, "total_tokens": 60}},
        )
    )

    return lines


class TestCodexParser(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.TemporaryDirectory()
        self.path = Path(self.tmpdir.name) / "synthetic-rollout.jsonl"
        self.path.write_text("\n".join(build_synthetic_rollout()) + "\n", encoding="utf-8")
        self.session = parse_rollout(self.path)

    def tearDown(self):
        self.tmpdir.cleanup()

    def test_meta(self):
        m = self.session.meta
        self.assertEqual(m.thread_id, "syn-thread-0001")
        self.assertEqual(m.cwd, "/tmp/example-repo")
        self.assertEqual(m.git_commit_hash, "deadbeef")
        self.assertEqual(m.git_branch, "test-branch")
        self.assertEqual(m.compaction_count, 1)

    def test_turn_count_and_human_prompt_exclusion(self):
        self.assertEqual(len(self.session.turns), 2)
        self.assertEqual(self.session.turns[0].user_text, "synthetic prompt one")
        self.assertEqual(self.session.turns[1].user_text, "synthetic prompt two")

    def test_session_start_injection_attached_to_turn_one(self):
        turn1 = self.session.turns[0]
        session_start = [i for i in turn1.injections if i.kind == "session_start"]
        self.assertEqual(len(session_start), 1)
        self.assertTrue(session_start[0].has_marker)
        self.assertEqual(session_start[0].marker_session_id, "syn-thread-0001")

    def test_other_injection_captured(self):
        self.assertEqual(len(self.session.other_injections), 1)
        self.assertIn("synthetic skills list", self.session.other_injections[0].text_head)

    def test_pack_injection_ctx_ids_and_truncation(self):
        turn1 = self.session.turns[0]
        pack = [i for i in turn1.injections if i.ctx_ids]
        self.assertEqual(len(pack), 1)
        self.assertEqual(sorted(pack[0].ctx_ids), ["ctx_0000000a-1111-2222-3333-444444444444", "ctx_0000000b-1111-2222-3333-444444444444"])
        self.assertTrue(pack[0].truncated)  # paired output carried the truncation banner
        self.assertEqual(pack[0].kind, "prompt_submit")

    def test_sctx_calls_full_arguments_and_error_code(self):
        turn1 = self.session.turns[0]
        self.assertEqual(len(turn1.sctx_calls), 1)
        self.assertEqual(turn1.sctx_calls[0].tool, "task_intent_update")
        self.assertIsNone(turn1.sctx_calls[0].error_code)

        turn2 = self.session.turns[1]
        self.assertEqual(len(turn2.sctx_calls), 1)
        self.assertEqual(turn2.sctx_calls[0].tool, "task_checkpoint")
        self.assertEqual(turn2.sctx_calls[0].error_code, "SYNTHETIC_ERROR")

    def test_tool_calls_and_reasoning_and_assistant(self):
        turn1 = self.session.turns[0]
        self.assertEqual(len(turn1.tool_calls), 1)
        self.assertEqual(turn1.tool_calls[0].name, "exec")
        self.assertEqual(turn1.reasoning_summaries, ["synthetic reasoning"])
        self.assertEqual(turn1.assistant_texts, ["synthetic answer citing ctx_0000000a-1111-2222-3333-444444444444"])

        # Turn 1's custom_tool_call_output (the exec wrapper's output) is
        # counted here too.
        self.assertEqual(turn1.tool_outputs_head_count, 1)

        turn2 = self.session.turns[1]
        self.assertEqual(len(turn2.tool_calls), 1)
        self.assertEqual(turn2.tool_calls[0].name, "apply_patch")
        self.assertEqual(turn2.tool_outputs_head_count, 1)  # function_call_output, counted not stored

    def test_usage_per_turn(self):
        turn1 = self.session.turns[0]
        self.assertEqual(turn1.usage.input_tokens, 100)
        self.assertEqual(turn1.usage.cached_input_tokens, 10)
        turn2 = self.session.turns[1]
        self.assertEqual(turn2.usage.input_tokens, 50)

    def test_compaction_marks_the_turn_it_occurred_in(self):
        # The `compacted` line arrives while turn 1 is still the active
        # turn (before turn 2's human prompt), so turn 1 carries the
        # compaction flag; it does not propagate forward to turn 2.
        self.assertTrue(self.session.turns[0].compaction)
        self.assertFalse(self.session.turns[1].compaction)


class TestDiscardWrapperDetection(unittest.TestCase):
    def test_multi_statement_isError_wrapper_is_detected(self):
        # The real shape found on 01a08017 turn 2.
        text = (
            'text(await tools.mcp__shared_context__task_intent_update({...})'
            '.then(r=>{store("mergeIntent",r);return {isError:r.isError};}));'
        )
        hits = find_discard_wrapper_hits("exec", text)
        kinds = {h.kind for h in hits}
        self.assertIn("isError_wrapper", kinds)
        for h in hits:
            self.assertIn("isError", h.snippet)

    def test_single_key_passthrough_is_detected(self):
        text = 'text(await tools.mcp__shared_context__task_checkpoint({}).then(r=>({ok:r.ok})));'
        hits = find_discard_wrapper_hits("exec", text)
        kinds = {h.kind for h in hits}
        self.assertIn("single_key_passthrough", kinds)

    def test_normal_then_usage_is_not_flagged(self):
        text = 'text(await tools.mcp__shared_context__task_checkpoint({}).then(r=>r.items.length));'
        hits = find_discard_wrapper_hits("exec", text)
        self.assertEqual(hits, [])


class TestEstimateTokens(unittest.TestCase):
    def test_empty(self):
        self.assertEqual(estimate_tokens(""), 0)

    def test_ascii_uses_chars_over_four(self):
        text = "a" * 40
        self.assertEqual(estimate_tokens(text), 10)

    def test_cjk_heavy_uses_chars_over_three(self):
        text = "分支" * 20  # 40 CJK chars, well over the 20% threshold
        self.assertEqual(estimate_tokens(text), round(len(text) / 3))


if __name__ == "__main__":
    unittest.main()
