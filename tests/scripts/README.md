# Real-host smoke acceptance

Project rule U-002: any acceptance using a real Codex smoke must also include a real Cursor smoke on the same product build. Missing, failed or blocked Cursor evidence does not pass the gate. Fixture playback and simulated model responses cannot substitute for either real host. Match the coverage to the requested host form: CLI evidence does not prove desktop UI behavior.

Use the paired entry point with a signed local product binary:

```sh
SCTX_SKIP_LAUNCHCTL=1 python3 tests/scripts/real_host_smoke_pair.py \
  --sctx-bin target/refactor-install/lib/node_modules/@bytedance-dev/shared-context-darwin-arm64/bin/sctx \
  --output-dir /private/tmp/shared-context-real-host-pair-UNIQUE
```

The output directory must be fresh for a live run. This runs Cursor first, then the single-trial Codex probe, and requires both to pass. Existing authenticated Cursor/Codex clients and their normal network access are needed; no credentials are printed or copied. On macOS the private Cursor HOME links the existing `Library/Keychains` solely for Cursor's normal authentication. Keep raw output private; never commit/archive that symlink or the host HOME as a repository fixture. Production installation/configuration/Runtime are not upgraded by the smoke.

Cursor Agent CLI uses native SessionStart to activate the isolated installation; the harness bootstraps the test Task. The real model calls `task_checkpoint` and then `candidate_list`; the latter runs queued Builder recovery. Native SessionEnd must clear the lease. Reading only the isolated installation's Shared Context Skill/workflow is allowed, as is host tool-schema discovery. Other business calls/file access do not qualify.

Codex uses the existing real-model checkpoint legality probe. The paired driver subsequently supplies Stop, eight repeated Stops and model-less SessionEnd against that same isolated Task. These are explicitly recorded as **driver delivery, not native Codex hooks**. A missing Cursor native Stop is reported rather than invented; Builder coverage comes from its normal `candidate_list` recovery. Neither script claims desktop GUI coverage or normal-use quality samples.

The verifier compares installed binary hashes and checkpoint schemas, checks raw Codex model-call evidence, Cursor host/wire/native-hook records, Builder completion, repeated-close quietness and lease cleanup. Checks stay active under `python -O`; every invocation invalidates any previous green receipt before running, so a failed rerun cannot leave stale success.

For independently captured existing directories with Codex `lifecycle-smoke.json` already present:

```sh
python3 tests/scripts/real_host_smoke_pair.py --verify-only \
  --codex-dir /private/tmp/real-codex-evidence \
  --cursor-dir /private/tmp/real-cursor-evidence \
  --output-dir /private/tmp/paired-verification
```

`paired-summary.json` is the shared acceptance result. Retain failures separately and record versions, actual model labels, build hash, native versus driven events, and any unsupported host coverage. Standalone probes remain diagnostic tools; a Codex-only result cannot approve a release/milestone smoke gate.
