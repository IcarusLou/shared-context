"""Unit tests for candidates.py against SYNTHETIC data built in-line in this
file. No real transcript text, real thread ids, or real repo paths are used
anywhere here.

Run with:
    python3 -m unittest discover -s tests/scripts/session_replay/tests
"""
from __future__ import annotations

import sqlite3
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
from session_model import (  # noqa: E402
    Injection,
    SctxCall,
    Session,
    SessionMeta,
    ToolCall,
    Turn,
)
import candidates  # noqa: E402


def _tool(name: str, args_head: str = "") -> ToolCall:
    return ToolCall(name=name, args_head=args_head)


def build_synthetic_good_session() -> Session:
    """One long, specific first prompt; turn 1 does almost all the work;
    no followups, no questions, an active sctx marker."""
    session = Session(meta=SessionMeta(
        host="codex", thread_id="syn-good-0001",
        cwd="/tmp/registered-repo/checkout",
        git_branch="main", cli_version="0.0.0-test",
        started_at="2026-09-01T00:00:00Z",
    ))
    turn1 = Turn(
        index=1,
        user_text=(
            "Refactor `src/foo/bar.rs` and the matching `src/foo/bar.ts` caller: "
            + "x" * 300
        ),
        assistant_texts=["done."],
        tool_calls=[_tool("exec", "cargo build"), _tool("exec", "cargo test")],
        injections=[Injection(kind="prompt_submit", text="pack", has_marker=True,
                               ctx_ids=["ctx_1", "ctx_2"])],
        sctx_calls=[SctxCall(tool="task_intent_update", arguments={"goal": "x"})],
    )
    session.turns.append(turn1)
    return session


def build_synthetic_bad_session() -> Session:
    """Short, URL-only first prompt; lots of short 'continue'-style
    followups; an agent that keeps asking; an external (lark/curl) tool
    dependency; no sctx activity."""
    session = Session(meta=SessionMeta(
        host="codex", thread_id="syn-bad-0001",
        cwd="/tmp/unregistered-repo/checkout",
        started_at="2026-09-01T00:00:00Z",
    ))
    turn1 = Turn(
        index=1,
        user_text="参考这个文档 https://example.larkoffice.com/wiki/abc123",
        assistant_texts=["这是我的理解，是否需要我继续？"],
        tool_calls=[_tool("exec", "curl https://example.larkoffice.com/wiki/abc123")],
    )
    turn2 = Turn(index=2, user_text="继续")
    turn3 = Turn(index=3, user_text="确认 1、2")
    session.turns.extend([turn1, turn2, turn3])
    return session


class TestDetectionHelpers(unittest.TestCase):
    def test_first_prompt_has_url_true_for_http(self):
        self.assertTrue(candidates._first_prompt_has_url("see https://example.com/x"))

    def test_first_prompt_has_url_true_for_lark(self):
        self.assertTrue(candidates._first_prompt_has_url("参考这个 lark 文档"))

    def test_first_prompt_has_url_false_for_plain_text(self):
        self.assertFalse(candidates._first_prompt_has_url("just refactor the parser please"))

    def test_first_prompt_mentions_files_extension(self):
        self.assertTrue(candidates._first_prompt_mentions_files("fix src/App.swift please"))

    def test_first_prompt_mentions_files_backtick(self):
        self.assertTrue(candidates._first_prompt_mentions_files("rename `SearchViewModel` class"))

    def test_first_prompt_mentions_files_path(self):
        self.assertTrue(candidates._first_prompt_mentions_files("edit app/src/main/foo.kt"))

    def test_first_prompt_mentions_files_false(self):
        self.assertFalse(candidates._first_prompt_mentions_files("summarize the current branch"))

    def test_external_dep_hits_keyword(self):
        hits = candidates._external_dep_hits("curl -s https://api.example.com/data")
        self.assertIn("curl", hits)
        self.assertIn("network-host", hits)

    def test_external_dep_hits_none(self):
        self.assertEqual(candidates._external_dep_hits("cargo build --release"), [])

    def test_external_dep_hits_gh_requires_trailing_space(self):
        self.assertIn("gh ", candidates._external_dep_hits("gh pr view 123"))
        self.assertEqual(candidates._external_dep_hits("night"), [])

    def test_is_agent_question_ascii_mark(self):
        self.assertTrue(candidates._is_agent_question("should I proceed?"))

    def test_is_agent_question_cjk_mark(self):
        self.assertTrue(candidates._is_agent_question("需要我继续吗？"))

    def test_is_agent_question_phrase(self):
        self.assertTrue(candidates._is_agent_question("这是我的理解，请确认后继续。"))

    def test_is_agent_question_false(self):
        self.assertFalse(candidates._is_agent_question("Implemented the change and ran the tests."))

    def test_first_prompt_trivial_short_text(self):
        self.assertTrue(candidates._first_prompt_is_trivial("fix it", mentions_files=False))

    def test_first_prompt_trivial_slash_command(self):
        # Long enough to pass the length check on its own, still a command.
        self.assertTrue(
            candidates._first_prompt_is_trivial("/statusline please run this now", mentions_files=False)
        )

    def test_first_prompt_trivial_generic_phrase_without_file_mention(self):
        self.assertTrue(
            candidates._first_prompt_is_trivial(
                "总结一下当前这个分支上都改了些什么内容以及为什么这么改", mentions_files=False
            )
        )

    def test_first_prompt_not_trivial_generic_phrase_with_file_mention(self):
        # Same generic verb, but anchored to a real file -> not trivial.
        self.assertFalse(
            candidates._first_prompt_is_trivial(
                "总结一下 `src/foo/bar.rs` 里这个改动的意图和影响范围", mentions_files=True
            )
        )

    def test_first_prompt_not_trivial_long_specific_text(self):
        text = "Refactor the retry logic in the network client: " + "x" * 60
        self.assertFalse(candidates._first_prompt_is_trivial(text, mentions_files=False))


class TestRepositoryRegistration(unittest.TestCase):
    def test_load_registered_repositories_parses_paths_array(self):
        with tempfile.TemporaryDirectory() as tmp:
            config = Path(tmp) / "config.toml"
            config.write_text(
                'version = 1\n'
                '[[repositories]]\n'
                'id = "Demo"\n'
                'paths = ["/tmp/registered-repo/checkout", "/tmp/registered-repo/other"]\n'
            )
            entries, note = candidates.load_registered_repositories(config)
            self.assertIsNone(note)
            self.assertIn(("Demo", "/tmp/registered-repo/checkout"), entries)
            self.assertIn(("Demo", "/tmp/registered-repo/other"), entries)

    def test_load_registered_repositories_missing_file(self):
        entries, note = candidates.load_registered_repositories(Path("/nonexistent/config.toml"))
        self.assertEqual(entries, [])
        self.assertIsNotNone(note)

    def test_match_repository_prefix(self):
        entries = [("Demo", "/tmp/registered-repo/checkout")]
        self.assertEqual(
            candidates._match_repository("/tmp/registered-repo/checkout", entries), "Demo"
        )
        self.assertEqual(
            candidates._match_repository("/tmp/registered-repo/checkout/sub/dir", entries), "Demo"
        )

    def test_match_repository_no_match(self):
        entries = [("Demo", "/tmp/registered-repo/checkout")]
        self.assertIsNone(candidates._match_repository("/tmp/unregistered-repo/checkout", entries))

    def test_match_repository_does_not_match_sibling_prefix(self):
        # "/tmp/registered-repo-2" must NOT match a registered path of
        # "/tmp/registered-repo" (naive str.startswith without the "/" guard
        # would wrongly match this).
        entries = [("Demo", "/tmp/registered-repo")]
        self.assertIsNone(candidates._match_repository("/tmp/registered-repo-2/checkout", entries))


class TestComputeMetricsAndScore(unittest.TestCase):
    def setUp(self):
        self.registered = [("Demo", "/tmp/registered-repo/checkout")]

    def test_good_session_is_registered_active_and_scores_high(self):
        session = build_synthetic_good_session()
        m = candidates.compute_metrics("codex", session.meta.thread_id, session, 1234, self.registered)
        self.assertEqual(m["human_prompts"], 1)
        self.assertTrue(m["first_prompt_mentions_files"])
        self.assertFalse(m["first_prompt_has_url"])
        self.assertEqual(m["turn1_tool_calls"], 2)
        self.assertEqual(m["total_tool_calls"], 2)
        self.assertEqual(m["turn1_tool_share"], 1.0)
        self.assertTrue(m["sctx_active"])
        self.assertEqual(m["ctx_injected"], 2)
        self.assertTrue(m["repo_registered"])
        self.assertEqual(m["repo_id"], "Demo")
        self.assertEqual(m["agent_questions"], 0)
        self.assertEqual(m["short_followups"], 0)
        self.assertEqual(m["external_deps"], 0)

    def test_bad_session_is_unregistered_inactive_and_scores_low(self):
        session = build_synthetic_bad_session()
        m = candidates.compute_metrics("codex", session.meta.thread_id, session, 1234, self.registered)
        self.assertEqual(m["human_prompts"], 3)
        self.assertTrue(m["first_prompt_has_url"])
        self.assertFalse(m["sctx_active"])
        self.assertFalse(m["repo_registered"])
        self.assertIsNone(m["repo_id"])
        self.assertEqual(m["short_followups"], 2)
        self.assertGreaterEqual(m["agent_questions"], 1)
        self.assertGreaterEqual(m["external_deps"], 1)
        self.assertIn("curl", m["external_dep_keywords"])

    def test_good_session_outscores_bad_session(self):
        good = candidates.compute_metrics(
            "codex", "syn-good-0001", build_synthetic_good_session(), 1, self.registered
        )
        bad = candidates.compute_metrics(
            "codex", "syn-bad-0001", build_synthetic_bad_session(), 1, self.registered
        )
        self.assertGreater(good["score"], bad["score"])
        # The bad session should score below zero given URL + questions +
        # followups + external deps all stack as penalties.
        self.assertLess(bad["score"], 0)

    def test_score_is_deterministic_pure_function_of_metrics(self):
        m = candidates.compute_metrics(
            "codex", "syn-good-0001", build_synthetic_good_session(), 1, self.registered
        )
        self.assertEqual(candidates.score_session(m), m["score"])

    def test_trivial_prompt_penalty_is_applied(self):
        base = candidates.compute_metrics(
            "codex", "syn-good-0001", build_synthetic_good_session(), 1, self.registered
        )
        trivial = dict(base)
        trivial["first_prompt_trivial"] = True
        base["first_prompt_trivial"] = False
        self.assertAlmostEqual(
            candidates.score_session(base) - candidates.score_session(trivial),
            candidates.TRIVIAL_PROMPT_PENALTY,
            places=3,
        )

    def test_high_tool_volume_outranks_high_turn1_share_alone(self):
        """Regression for the owner's 2026-09-10 follow-up: a single 4-call
        prompt with 100% turn-1 share must NOT outscore a session that did a
        long, mostly-turn-1 stretch of real work (101 calls)."""
        tiny_session = Session(meta=SessionMeta(host="codex", thread_id="syn-tiny",
                                                  cwd="/tmp/registered-repo/checkout"))
        tiny_session.turns.append(Turn(
            index=1,
            user_text="Refactor `src/foo/bar.rs` for the new retry policy end to end.",
            tool_calls=[_tool("exec") for _ in range(4)],
        ))
        big_session = Session(meta=SessionMeta(host="codex", thread_id="syn-big",
                                                 cwd="/tmp/registered-repo/checkout"))
        big_session.turns.append(Turn(
            index=1,
            user_text="Refactor `src/foo/bar.rs` for the new retry policy end to end.",
            tool_calls=[_tool("exec") for _ in range(101)],
        ))
        tiny = candidates.compute_metrics("codex", "syn-tiny", tiny_session, 1, self.registered)
        big = candidates.compute_metrics("codex", "syn-big", big_session, 1, self.registered)
        self.assertEqual(tiny["turn1_tool_share"], 1.0)
        self.assertEqual(big["turn1_tool_share"], 1.0)
        self.assertGreater(big["score"], tiny["score"])

    def test_min_total_tools_default_matches_owner_instruction(self):
        self.assertEqual(candidates.DEFAULT_MIN_TOTAL_TOOLS, 15)


class TestCodexCandidateIteration(unittest.TestCase):
    def test_iter_codex_candidates_filters_by_since_and_thread_source(self):
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            db_path = home / "state_5.sqlite"
            rollout_old = home / "old.jsonl"
            rollout_new = home / "new.jsonl"
            rollout_old.write_text("{}\n")
            rollout_new.write_text("{}\n")

            conn = sqlite3.connect(str(db_path))
            conn.execute(
                "CREATE TABLE threads (id TEXT, rollout_path TEXT, created_at_ms INTEGER, "
                "created_at INTEGER, thread_source TEXT)"
            )
            conn.execute(
                "INSERT INTO threads VALUES (?, ?, ?, ?, ?)",
                ("syn-old", str(rollout_old), 1000, None, "user"),
            )
            conn.execute(
                "INSERT INTO threads VALUES (?, ?, ?, ?, ?)",
                ("syn-new", str(rollout_new), 4102444800000, None, "user"),  # year 2100
            )
            conn.execute(
                "INSERT INTO threads VALUES (?, ?, ?, ?, ?)",
                ("syn-nonuser", str(rollout_new), 4102444800000, None, "assistant-initiated"),
            )
            conn.commit()
            conn.close()

            import datetime as dt
            since = dt.datetime(2050, 1, 1, tzinfo=dt.timezone.utc)
            results = list(candidates.iter_codex_candidates(home, since))
            ids = {r[0] for r in results}
            self.assertIn("syn-new", ids)
            self.assertNotIn("syn-old", ids)
            self.assertNotIn("syn-nonuser", ids)

    def test_iter_codex_candidates_no_since_returns_all_user_rows(self):
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            db_path = home / "state_5.sqlite"
            rollout = home / "r.jsonl"
            rollout.write_text("{}\n")
            conn = sqlite3.connect(str(db_path))
            conn.execute(
                "CREATE TABLE threads (id TEXT, rollout_path TEXT, created_at_ms INTEGER, "
                "created_at INTEGER, thread_source TEXT)"
            )
            conn.execute(
                "INSERT INTO threads VALUES (?, ?, ?, ?, ?)",
                ("syn-any", str(rollout), None, None, "user"),
            )
            conn.commit()
            conn.close()
            results = list(candidates.iter_codex_candidates(home, None))
            self.assertEqual([r[0] for r in results], ["syn-any"])


if __name__ == "__main__":
    unittest.main()
