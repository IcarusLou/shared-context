# Shared Context NPM packaging

This directory contains the unpublished source for:

- `@company/shared-context`: a thin JavaScript launcher.
- `@company/shared-context-darwin-arm64`: a signed arm64 Mach-O platform package.
- `@company/shared-context-darwin-x64`: a signed x64 Mach-O platform package.

There is no `postinstall` lifecycle script. Installing any package leaves Cursor and Codex
configuration untouched. The launcher selects the current Darwin CPU package, verifies the
packaged `sctx` SHA-256 and macOS code signature, and forwards argv, stdio, signals, and exit code.

## Build local artifacts

Both builders require an explicit path to an already-signed, thin binary. They validate the code
signature and Mach-O architecture before packing and never publish or upload anything.

```bash
node npm/scripts/build-platform-package.js \
  --arch arm64 \
  --binary /absolute/path/to/signed/arm64/sctx \
  --output-dir /absolute/path/to/artifacts

node npm/scripts/build-offline-bundle.js \
  --arch arm64 \
  --binary /absolute/path/to/signed/arm64/sctx \
  --output-dir /absolute/path/to/artifacts
```

Use `--arch x64` with a signed, thin x86_64 binary for the Intel artifacts. Each offline builder
output contains the main tgz and exactly one platform tgz, a local-only root package/lock,
`MANIFEST.json`, `SHA256SUMS`, an `install` entrypoint, and a reproducible `.tar.gz` archive.

## Offline install

After transferring the matching archive to the target Mac:

```bash
tar -xzf shared-context-0.1.0-darwin-arm64-offline.tar.gz
./shared-context-0.1.0-darwin-arm64-offline/install \
  --agents cursor,codex --yes
```

`install` verifies `SHA256SUMS`, runs npm in offline mode with lifecycle scripts disabled, and then
always invokes `sctx setup` with the supplied options. Calling `./install` with no options therefore
still starts setup with its defaults; callers must not include an extra `setup` argument.

## Agent Skill installation

NPM installation itself still does not change Agent configuration or install Skills. Explicit
`sctx setup` / `sctx upgrade` installs the instruction-only Shared Context Skill at
`~/.agents/skills/shared-context/`, shared by Cursor and Codex, together with the MCP and Hook
registrations. Setup owns only the exact Skill files and hashes recorded in its manifest:

- repeated setup is byte-stable and does not duplicate the Skill;
- upgrade replaces only an unchanged product-owned Skill;
- a pre-existing same-name Skill or a user-modified installed Skill is preserved with a warning;
- setup rollback restores the prior bytes and permissions; and
- uninstall removes only files that still match the recorded product hashes.

The installed Skill instructs a supporting Agent to retrieve context for substantive engineering
tasks and to propose evidence-backed reusable findings only as Candidates. It cannot review or
publish through the Shared Context MCP tool surface. Implicit Skill invocation depends on the Agent
runtime and is not guaranteed; Hooks remain the deterministic fallback for baseline injection and
Breadcrumb capture.

`context_for_task` and `context_propose` can route a current `workspace` through its exact local
WorkspaceBinding; an explicit `space_id` takes precedence. An unbound or missing route fails closed
instead of guessing or searching across Spaces. Workspace paths remain non-authoritative query hints
and are not persisted or echoed in results.

Proposal idempotency is deliberately strict: only complete authoritative Drafts that are exactly
equal field by field within the same ContextSpace are duplicates. Any authoritative field difference
creates a distinct Candidate. FTS rank, embeddings, paraphrase similarity, or topic overlap must
never automatically merge or suppress a proposal. The check covers existing revisions in every
lifecycle state. An exact retry returns `deduplicated: true`, `status: existing`, and the existing
Event/Context/Revision IDs without creating a new Event or returning a new Batch/Commit ID.

Automated tests prove the installed bytes, MCP contracts, routing and Candidate-only writes. Real
Cursor/Codex implicit invocation in a natural task remains `NOT_PROVEN` until exercised in a
supported live Agent session with a trace of the expected MCP Tool Call.

## Tests

```bash
cd npm
npm test
```

On an arm64 Mac the suite runs the real signed `sctx` arm64 package through an offline install and
setup smoke, including the globally installed Skill assets. The x64 suite cross-builds and signs a
real x86_64 Mach-O, then proves package/bundle structure and npm CPU contracts only; native x64
execution remains `NOT_PROVEN` until run on Intel hardware. Neither package smoke nor static Skill
validation proves that a real Agent implicitly invoked the Skill; that remains `NOT_PROVEN` unless
an end-to-end Agent trace records the expected MCP Tool Call.
