# Repository test policy

When acceptance includes a real Codex smoke test, it must also include a real Cursor smoke test on the same product build. This user requirement (U-002, 2026-09-09) supersedes older plans that permitted either host alone.

- Use `tests/scripts/real_host_smoke_pair.py`; see `tests/scripts/README.md`.
- Missing, failed or blocked Cursor evidence means the paired gate has not passed.
- Fixture replay, hand-authored hook payloads and simulated model responses are not real-host evidence.
- Record host versions/models/build hashes and distinguish native hooks from driver-delivered checks. CLI evidence does not establish desktop UI coverage.
- Keep smoke installations isolated and exclude their sessions from normal-use observation samples.
