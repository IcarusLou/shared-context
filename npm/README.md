# Shared Context NPM packaging

This directory contains the publishable source for the internal packages:

- `@bytedance-dev/shared-context`: a thin JavaScript launcher.
- `@bytedance-dev/shared-context-darwin-arm64`: a signed arm64 Mach-O platform package.
- `@bytedance-dev/shared-context-darwin-x64`: a signed x64 Mach-O platform package.

There is no `postinstall` lifecycle script. Installing any package leaves Cursor and Codex
configuration untouched. The launcher selects the current Darwin CPU package, verifies the
packaged `sctx` SHA-256 and macOS code signature, and forwards argv, stdio, signals, and exit code.

All three packages declare the internal `@bytedance-dev` scope and the exact publish registry in
`publishConfig`: `https://bnpm.byted.org`.

## Install the published internal package

Install from the internal registry with optional dependencies enabled so NPM selects the matching
macOS platform package:

```bash
npm install --global @bytedance-dev/shared-context \
  --registry=https://bnpm.byted.org
```

This only installs the command. Run `sctx setup` explicitly when Agent configuration should change.

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

## One-command local install

For end-to-end testing on the current Mac, build the Rust CLI from source, ad-hoc sign it, pack the
launcher and matching platform package, and install both local tgz files with:

```bash
cd npm
npm run install:local
```

The command defaults to a release build and the repository-local `target/npm-local` NPM prefix. It
prints the absolute `sctx` path and a `PATH` export after verifying the installed binary checksum,
code signature, and version. The NPM install uses an empty temporary cache in offline mode, ignores
lifecycle scripts, and does not run `sctx setup` or change Cursor/Codex configuration.

Use a debug build or a different dedicated prefix when needed:

```bash
npm run install:local -- --profile debug --prefix /absolute/path/to/prefix
```

## Test the exact pending release

To build all three tarballs that the release job would publish, install the current Mac's two
matching tarballs, and verify the checksum, code signature, and CLI version:

```bash
rustup target add aarch64-apple-darwin x86_64-apple-darwin
cd npm
npm run release:test
```

The release inputs are written under `target/npm-release`; the isolated test installation uses
`target/npm-release-test`. Neither command uploads a package or runs `sctx setup`. To reuse
upstream-signed binaries instead of source-building temporary signed copies:

```bash
npm run release:prepare -- \
  --arm64-binary /absolute/path/to/signed/arm64/sctx \
  --x64-binary /absolute/path/to/signed/x64/sctx
npm run install:release
```

## Publish an internal release

The three NPM package versions, the launcher's exact optional dependency versions, and
`workspace.package.version` in the root `Cargo.toml` must match. Stable versions use the `latest`
dist-tag; prerelease versions such as `1.2.0-beta.0` use the corresponding `beta` tag.

The formal path is tag-driven:

1. Run `cd npm && npm run release:version -- 1.2.0` (or the intended prerelease SemVer).
2. Review and commit the synchronized Cargo, Cargo.lock, NPM package, and package-lock changes.
3. Run `npm test && npm run release:test`; the installed `sctx --version` must match.
4. Create and push the matching tag, for example `v1.2.0` or `v1.2.0-beta.0`.
5. Approve the protected `internal-npm` environment when prompted.

`.github/workflows/publish-npm.yml` rebuilds the release, installs the exact tarballs, preserves the
manifest and checksums as CI evidence, authenticates with the environment's `BNPM_TOKEN` secret,
and publishes in this fixed order:

1. `@bytedance-dev/shared-context-darwin-arm64`
2. `@bytedance-dev/shared-context-darwin-x64`
3. `@bytedance-dev/shared-context`

The workflow sets a stable `CI_NAME=allow_same_version_<run-id>` so a retry of the same CI release
can use bnpm's same-operation retry behavior without changing tarball identity. The publish command
also verifies the Git tag, version, dist-tag, registry, artifact checksums, and authenticated user
before uploading anything.

For an authorized local recovery, use previously prepared artifacts and confirm the version
explicitly:

```bash
npm run release:publish -- --confirm-version 1.2.0
```

`--confirm-version` is intentionally a non-mutating safety check; it never rewrites source or
rebuilds a binary during upload. Change the version first with `release:version`, then run
`release:test` so Cargo recompiles `env!("CARGO_PKG_VERSION")` into the CLI before publishing.

## Offline install

After transferring the matching archive to the target Mac:

```bash
tar -xzf shared-context-0.1.0-darwin-arm64-offline.tar.gz
./shared-context-0.1.0-darwin-arm64-offline/install \
  --agents cursor,codex \
  --knowledge-store-url git@github.example.com:team/shared-context.git \
  --yes
```

`install` verifies `SHA256SUMS`, runs npm in offline mode with lifecycle scripts disabled, and then
always invokes `sctx setup` with the supplied options. Calling `./install` with no options therefore
still starts setup with its defaults; callers must not include an extra `setup` argument.
The optional Knowledge Store URL is forwarded unchanged as an argv value; Setup rejects embedded
credentials and never prints or stores the raw URL in its install manifest.

After teammates merge an installation work branch into the protected default branch, explicitly
receive it and publish this installation's own proposal branch with:

```bash
sctx knowledge sync
```

The thin launcher forwards this command unchanged. Shared Context does not create or merge a Pull
Request automatically.

Do not pass `--demo` as an install acceptance check. Mew #195 records a human-accepted, non-core
known limitation: offline `setup --demo` has no authorized Agent Session lease, so its public MCP
search is rejected by the Server guard. Normal install/setup, Hook activation, Skill loading, and
authorized MCP workflows remain covered.

## Tests

```bash
cd npm
npm test
```

On an arm64 Mac the suite runs the real signed `sctx` arm64 package through offline packaging and
install coverage. Exactly the Mew #195 `setup --demo` smoke is explicitly skipped. The x64 suite
cross-builds and signs a real x86_64 Mach-O, then proves package/bundle structure and npm CPU
contracts only; native x64 execution remains `NOT_PROVEN` until run on Intel hardware.
