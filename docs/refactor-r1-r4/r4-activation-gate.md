# H-002 — Coordinate installed schema19 activation with live MCP clients

Status: awaiting user's activation-timing decision. Code gates are complete; production installation and observation baseline have not changed.

## Concrete reviewable result

- Product code through6e3204a, gate-only fixture repair1396a5d. All required R1–R4 code issues accepted. [Requirement audit](acceptance-audit.md) and [decision log](../decisions/refactor-r1-r4.md).
- Final workspace run:1052passed/1fixture failure/14ignored; the sole failure was a detached Git maintenance lock race, reproduced6/80 and repaired0/80. Full scenario-runner41passed and independent observer5passed after repair. All1053current tests have passing evidence; the original full run remains exit101. Finalfmt/clippy and NPM21passed/1platform skip.
- Signed local NPM build verified at `target/refactor-install/bin/sctx`; packaged Mach-O at `target/refactor-install/lib/node_modules/@bytedance-dev/shared-context-darwin-arm64/bin/sctx`.
- Real database rehearsal: all23old table projections retain row count and exact value hashes across18→19; all25usage rows become checkpoint_derived, all34historical relations remain unknown. Real16checkpoints/34Claims have all required empty tombstones and no nonempty retired field. Evidence `/private/tmp/sctx-m4-production-{preflight,rehearsal}/`.

## ROI-1 operational boundary

Nine live production MCP processes were observed: eight Codex and one Cursor, all launched through the existing installation. The currently installed binary was independently run against only the schema19 rehearsal copy and exited2 with `unsupported task runtime schema version 19; expected 18` (`/private/tmp/sctx-m4-old-reader-check.json`). Switching the shared live Runtime while old clients remain connected can break their calls until reconnection. Existing sessions were not terminated, and no live database or managed host configuration was changed.

The installer also correctly rejects different bytes in an existing version directory. Use the supported unique Runtime label `0.2.0-dev.8-r1-r4.6e3204a`; preserve the old binary directory and installer backups. This is an installation label, not a package version bump or published release.

## Proposed activation after approval

1. Install the verified local packages into the existing NPM prefix `/Users/bytedance/.nvm/versions/node/v22.15.0` using `node npm/scripts/install-local.js --prefix ...`.
2. Run the newly installed signed binary's upgrade with `--runtime-version 0.2.0-dev.8-r1-r4.6e3204a` and the exact new runtime source. Installer owns its transactional backup/migration/managed configuration update. All6currently installed skill files match their ownership hashes; no legacy Capture cleanup targets exist.
3. Coordinate the user's reconnection of Codex/Cursor Shared Context MCP clients. Verify actual installed commands, schema, managed guidance and unchanged Knowledge Git; record the post-activation baseline.
4. Observe normal use for>=2weeks OR>=20new sessions, excluding17historical Runtime sessions and every isolated test/rehearsal. Keep ranking/permissions fixed and produce the required B-entry report. No normal-use observation has started yet.

Question awaiting authority: activate the local installation now, with the user reconnecting the existing MCP clients afterward, or retain the current installation until a suitable time?

## Why this is a human gate

The invoked [review-gated-development skill](/Users/bytedance/.codex/skills/review-gated-development/SKILL.md) says: “Pause only when review evidence exposes a decision that requires human judgment or fresh authority”. It does not demand approval for routine milestone completion or every upgrade. The main agent judges coordination with nine existing live client connections to require the user's timing choice; forcing termination/restart of those clients is not inferred from the refactor request. Implementation/delegation pauses at this gate; the goal remains incomplete.

## U-002 supplement

The user additionally required real Cursor coverage whenever Codex real smoke is used. This independently authorized test/rule work is complete in cafe920; [paired evidence](paired-real-host-smoke.md) covers both real CLI hosts on the same signed product build. Future gates use AGENTS.md/DEVELOPMENT and the paired verifier. The follow-up did not approve production activation; H-002 remains pending. Existing production Runtime/table data was verified unchanged after both smokes.
