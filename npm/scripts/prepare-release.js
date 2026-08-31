#!/usr/bin/env node

'use strict';

const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const {
  PACKAGES_ROOT,
  buildPlatformPackage,
  npmPack,
  run,
} = require('./lib/packaging');
const {
  DEFAULT_ARTIFACTS,
  validateReleaseMetadata,
  writeReleaseManifest,
} = require('./lib/release');

const REPOSITORY_ROOT = path.resolve(__dirname, '../..');
const RUST_TARGETS = Object.freeze({
  arm64: 'aarch64-apple-darwin',
  x64: 'x86_64-apple-darwin',
});

function usage() {
  return `Build the complete, publishable Shared Context NPM release.

Usage:
  node npm/scripts/prepare-release.js [options]

Options:
  --output-dir PATH          Artifact directory (default: target/npm-release)
  --arm64-binary PATH        Explicit pre-signed arm64 sctx binary
  --x64-binary PATH          Explicit pre-signed x64 sctx binary
  --signing-identity VALUE   codesign identity for source builds (default: ad-hoc '-';
                             SCTX_CODESIGN_IDENTITY is also supported)
  -h, --help                 Show this help

Pass both binary options together to package upstream-signed binaries. With neither option, the
script cross-builds both release binaries and signs temporary copies before packaging.
`;
}

function parseOptions(argv, cwd = process.cwd(), environment = process.env) {
  const options = {
    outputDirectory: DEFAULT_ARTIFACTS,
    signingIdentity: environment.SCTX_CODESIGN_IDENTITY || '-',
  };
  const mappings = {
    '--output-dir': 'outputDirectory',
    '--arm64-binary': 'arm64Binary',
    '--x64-binary': 'x64Binary',
    '--signing-identity': 'signingIdentity',
  };
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index];
    if (argument === '-h' || argument === '--help') {
      options.help = true;
      continue;
    }
    const property = mappings[argument];
    if (!property) {
      throw new Error(`Unknown argument ${argument}\n\n${usage()}`);
    }
    const value = argv[index + 1];
    if (!value || value.startsWith('--')) {
      throw new Error(`Missing value for ${argument}`);
    }
    index += 1;
    options[property] = property === 'signingIdentity' ? value : path.resolve(cwd, value);
  }
  if (Boolean(options.arm64Binary) !== Boolean(options.x64Binary)) {
    throw new Error('--arm64-binary and --x64-binary must be provided together');
  }
  return options;
}

function buildAndSignBinaries(temporary, signingIdentity) {
  const binaries = {};
  for (const [arch, rustTarget] of Object.entries(RUST_TARGETS)) {
    process.stdout.write(`[build] Compiling sctx release for ${rustTarget}...\n`);
    run(
      'cargo',
      [
        'build',
        '--locked',
        '--release',
        '--bin',
        'sctx',
        '--target',
        rustTarget,
        '--target-dir',
        path.join(REPOSITORY_ROOT, 'target'),
      ],
      { cwd: REPOSITORY_ROOT, stdio: 'inherit' },
    );
    const source = path.join(REPOSITORY_ROOT, 'target', rustTarget, 'release/sctx');
    const signed = path.join(temporary, `signed-${arch}/sctx`);
    fs.mkdirSync(path.dirname(signed), { recursive: true });
    fs.copyFileSync(source, signed);
    fs.chmodSync(signed, 0o755);
    run('/usr/bin/codesign', ['--force', '--sign', signingIdentity, signed]);
    binaries[arch] = signed;
  }
  return binaries;
}

function prepareRelease({
  outputDirectory = DEFAULT_ARTIFACTS,
  arm64Binary,
  x64Binary,
  signingIdentity = '-',
} = {}) {
  const { version } = validateReleaseMetadata();
  const output = path.resolve(outputDirectory);
  const temporary = fs.mkdtempSync(path.join(os.tmpdir(), 'shared-context-release-'));
  try {
    const stage = path.join(temporary, 'artifacts');
    fs.mkdirSync(stage, { recursive: true });
    const binaries = arm64Binary && x64Binary
      ? { arm64: path.resolve(arm64Binary), x64: path.resolve(x64Binary) }
      : buildAndSignBinaries(temporary, signingIdentity);

    for (const arch of ['arm64', 'x64']) {
      process.stdout.write(`[pack] Creating ${arch} platform package...\n`);
      buildPlatformPackage({ arch, binary: binaries[arch], outputDirectory: stage });
    }
    process.stdout.write('[pack] Creating launcher package...\n');
    const packedMain = npmPack(
      path.join(PACKAGES_ROOT, 'shared-context'),
      stage,
      path.join(temporary, 'npm-cache-main'),
    );
    fs.renameSync(packedMain, path.join(stage, 'shared-context.tgz'));
    const manifest = writeReleaseManifest(stage, version);

    fs.mkdirSync(output, { recursive: true });
    fs.cpSync(stage, output, { recursive: true, force: true });
    process.stdout.write(`\nPrepared @bytedance-dev/shared-context@${version} in ${output}\n`);
    process.stdout.write('No package was uploaded. Run npm run install:release to test these tarballs.\n');
    return { manifest, outputDirectory: output };
  } finally {
    fs.rmSync(temporary, { recursive: true, force: true });
  }
}

function main(argv = process.argv.slice(2)) {
  const options = parseOptions(argv);
  if (options.help) {
    process.stdout.write(usage());
    return;
  }
  prepareRelease(options);
}

if (require.main === module) {
  try {
    main();
  } catch (error) {
    process.stderr.write(`prepare-release: ${error.message}\n`);
    process.exitCode = 1;
  }
}

module.exports = {
  RUST_TARGETS,
  buildAndSignBinaries,
  main,
  parseOptions,
  prepareRelease,
  usage,
};
