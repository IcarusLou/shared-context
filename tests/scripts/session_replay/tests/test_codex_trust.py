"""Unit tests for codex_trust.py against SYNTHETIC hooks.json / config.toml files
built inline in this file. No real hook command line, path or hash is used here;
the proof against the operator's real ~/.codex lives in `codex_trust.py`'s own
`--config` verifier, which is a runtime check rather than a fixture.

Run with:
    python3 -m unittest discover -s tests/scripts/session_replay/tests
"""

from __future__ import annotations

import hashlib
import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import codex_trust  # noqa: E402
import replay  # noqa: E402


def command_hook(command: str, **extra: object) -> dict:
    handler = {"type": "command", "command": command, "statusMessage": "Test"}
    handler.update(extra)
    return handler


def hooks_file(**events: object) -> dict:
    return {"hooks": dict(events)}


def expected_hash(payload: dict) -> str:
    body = json.dumps(payload, separators=(",", ":"), ensure_ascii=False).encode("utf-8")
    return "sha256:" + hashlib.sha256(body).hexdigest()


class VersionForTomlTests(unittest.TestCase):
    def test_sorts_keys_recursively_and_serializes_compactly(self) -> None:
        value = {"b": 1, "a": [{"z": True, "y": "x"}]}
        self.assertEqual(
            codex_trust.version_for_toml(value),
            expected_hash({"a": [{"y": "x", "z": True}], "b": 1}),
        )

    def test_non_ascii_is_not_escaped(self) -> None:
        # serde_json writes UTF-8 directly, so a hash computed with ensure_ascii
        # would silently differ for any hook whose status message is not English.
        self.assertEqual(
            codex_trust.version_for_toml({"a": "中文"}),
            expected_hash({"a": "中文"}),
        )


class HookHashTests(unittest.TestCase):
    def test_command_hook_identity_shape(self) -> None:
        digest = codex_trust.hook_hash("PostToolUse", None, command_hook("/bin/true"))
        self.assertEqual(
            digest,
            expected_hash(
                {
                    "event_name": "post_tool_use",
                    "hooks": [
                        {
                            "async": False,
                            "command": "/bin/true",
                            "statusMessage": "Test",
                            "timeout": 600,
                            "type": "command",
                        }
                    ],
                }
            ),
        )

    def test_absent_matcher_contributes_no_key(self) -> None:
        # toml::Value::try_from drops a None-valued entry; a null would change the hash.
        with_matcher = codex_trust.hook_hash("PostToolUse", "", command_hook("/bin/true"))
        without = codex_trust.hook_hash("PostToolUse", None, command_hook("/bin/true"))
        self.assertNotEqual(with_matcher, without)
        self.assertEqual(
            with_matcher,
            expected_hash(
                {
                    "event_name": "post_tool_use",
                    "hooks": [
                        {
                            "async": False,
                            "command": "/bin/true",
                            "statusMessage": "Test",
                            "timeout": 600,
                            "type": "command",
                        }
                    ],
                    "matcher": "",
                }
            ),
        )

    def test_matcher_is_dropped_for_events_that_ignore_it(self) -> None:
        for event in ("UserPromptSubmit", "Stop", "Interrupt"):
            with self.subTest(event=event):
                self.assertEqual(
                    codex_trust.hook_hash(event, "anything", command_hook("/bin/true")),
                    codex_trust.hook_hash(event, None, command_hook("/bin/true")),
                )

    def test_session_end_timeout_defaults_to_one_and_clamps_to_three(self) -> None:
        default = codex_trust.hook_hash("SessionEnd", None, command_hook("/bin/true"))
        explicit_one = codex_trust.hook_hash(
            "SessionEnd", None, command_hook("/bin/true", timeout=1)
        )
        self.assertEqual(default, explicit_one)
        clamped = codex_trust.hook_hash(
            "SessionEnd", None, command_hook("/bin/true", timeout=900)
        )
        self.assertEqual(
            clamped,
            codex_trust.hook_hash("SessionEnd", None, command_hook("/bin/true", timeout=3)),
        )

    def test_other_events_default_to_ten_minutes(self) -> None:
        self.assertEqual(
            codex_trust.hook_hash("SessionStart", None, command_hook("/bin/true")),
            codex_trust.hook_hash("SessionStart", None, command_hook("/bin/true", timeout=600)),
        )

    def test_additional_context_limit_normalization(self) -> None:
        # Equal to the default -> dropped; on an event that cannot emit
        # additionalContext -> dropped; otherwise kept.
        plain = codex_trust.hook_hash("SessionStart", None, command_hook("/bin/true"))
        at_default = codex_trust.hook_hash(
            "SessionStart", None, command_hook("/bin/true", additionalContextLimit=2500)
        )
        self.assertEqual(plain, at_default)
        raised = codex_trust.hook_hash(
            "SessionStart", None, command_hook("/bin/true", additionalContextLimit=9000)
        )
        self.assertNotEqual(plain, raised)
        ignored = codex_trust.hook_hash(
            "Stop", None, command_hook("/bin/true", additionalContextLimit=9000)
        )
        self.assertEqual(ignored, codex_trust.hook_hash("Stop", None, command_hook("/bin/true")))

    def test_command_change_changes_the_hash(self) -> None:
        # The whole point: retargeting a hook at a dev binary invalidates the
        # operator's stored trusted_hash, which is why replay.py must recompute.
        self.assertNotEqual(
            codex_trust.hook_hash("SessionStart", None, command_hook("/a/sctx hook")),
            codex_trust.hook_hash("SessionStart", None, command_hook("/b/sctx hook")),
        )

    def test_skipped_handlers_have_no_hash(self) -> None:
        self.assertIsNone(codex_trust.hook_hash("SessionStart", None, {"type": "prompt"}))
        self.assertIsNone(codex_trust.hook_hash("SessionStart", None, {"type": "agent"}))
        self.assertIsNone(codex_trust.hook_hash("SessionStart", None, command_hook("   ")))
        self.assertIsNone(
            codex_trust.hook_hash(
                "SessionEnd", None, {"type": "mcp_tool", "server": "s", "tool": "t"}
            )
        )

    def test_unknown_event_and_handler_type_are_refused(self) -> None:
        with self.assertRaises(codex_trust.TrustError):
            codex_trust.hook_hash("NoSuchEvent", None, command_hook("/bin/true"))
        with self.assertRaises(codex_trust.TrustError):
            codex_trust.hook_hash("Stop", None, {"type": "webhook"})

    def test_mcp_tool_hook_keeps_an_empty_input_map(self) -> None:
        digest = codex_trust.hook_hash(
            "PostToolUse", None, {"type": "mcp_tool", "server": "s", "tool": "t"}
        )
        self.assertEqual(
            digest,
            expected_hash(
                {
                    "event_name": "post_tool_use",
                    "hooks": [
                        {
                            "input": {},
                            "server": "s",
                            "timeout": 600,
                            "tool": "t",
                            "type": "mcp_tool",
                        }
                    ],
                }
            ),
        )


class HookStateEntryTests(unittest.TestCase):
    def test_keys_are_positional_and_survive_skipped_handlers(self) -> None:
        document = hooks_file(
            SessionStart=[
                {"hooks": [{"type": "prompt"}, command_hook("/bin/true")]},
                {"matcher": "x", "hooks": [command_hook("/bin/false")]},
            ]
        )
        entries = codex_trust.hook_state_entries(document, "/tmp/hooks.json")
        self.assertEqual(
            sorted(entries),
            [
                "/tmp/hooks.json:session_start:0:1",
                "/tmp/hooks.json:session_start:1:0",
            ],
        )

    def test_render_hook_state_is_parseable_toml(self) -> None:
        entries = {'/a b/hooks.json:stop:0:0': "sha256:00"}
        rendered = codex_trust.render_hook_state(entries)
        self.assertIn('[hooks.state."/a b/hooks.json:stop:0:0"]', rendered)
        try:
            import tomllib
        except ModuleNotFoundError:  # pragma: no cover - Python 3.10
            return
        parsed = tomllib.loads(rendered)
        self.assertEqual(
            parsed["hooks"]["state"]["/a b/hooks.json:stop:0:0"]["trusted_hash"], "sha256:00"
        )


class VerifyConfigTests(unittest.TestCase):
    def test_matched_mismatched_and_unresolved_are_separated(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            hooks_path = root / "hooks.json"
            document = hooks_file(
                SessionStart=[{"hooks": [command_hook("/bin/true")]}],
                Stop=[{"hooks": [command_hook("/bin/false")]}],
            )
            hooks_path.write_text(json.dumps(document), encoding="utf-8")
            entries = codex_trust.hook_state_entries(document, str(hooks_path))
            start_key = f"{hooks_path}:session_start:0:0"
            stop_key = f"{hooks_path}:stop:0:0"
            config = root / "config.toml"
            config.write_text(
                "[hooks.state]\n\n"
                f'[hooks.state."{start_key}"]\n'
                f'trusted_hash = "{entries[start_key]}"\n'
                "enabled = true\n\n"
                f'[hooks.state."{stop_key}"]\n'
                'trusted_hash = "sha256:deadbeef"\n\n'
                '[hooks.state."plugin@pack:hooks/hooks.json:stop:0:0"]\n'
                'trusted_hash = "sha256:cafe"\n\n'
                '[projects."/somewhere"]\n'
                'trust_level = "trusted"\n',
                encoding="utf-8",
            )
            report = codex_trust.verify_config(config)
        self.assertEqual(report["matched"], [start_key])
        self.assertEqual(len(report["mismatched"]), 1)
        self.assertEqual(report["mismatched"][0]["key"], stop_key)
        self.assertEqual(
            report["unresolved_key_sources"], ["plugin@pack:hooks/hooks.json:stop:0:0"]
        )
        self.assertEqual(report["state_entries"], 3)

    def test_stale_state_entries_are_reported_as_missing_not_mismatched(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            hooks_path = root / "hooks.json"
            hooks_path.write_text(json.dumps(hooks_file()), encoding="utf-8")
            config = root / "config.toml"
            config.write_text(
                f'[hooks.state."{hooks_path}:stop:0:0"]\n'
                'trusted_hash = "sha256:00"\n',
                encoding="utf-8",
            )
            report = codex_trust.verify_config(config)
        self.assertEqual(report["mismatched"], [])
        self.assertEqual(len(report["missing_from_hooks_file"]), 1)


class ReplayRetargetingTests(unittest.TestCase):
    """The replay-side glue that depends on the hash: rewriting hooks.json and
    the config.toml tables that trust it."""

    def test_retarget_sctx_hooks_rewrites_only_the_binary_path(self) -> None:
        document = hooks_file(
            SessionStart=[
                {
                    "hooks": [
                        command_hook(
                            "'/Users/x/.shared-context/bin/current/sctx' hook "
                            "--agent codex --agent-version '9.9.9'"
                        )
                    ]
                }
            ],
            Stop=[{"hooks": [command_hook("node /somewhere/else/track.js --event Stop")]}],
        )
        retargeted, count = replay.retarget_sctx_hooks(document, Path("/dev/build/sctx"))
        self.assertEqual(count, 1)
        self.assertEqual(
            retargeted["hooks"]["SessionStart"][0]["hooks"][0]["command"],
            "'/dev/build/sctx' hook --agent codex --agent-version '9.9.9'",
        )
        self.assertEqual(
            retargeted["hooks"]["Stop"][0]["hooks"][0]["command"],
            "node /somewhere/else/track.js --event Stop",
        )

    def test_drop_hook_state_tables_removes_only_the_named_source(self) -> None:
        body = (
            '[hooks.state."/a/hooks.json:stop:0:0"]\n'
            'trusted_hash = "sha256:00"\n'
            "enabled = true\n"
            "\n"
            '[hooks.state."/b/hooks.json:stop:0:0"]\n'
            'trusted_hash = "sha256:11"\n'
            "\n"
            '[projects."/p"]\n'
            'trust_level = "trusted"\n'
        )
        kept, dropped = replay.drop_hook_state_tables(body, "/a/hooks.json:")
        self.assertEqual(dropped, 1)
        self.assertNotIn("/a/hooks.json", kept)
        self.assertIn("/b/hooks.json", kept)
        self.assertIn('[projects."/p"]', kept)
        self.assertNotIn("enabled = true", kept)

    def test_retarget_mcp_command_touches_only_that_server(self) -> None:
        body = (
            "[mcp_servers.other]\n"
            'command = "/keep/me"\n'
            "\n"
            "[mcp_servers.shared-context]\n"
            'command = "/Users/x/.shared-context/bin/current/sctx"\n'
            'args = ["mcp", "serve"]\n'
            "\n"
            "[mcp_servers.shared-context.tools.context_get]\n"
            'approval_mode = "approve"\n'
        )
        rewritten, changed = replay.retarget_mcp_command(
            body, "shared-context", Path("/dev/build/sctx")
        )
        self.assertTrue(changed)
        self.assertIn('command = "/dev/build/sctx"\n', rewritten)
        self.assertIn('command = "/keep/me"\n', rewritten)
        self.assertIn('args = ["mcp", "serve"]', rewritten)

    def test_retarget_mcp_command_reports_a_missing_server(self) -> None:
        _, changed = replay.retarget_mcp_command(
            "[mcp_servers.other]\ncommand = \"/x\"\n", "shared-context", Path("/dev/sctx")
        )
        self.assertFalse(changed)

    def test_round_trip_retarget_then_trust(self) -> None:
        """A retargeted hooks.json plus the recomputed block verifies clean.

        This is the whole `--sctx-bin` contract in one assertion: rewrite the
        command, drop the operator's tables, append fresh ones, and Codex's own
        check (recompute and compare) then passes.
        """
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            real_hooks = root / "real" / "hooks.json"
            real_hooks.parent.mkdir()
            document = hooks_file(
                SessionStart=[
                    {"hooks": [command_hook("'/Users/x/.shared-context/bin/1/sctx' hook")]}
                ],
                SessionEnd=[
                    {
                        "hooks": [
                            command_hook(
                                "'/Users/x/.shared-context/bin/1/sctx' hook", timeout=30
                            )
                        ]
                    }
                ],
            )
            real_hooks.write_text(json.dumps(document), encoding="utf-8")
            real_entries = codex_trust.hook_state_entries(document, str(real_hooks))
            config = root / "config.toml"
            config.write_text(
                "".join(
                    f'[hooks.state."{key}"]\ntrusted_hash = "{value}"\n\n'
                    for key, value in real_entries.items()
                ),
                encoding="utf-8",
            )
            # The operator's own file verifies before anything is changed.
            self.assertEqual(len(codex_trust.verify_config(config)["matched"]), 2)

            target_hooks = root / "replay" / "hooks.json"
            target_hooks.parent.mkdir()
            retargeted, count = replay.retarget_sctx_hooks(
                codex_trust.load_hooks_file(real_hooks), Path("/dev/build/sctx")
            )
            self.assertEqual(count, 2)
            target_hooks.write_text(json.dumps(retargeted), encoding="utf-8")
            body, dropped = replay.drop_hook_state_tables(
                config.read_text(encoding="utf-8"), f"{real_hooks}:"
            )
            self.assertEqual(dropped, 2)
            body += codex_trust.render_hook_state(
                codex_trust.hook_state_entries(retargeted, str(target_hooks))
            )
            target_config = root / "replay" / "config.toml"
            target_config.write_text(body, encoding="utf-8")
            report = codex_trust.verify_config(target_config)
        self.assertEqual(len(report["matched"]), 2)
        self.assertEqual(report["mismatched"], [])
        self.assertEqual(report["sources_resolved"], [str(target_hooks)])


class BuildCodexHomeTests(unittest.TestCase):
    """Both trust routes, end to end, against a synthetic CODEX_HOME."""

    def _fake_codex_home(self, root: Path) -> Path:
        real = root / "real"
        real.mkdir()
        document = hooks_file(
            SessionStart=[
                {"hooks": [command_hook("'/Users/x/.shared-context/bin/current/sctx' hook")]}
            ]
        )
        (real / "hooks.json").write_text(json.dumps(document), encoding="utf-8")
        entries = codex_trust.hook_state_entries(document, str(real / "hooks.json"))
        body = (
            "[mcp_servers.shared-context]\n"
            'command = "/Users/x/.shared-context/bin/current/sctx"\n\n'
        ) + "".join(
            f'[hooks.state."{key}"]\ntrusted_hash = "{value}"\n\n'
            for key, value in entries.items()
        )
        (real / "config.toml").write_text(body, encoding="utf-8")
        (real / "auth.json").write_text("{}", encoding="utf-8")
        return real

    def test_without_sctx_bin_the_hooks_file_is_copied_byte_for_byte(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            real = self._fake_codex_home(root)
            warnings: list[str] = []
            record = replay.build_codex_home(root / "out", real, Path("/co"), warnings)
            self.assertEqual(warnings, [])
            self.assertEqual(record["trust"]["mode"], "path-prefix-rewrite")
            self.assertFalse(record["sctx_bin_retargeted"])
            self.assertFalse(record["mcp_command_retargeted"])
            self.assertEqual(
                (root / "out" / "hooks.json").read_bytes(), (real / "hooks.json").read_bytes()
            )
            report = codex_trust.verify_config(root / "out" / "config.toml")
            self.assertEqual(len(report["matched"]), 1)
            self.assertEqual(report["mismatched"], [])

    def test_with_sctx_bin_the_hooks_are_retargeted_and_retrusted(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            real = self._fake_codex_home(root)
            warnings: list[str] = []
            record = replay.build_codex_home(
                root / "out", real, Path("/co"), warnings, sctx_bin=Path("/dev/build/sctx")
            )
            self.assertEqual(warnings, [])
            self.assertEqual(record["trust"]["mode"], "recomputed")
            self.assertEqual(record["trust"]["operator_entries_reproduced"], "1/1")
            self.assertEqual(record["trust"]["state_keys_dropped"], 1)
            self.assertEqual(record["trust"]["hook_commands_retargeted"], 1)
            self.assertTrue(record["mcp_command_retargeted"])
            written = json.loads((root / "out" / "hooks.json").read_text(encoding="utf-8"))
            self.assertEqual(
                written["hooks"]["SessionStart"][0]["hooks"][0]["command"],
                "'/dev/build/sctx' hook",
            )
            config = (root / "out" / "config.toml").read_text(encoding="utf-8")
            self.assertIn('command = "/dev/build/sctx"', config)
            self.assertNotIn(str(real / "hooks.json"), config)
            report = codex_trust.verify_config(root / "out" / "config.toml")
            self.assertEqual(len(report["matched"]), 1)
            self.assertEqual(report["mismatched"], [])

    def test_a_changed_hash_algorithm_is_reported_not_silently_accepted(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            real = self._fake_codex_home(root)
            config = real / "config.toml"
            config.write_text(
                config.read_text(encoding="utf-8").replace("sha256:", "sha256:ff"),
                encoding="utf-8",
            )
            warnings: list[str] = []
            record = replay.build_codex_home(
                root / "out", real, Path("/co"), warnings, sctx_bin=Path("/dev/build/sctx")
            )
        self.assertEqual(record["trust"]["operator_entries_reproduced"], "0/1")
        self.assertTrue(any("reproduces only 0 of 1" in note for note in warnings))


if __name__ == "__main__":
    unittest.main()
