#!/usr/bin/env node

'use strict';

const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const {
  PACKAGES_ROOT,
  buildPlatformPackage,
  npmEnvironment,
  npmPack,
  run,
  sha256File,
  targetFor,
} = require('./lib/packaging');

const REPOSITORY_ROOT = path.resolve(__dirname, '../..');
const DEFAULT_PREFIX = path.join(REPOSITORY_ROOT, 'target/npm-local');

function usage() {
  return `Build Shared Context from source and install its local NPM packages.

Usage:
  node npm/scripts/install-local.js [--prefix PATH] [--profile release|debug]

Options:
  --prefix PATH             Dedicated NPM prefix (default: target/npm-local)
  --profile release|debug   Cargo build profile (default: release)
  -h, --help                Show this help

The install is offline, ignores lifecycle scripts, and does not run sctx setup.
`;
}

function parseOptions(argv, cwd = process.cwd()) {
  const options = {
    prefix: DEFAULT_PREFIX,
    profile: 'release',
  };
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index];
    if (argument === '-h' || argument === '--help') {
      options.help = true;
      continue;
    }
    if (argument !== '--prefix' && argument !== '--profile') {
      throw new Error(`Unknown argument ${argument}\n\n${usage()}`);
    }
    const value = argv[index + 1];
    if (!value || value.startsWith('--')) {
      throw new Error(`Missing value for ${argument}`);
    }
    index += 1;
    if (argument === '--prefix') {
      options.prefix = path.resolve(cwd, value);
    } else {
      options.profile = value;
    }
  }
  if (!['debug', 'release'].includes(options.profile)) {
    throw new Error(`Unsupported profile ${options.profile}; expected debug or release`);
  }
  return options;
}

function hostTarget(platform = process.platform, architecture = process.arch) {
  if (platform !== 'darwin') {
    throw new Error(`Local NPM packages only support macOS; detected ${platform}/${architecture}`);
  }
  const target = targetFor(architecture);
  return {
    ...target,
    rustTarget: architecture === 'arm64' ? 'aarch64-apple-darwin' : 'x86_64-apple-darwin',
  };
}

function installArguments({ prefix, cache, mainTarball, platformTarball }) {
  return [
    'install',
    '--global',
    '--prefix',
    prefix,
    '--offline',
    '--ignore-scripts',
    '--omit=optional',
    '--no-audit',
    '--no-fund',
    '--cache',
    cache,
    mainTarball,
    platformTarball,
  ];
}

function shellQuote(value) {
  return `'${value.replaceAll("'", `'"'"'`)}'`;
}

function installLocal({ prefix = DEFAULT_PREFIX, profile = 'release' } = {}) {
  const target = hostTarget();
  const temporary = fs.mkdtempSync(path.join(os.tmpdir(), 'shared-context-local-install-'));
  try {
    const cargoArguments = [
      'build',
      '--locked',
      '--bin',
      'sctx',
      '--target',
      target.rustTarget,
      '--target-dir',
      path.join(REPOSITORY_ROOT, 'target'),
    ];
    if (profile === 'release') {
      cargoArguments.push('--release');
    }
    process.stdout.write(
      `[1/4] Building sctx (${profile}, ${target.rustTarget}) from source...\n`,
    );
    run('cargo', cargoArguments, { cwd: REPOSITORY_ROOT, stdio: 'inherit' });

    const unsignedBinary = path.join(
      REPOSITORY_ROOT,
      'target',
      target.rustTarget,
      profile,
      'sctx',
    );
    const signedBinary = path.join(temporary, 'signed/sctx');
    fs.mkdirSync(path.dirname(signedBinary), { recursive: true });
    fs.copyFileSync(unsignedBinary, signedBinary);
    fs.chmodSync(signedBinary, 0o755);
    process.stdout.write('[2/4] Applying and verifying an ad-hoc local code signature...\n');
    run('/usr/bin/codesign', ['--force', '--sign', '-', signedBinary]);

    const artifacts = path.join(temporary, 'artifacts');
    process.stdout.write('[3/4] Packing the launcher and current-platform NPM packages...\n');
    const platform = buildPlatformPackage({
      arch: target.arch,
      binary: signedBinary,
      outputDirectory: artifacts,
    });
    const packedMain = npmPack(
      path.join(PACKAGES_ROOT, 'shared-context'),
      artifacts,
      path.join(temporary, 'npm-cache-pack-main'),
    );
    const mainTarball = path.join(artifacts, 'shared-context.tgz');
    fs.renameSync(packedMain, mainTarball);

    const installCache = path.join(temporary, 'npm-cache-install');
    process.stdout.write(`[4/4] Installing local tgz packages into ${prefix}...\n`);
    fs.mkdirSync(prefix, { recursive: true });
    run(
      'npm',
      installArguments({
        prefix,
        cache: installCache,
        mainTarball,
        platformTarball: platform.tarball,
      }),
      {
        cwd: REPOSITORY_ROOT,
        env: npmEnvironment(installCache),
        stdio: 'inherit',
      },
    );

    const installedBinary = path.join(
      prefix,
      'lib/node_modules',
      target.packageName,
      'bin/sctx',
    );
    const expectedSha256 = platform.manifest.binary.sha256;
    const installedSha256 = sha256File(installedBinary);
    if (installedSha256 !== expectedSha256) {
      throw new Error(
        `Installed binary SHA-256 mismatch: expected ${expectedSha256}, got ${installedSha256}`,
      );
    }
    run('/usr/bin/codesign', ['--verify', '--strict', '--verbose=2', installedBinary]);
    const command = path.join(prefix, 'bin/sctx');
    const version = run(command, ['--version']);
    const result = { command, installedBinary, prefix, profile, target, version };

    process.stdout.write(`\nLocal install verified: ${version}\n`);
    process.stdout.write(`Run directly: ${shellQuote(command)}\n`);
    process.stdout.write(
      `Add for this shell: export PATH=${shellQuote(path.dirname(command))}:"$PATH"\n`,
    );
    process.stdout.write('No setup was run and no Agent configuration was changed.\n');
    return result;
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
  installLocal(options);
}

if (require.main === module) {
  try {
    main();
  } catch (error) {
    process.stderr.write(`install-local: ${error.message}\n`);
    process.exitCode = 1;
  }
}

module.exports = {
  DEFAULT_PREFIX,
  hostTarget,
  installArguments,
  installLocal,
  main,
  parseOptions,
  usage,
};
