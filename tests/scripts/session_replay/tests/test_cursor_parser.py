"""Unit tests for hosts/cursor.py against a SYNTHETIC Cursor transcript built
in-line in this file, plus a synthetic sctx `runtime.sqlite` and
`hook-diagnostics.json` for the reconstruction path. No real transcript text,
no real context ids and no real database is touched.

Run with:
    python3 -m unittest discover -s tests/scripts/session_replay/tests
"""
from __future__ import annotations

import json
import sqlite3
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
from hosts.cursor import (  # noqa: E402
    is_human_prompt,
    load_session,
    parse_cursor_timestamp,
    parse_transcript,
    reconstruct_from_sctx,
    resolve_transcript,
    slugify_cwd,
    split_user_message,
)

CONVERSATION = "syn0cur1-0000-4000-8000-000000000001"

# Synthetic epoch seconds. T1 is stamped 09:00, T2 09:10, so the window
# boundary sits at 09:10 exactly.
T1_STAMP = "<timestamp>Monday, Mar 2, 2026, 9:00 AM (UTC+8)</timestamp>"
T2_STAMP = "<timestamp>Monday, Mar 2, 2026, 9:10 AM (UTC+8)</timestamp>"
T1_EPOCH = 1772413200  # 2026-03-02 09:00 +08:00
T2_EPOCH = 1772413800  # 2026-03-02 09:10 +08:00


def _user(text: str) -> str:
    return json.dumps({"role": "user", "message": {"content": [{"type": "text", "text": text}]}})


def _assistant(blocks: list) -> str:
    return json.dumps({"role": "assistant", "message": {"content": blocks}})


def _tool(name: str, payload: dict) -> dict:
    return {"type": "tool_use", "name": name, "input": payload}


def _sctx(tool: str, arguments, namespace: str = "user-shared-context") -> dict:
    return _tool(
        "CallDynamicTool",
        {"namespace": namespace, "toolName": tool, "arguments": arguments},
    )


def build_synthetic_transcript() -> list:
    """Two human turns plus every host-talking shape the parser must skip."""
    lines = []

    # Turn 1: a plain timestamped human prompt.
    lines.append(_user(f"{T1_STAMP}\n<user_query>\nsynthetic prompt one\n</user_query>"))
    lines.append(
        _assistant(
            [
                {"type": "text", "text": "synthetic assistant reply one"},
                _tool("Read", {"path": "/tmp/synthetic/example.txt"}),
                _sctx(
                    "task_intent_update",
                    {
                        "agent_kind": "cursor",
                        "external_session_id": CONVERSATION,
                        "task_boundary": "new",
                    },
                ),
                _sctx(
                    "task_checkpoint",
                    {
                        "agent_kind": "cursor",
                        "external_session_id": CONVERSATION,
                        "claims": [{"context_kind": "decision"}, {"context_kind": "discovery"}],
                        "unknowns": [],
                    },
                ),
            ]
        )
    )
    lines.append(json.dumps({"type": "turn_ended", "status": "success"}))

    # Cursor's own follow-up prompt: must NOT open a turn.
    lines.append(
        _user(
            f"{T1_STAMP}\n\n<user_query>Briefly inform the user about the task result and "
            "perform any follow-up actions (if needed).</user_query>"
        )
    )
    lines.append(_assistant([{"type": "text", "text": "synthetic follow-up reply"}]))

    # A user message with no <user_query> at all: also the host talking.
    lines.append(_user("Your previous response was interrupted. Continue from where you left off."))

    # Turn 2: a skills preamble ahead of the query, and a JSON *string*
    # arguments payload that is truncated mid-object (the real fixture had one).
    lines.append(
        _user(
            "<manually_attached_skills>\nsynthetic inlined skill body\n"
            "</manually_attached_skills>\n"
            f"{T2_STAMP}\n<user_query>synthetic prompt two</user_query>"
        )
    )
    lines.append(
        _assistant(
            [
                _tool("GetDynamicTools", {"namespace": "user-shared-context", "toolName": "x"}),
                _sctx("task_checkpoint", '{"agent_kind":"cursor","claims":[{"a":1}'),
                # Same server, registered without Cursor's scope prefix. Real
                # transcripts carry both spellings; both must be recognised.
                _sctx(
                    "candidate_discard",
                    {
                        "agent_kind": "cursor",
                        "external_session_id": CONVERSATION,
                        "candidate_ids": ["cnd_syn_1"],
                    },
                    namespace="shared-context",
                ),
                # A non-sctx dynamic tool must not be counted as an sctx call.
                _tool(
                    "CallDynamicTool",
                    {"namespace": "some-other-server", "toolName": "unrelated", "arguments": {}},
                ),
                {"type": "text", "text": "synthetic assistant reply two"},
            ]
        )
    )
    lines.append(json.dumps({"type": "turn_ended", "status": "error", "error": "synthetic failure"}))
    return lines


def write_transcript(root: Path, lines: list) -> Path:
    directory = root / "projects" / "syn-slug" / "agent-transcripts" / CONVERSATION
    directory.mkdir(parents=True)
    path = directory / f"{CONVERSATION}.jsonl"
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return path


def write_sctx_state(home: Path, digest: str) -> None:
    """A minimal synthetic sctx installation: the three tables the parser
    joins, plus a hook-diagnostics file keyed by the caller's digest."""
    state = home / ".shared-context" / "state"
    state.mkdir(parents=True)
    connection = sqlite3.connect(state / "runtime.sqlite")
    connection.executescript(
        """
        CREATE TABLE external_session (
            external_session_id TEXT, agent_kind TEXT, external_session_key TEXT);
        CREATE TABLE task_session (task_session_id TEXT, external_session_id TEXT, task_id TEXT);
        CREATE TABLE task_injection (
            task_id TEXT, context_id TEXT, intent_revision_id TEXT, revision_id TEXT,
            injected_at_unix_seconds TEXT, source TEXT);
        """
    )
    connection.execute(
        "INSERT INTO external_session VALUES ('xss_syn', 'cursor', ?)", (CONVERSATION,)
    )
    connection.execute("INSERT INTO task_session VALUES ('tss_syn', 'xss_syn', 'tsk_syn')")
    rows = [
        # Two rows one second into turn 1 -> one grouped event with 2 ctx ids.
        ("tsk_syn", "ctx_syn_a", "tir_syn_1", "rev_syn_1", str(T1_EPOCH + 1), "task_context"),
        ("tsk_syn", "ctx_syn_b", "tir_syn_1", "rev_syn_2", str(T1_EPOCH + 1), "task_context"),
        # One row inside turn 2.
        ("tsk_syn", "ctx_syn_c", "tir_syn_2", "rev_syn_3", str(T2_EPOCH + 5), "intent_update"),
    ]
    connection.executemany("INSERT INTO task_injection VALUES (?,?,?,?,?,?)", rows)
    connection.commit()
    connection.close()

    logs = home / ".shared-context-logs" / "state"
    logs.mkdir(parents=True)
    (logs / "hook-diagnostics.json").write_text(
        json.dumps(
            {
                "schema_version": 1,
                "total": 4,
                "recent_events": [
                    {
                        "operation": "hook.cursor.session_start",
                        "outcome": "success",
                        "reason": "ok",
                        "occurred_at_unix_ms": (T1_EPOCH - 2) * 1000,
                        "session_digest": digest,
                    },
                    {
                        "operation": "hook.cursor.post_tool_use",
                        "outcome": "success",
                        "reason": "ok",
                        "occurred_at_unix_ms": (T1_EPOCH + 2) * 1000,
                        "session_digest": digest,
                    },
                    {
                        "operation": "hook.cursor.post_tool_use",
                        "outcome": "success",
                        "reason": "ok",
                        "occurred_at_unix_ms": (T2_EPOCH + 2) * 1000,
                        "session_digest": digest,
                    },
                    {
                        "operation": "hook.cursor.turn_stop",
                        "outcome": "success",
                        "reason": "ok",
                        "occurred_at_unix_ms": (T2_EPOCH + 9) * 1000,
                        "session_digest": "some-other-session",
                    },
                ],
            }
        ),
        encoding="utf-8",
    )


class CursorHelperTests(unittest.TestCase):
    def test_slugify_matches_cursors_project_directory_naming(self):
        self.assertEqual(
            slugify_cwd("/Users/someone/work/cross/fe/search_web_monorepo"),
            "Users-someone-work-cross-fe-search-web-monorepo",
        )

    def test_timestamp_parses_with_its_stated_offset(self):
        stamp = parse_cursor_timestamp("Monday, Mar 2, 2026, 9:00 AM (UTC+8)")
        self.assertIsNotNone(stamp)
        self.assertEqual(int(stamp.timestamp()), T1_EPOCH)

    def test_timestamp_without_offset_is_read_as_utc(self):
        stamp = parse_cursor_timestamp("Monday, Mar 2, 2026, 9:00 AM")
        self.assertEqual(stamp.utcoffset().total_seconds(), 0)

    def test_unparseable_timestamp_is_none_rather_than_a_guess(self):
        self.assertIsNone(parse_cursor_timestamp("some time yesterday"))

    def test_split_user_message_separates_query_from_preamble(self):
        query, stamp, preamble = split_user_message(
            f"{T1_STAMP}\n<user_query>\n  hello  \n</user_query>"
        )
        self.assertEqual(query, "hello")
        self.assertEqual(int(stamp.timestamp()), T1_EPOCH)
        self.assertIn("<timestamp>", preamble)

    def test_host_continuation_prompts_are_not_human(self):
        self.assertFalse(is_human_prompt(None))
        self.assertFalse(is_human_prompt("Briefly inform the user about the task result."))
        self.assertFalse(
            is_human_prompt("The beginning of the above subagent result is already visible.")
        )
        self.assertTrue(is_human_prompt("please refactor the parser"))


class CursorParserTests(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.root = Path(self._tmp.name)
        self.cursor_home = self.root / "cursor"
        self.path = write_transcript(self.cursor_home, build_synthetic_transcript())
        self.session = parse_transcript(self.path, conversation_id=CONVERSATION)

    def tearDown(self):
        self._tmp.cleanup()

    def test_resolve_transcript_finds_the_conversation_by_id_alone(self):
        self.assertEqual(
            resolve_transcript(CONVERSATION, cursor_home=self.cursor_home), self.path
        )
        self.assertIsNone(resolve_transcript("no-such-id", cursor_home=self.cursor_home))

    def test_only_human_prompts_open_turns(self):
        self.assertEqual(self.session.meta.host, "cursor")
        self.assertEqual([turn.user_text for turn in self.session.turns],
                         ["synthetic prompt one", "synthetic prompt two"])

    def test_host_prompts_are_recorded_as_other_injections(self):
        heads = " | ".join(item.text_head for item in self.session.other_injections)
        self.assertIn("cursor host prompt: Briefly inform", heads)
        self.assertIn("cursor host prompt: Your previous response", heads)
        self.assertIn("manually_attached_skills", heads)
        self.assertIn("turn_ended status=error", heads)

    def test_turn_timestamps_come_from_the_user_message_tag(self):
        self.assertEqual(
            int(__import__("datetime").datetime.fromisoformat(
                self.session.turns[0].started_at).timestamp()),
            T1_EPOCH,
        )
        self.assertEqual(
            int(__import__("datetime").datetime.fromisoformat(
                self.session.turns[1].started_at).timestamp()),
            T2_EPOCH,
        )

    def test_both_namespace_spellings_are_recognised(self):
        from hosts.cursor import is_sctx_namespace

        self.assertTrue(is_sctx_namespace("shared-context"))
        self.assertTrue(is_sctx_namespace("user-shared-context"))
        self.assertFalse(is_sctx_namespace("cursor-app-control"))
        self.assertFalse(is_sctx_namespace("shared-context-gardener"))
        self.assertFalse(is_sctx_namespace(None))

    def test_sctx_calls_are_picked_out_of_the_dynamic_tool_wrapper(self):
        first, second = self.session.turns
        self.assertEqual([call.tool for call in first.sctx_calls],
                         ["task_intent_update", "task_checkpoint"])
        # The other-namespace CallDynamicTool is a tool call but not an sctx call.
        self.assertEqual([call.tool for call in second.sctx_calls],
                         ["task_checkpoint", "candidate_discard"])
        self.assertIn("CallDynamicTool", [call.name for call in second.tool_calls])

    def test_full_arguments_are_kept_not_truncated(self):
        checkpoint = self.session.turns[0].sctx_calls[1]
        self.assertEqual(len(checkpoint.arguments["claims"]), 2)
        self.assertEqual(checkpoint.arguments["external_session_id"], CONVERSATION)

    def test_results_are_reported_as_unavailable_not_empty(self):
        for turn in self.session.turns:
            for call in turn.sctx_calls:
                self.assertIsNone(call.result_text)
                self.assertEqual(call.status, "unknown")
                self.assertIsNone(call.error_code)

    def test_truncated_string_arguments_are_preserved_as_unparsed(self):
        broken = self.session.turns[1].sctx_calls[0]
        self.assertTrue(broken.arguments["__arguments_unparsed__"])
        self.assertIn("claims", broken.arguments["__arguments_text__"])

    def test_fidelity_notes_name_the_two_structural_absences(self):
        joined = " ".join(self.session.meta.fidelity_notes)
        self.assertIn("tool outputs are NOT recorded", joined)
        self.assertIn("hook injections are NOT recorded", joined)

    def test_no_injections_come_from_the_transcript_itself(self):
        self.assertEqual(sum(len(turn.hook_injections) for turn in self.session.turns), 0)
        self.assertEqual(sum(len(turn.pack_deliveries) for turn in self.session.turns), 0)


class CursorReconstructionTests(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.root = Path(self._tmp.name)
        self.cursor_home = self.root / "cursor"
        self.home = self.root / "home"
        self.path = write_transcript(self.cursor_home, build_synthetic_transcript())
        sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
        import sctx_logs

        digest = sctx_logs.telemetry_session_digest("cursor", CONVERSATION)
        self.assertIsNotNone(digest)
        write_sctx_state(self.home, digest)
        self.session = parse_transcript(self.path, conversation_id=CONVERSATION)
        self.record = reconstruct_from_sctx(
            self.session, self.home, external_session_id=CONVERSATION
        )

    def tearDown(self):
        self._tmp.cleanup()

    def test_rows_are_grouped_into_events_not_counted_one_per_context_id(self):
        self.assertEqual(self.record["task_injection_rows"], 3)
        self.assertEqual(self.record["task_injection_events"], 2)

    def test_events_land_in_the_turn_whose_window_contains_them(self):
        first, second = self.session.turns
        self.assertEqual(len(first.pack_deliveries), 1)
        self.assertEqual(first.pack_deliveries[0].ctx_ids, ["ctx_syn_a", "ctx_syn_b"])
        self.assertEqual(first.pack_deliveries[0].tool, "sctx:task_context")
        self.assertEqual(len(second.pack_deliveries), 1)
        self.assertEqual(second.pack_deliveries[0].ctx_ids, ["ctx_syn_c"])

    def test_every_reconstructed_pack_delivery_is_flagged_and_claims_no_bytes(self):
        for turn in self.session.turns:
            for pack in turn.pack_deliveries:
                self.assertEqual(pack.reconstructed_from, "sctx:task_injection")
                self.assertEqual(pack.bytes, 0)
                self.assertIsNone(pack.delivered_bytes)
                self.assertEqual(pack.channel, "unknown")
                self.assertIsNone(pack.channel_cap_bytes)
                self.assertFalse(pack.truncated)
                self.assertIn("reconstructed from sctx", pack.text)

    def test_hook_events_are_bucketed_by_digest_and_by_window(self):
        first, second = self.session.turns
        # The pre-turn-1 session_start falls into turn 1 because turn 1's lower
        # bound is open; the other-session turn_stop is filtered out by digest.
        self.assertEqual(sum(event.count for event in first.hook_events), 2)
        self.assertEqual(sum(event.count for event in second.hook_events), 1)
        self.assertEqual(self.record["hook_diagnostics_events"], 3)
        self.assertEqual(self.record["unwindowed"]["hook_event_count"], 0)

    def test_a_transcript_without_timestamps_degrades_to_session_level(self):
        lines = [
            _user("<user_query>undated synthetic prompt</user_query>"),
            _assistant([{"type": "text", "text": "reply"}]),
        ]
        other = self.root / "cursor-undated"
        path = write_transcript(other, lines)
        session = parse_transcript(path, conversation_id=CONVERSATION)
        record = reconstruct_from_sctx(session, self.home, external_session_id=CONVERSATION)
        self.assertIn("session-level only", record["windowing"])
        self.assertEqual(len(record["unwindowed"]["injection_events"]), 2)
        self.assertEqual(sum(len(turn.pack_deliveries) for turn in session.turns), 0)

    def test_load_session_reports_an_unknown_cwd_instead_of_inventing_one(self):
        session, path = load_session(
            CONVERSATION, home=self.home, cursor_home=self.cursor_home
        )
        self.assertEqual(path, self.path)
        self.assertEqual(session.meta.cwd, "")
        self.assertTrue(any("cwd unknown" in note for note in session.meta.fidelity_notes))


if __name__ == "__main__":
    unittest.main()
