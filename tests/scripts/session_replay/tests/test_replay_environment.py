"""Unit tests for the three replay-fidelity defects found by the 2026-09-12 paired replay.

All three had the same shape: the driver did something the manifest did not record, so an audit
read a clean run where there had been none.

    python3 -m unittest discover -s tests/scripts/session_replay/tests
"""

from __future__ import annotations

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import replay  # noqa: E402


def git(*args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(["git", *args], capture_output=True, text=True, check=True)


class ProxyPolicyTests(unittest.TestCase):
    """The Codex child used to inherit the caller's proxy with nothing recorded about it.

    Measured: a local proxy on 127.0.0.1:17890 reset the app-server's websocket on every turn, all
    three prompts produced nothing, and the manifest still described three replayed turns.
    """

    def base_environment(self) -> dict[str, str]:
        return {
            "HOME": "/isolated/home",
            "CODEX_HOME": "/isolated/codex",
            "HTTP_PROXY": "http://127.0.0.1:17890",
            "https_proxy": "http://127.0.0.1:17890",
        }

    def test_an_inherited_proxy_is_recorded_and_warned_about(self) -> None:
        environment = self.base_environment()
        warnings: list[str] = []
        record = replay.apply_proxy_policy(environment, None, False, warnings)
        self.assertEqual(record["proxy"]["source"], "inherited")
        self.assertEqual(
            record["proxy"]["values"],
            {"HTTP_PROXY": "http://127.0.0.1:17890", "https_proxy": "http://127.0.0.1:17890"},
        )
        # Inheriting is still the default -- the point is that it can no longer happen silently.
        self.assertEqual(environment["HTTP_PROXY"], "http://127.0.0.1:17890")
        self.assertTrue(any("inherited a proxy" in note for note in warnings))

    def test_no_inherited_proxy_produces_no_warning(self) -> None:
        warnings: list[str] = []
        record = replay.apply_proxy_policy(
            {"HOME": "/h", "CODEX_HOME": "/c"}, None, False, warnings
        )
        self.assertEqual(record["proxy"], {"source": "inherited", "values": {}})
        self.assertEqual(warnings, [])

    def test_an_explicit_proxy_replaces_every_variable_in_both_cases(self) -> None:
        environment = self.base_environment()
        record = replay.apply_proxy_policy(environment, "http://127.0.0.1:7897", False, [])
        self.assertEqual(record["proxy"]["source"], "--proxy")
        for name in replay.PROXY_VARIABLES:
            self.assertEqual(environment[name], "http://127.0.0.1:7897", name)
        self.assertEqual(set(record["proxy"]["values"]), set(replay.PROXY_VARIABLES))

    def test_no_proxy_strips_every_variable_and_says_which(self) -> None:
        environment = self.base_environment()
        record = replay.apply_proxy_policy(environment, None, True, [])
        self.assertEqual(record["proxy"]["source"], "--no-proxy")
        self.assertEqual(record["proxy"]["removed"], ["HTTP_PROXY", "https_proxy"])
        for name in replay.PROXY_VARIABLES:
            self.assertNotIn(name, environment)
        self.assertEqual(environment["NO_PROXY"], "*")


class StreamInterruptionTests(unittest.TestCase):
    """Nothing used to match on Codex's reconnect text, so a wedged stream ran the full budget."""

    def test_both_recovery_strings_are_counted_case_insensitively(self) -> None:
        line = (
            '{"method":"error","params":{"message":"responseStreamDisconnected"}} '
            "Reconnecting in 1s... reconnecting again"
        )
        self.assertEqual(replay.count_stream_interruptions(line), 3)

    def test_ordinary_output_counts_nothing(self) -> None:
        self.assertEqual(replay.count_stream_interruptions('{"method":"turn/completed"}'), 0)
        self.assertEqual(replay.count_stream_interruptions(""), 0)

    def test_read_from_returns_only_what_was_appended(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            path = Path(raw) / "turn.stderr"
            path.write_text("before\n", encoding="utf-8")
            offset = path.stat().st_size
            with path.open("a", encoding="utf-8") as handle:
                handle.write("Reconnecting\n")
            self.assertEqual(replay.read_from(path, offset), "Reconnecting\n")
            self.assertEqual(replay.count_stream_interruptions(replay.read_from(path, offset)), 1)

    def test_read_from_a_missing_file_is_empty_rather_than_fatal(self) -> None:
        self.assertEqual(replay.read_from(Path("/nonexistent/turn.stderr"), 0), "")


class TurnResultTests(unittest.TestCase):
    def test_a_turn_carries_its_own_id_and_a_failure_flag(self) -> None:
        # `thread_id` is the same for every turn of a replay by design; it was the only per-turn
        # identity in the manifest, so three turns read as three records of one.
        result = replay.TurnResult(
            prompt_index=1,
            prompt_chars=10,
            started="s",
            finished="f",
            exit_code=1,
            thread_id="thread-1",
            usage=None,
            waited_for_turn_stop=False,
            stream_path="a",
            stderr_path="b",
            errors=["boom"],
            turn_id="turn-1",
            stream_interruptions=4,
            failed=True,
        )
        self.assertIn("turn_id", result.__dict__)
        self.assertIn("failed", result.__dict__)
        self.assertNotEqual(result.turn_id, result.thread_id)


class WorktreeBranchTests(unittest.TestCase):
    """A detached worktree made the replayed agent read `git_branch: '-'` as a repository fact.

    The `--detach` fallback fired on *any* non-zero exit from the `-b` form and discarded git's
    own reason, so the audit could not tell "the host would not give us a branch" from "the replay
    never asked for one".
    """

    def repository(self, root: Path) -> replay.Original:
        git("init", "--initial-branch=main", "-q", str(root))
        git("-C", str(root), "config", "user.email", "fixture@example.com")
        git("-C", str(root), "config", "user.name", "Fixture")
        (root / "file.txt").write_text("one\n", encoding="utf-8")
        git("-C", str(root), "add", "file.txt")
        git("-C", str(root), "commit", "-q", "-m", "one")
        commit = git("-C", str(root), "rev-parse", "HEAD").stdout.strip()
        original = replay.Original.__new__(replay.Original)
        object.__setattr__(original, "cwd", root)
        object.__setattr__(original, "commit", commit)
        object.__setattr__(original, "branch", "main")
        object.__setattr__(original, "thread_id", "thread-fixture")
        return original

    def test_the_first_replay_gets_an_attached_branch(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            original = self.repository(root / "repo")
            warnings: list[str] = []
            head, branch, record = replay.prepare_worktree(
                original, root / "wt", "20260912T000000Z-abcd", warnings
            )
            self.assertEqual(head, original.commit)
            self.assertEqual(branch, "replay/main")
            self.assertEqual(record["branch_mode"], "created")
            self.assertEqual(record["attempts"], [])
            self.assertEqual(warnings, [])
            self.assertEqual(
                git("-C", str(root / "wt"), "branch", "--show-current").stdout.strip(),
                "replay/main",
                "a detached worktree is what makes git_branch report '-'",
            )

    def test_a_second_replay_uniquifies_rather_than_detaching(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            original = self.repository(root / "repo")
            replay.prepare_worktree(original, root / "wt-1", "first", [])
            warnings: list[str] = []
            _, branch, record = replay.prepare_worktree(
                original, root / "wt-2", "second", warnings
            )
            self.assertEqual(branch, "replay/main-second")
            self.assertEqual(record["branch_mode"], "created_after_collision")
            self.assertEqual(warnings, [], "a foreseen collision is not a warning")

    def test_an_unobtainable_branch_is_detached_but_reported_with_gits_own_reason(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            original = self.repository(root / "repo")
            # Both candidate names already exist as branches checked out nowhere but occupied by
            # refs the `-b` form refuses to create again.
            for name in ("replay/main", "replay/main-taken"):
                git("-C", str(root / "repo"), "branch", name, original.commit)
            warnings: list[str] = []
            _, branch, record = replay.prepare_worktree(
                original, root / "wt", "taken", warnings
            )
            self.assertIsNone(branch)
            self.assertEqual(record["branch_mode"], "detached")
            self.assertEqual([attempt["branch"] for attempt in record["attempts"]],
                             ["replay/main-taken"])
            self.assertTrue(record["attempts"][0]["error"], "git's reason must be kept")
            self.assertTrue(any("worktree add -b" in note for note in warnings))


if __name__ == "__main__":
    unittest.main()
