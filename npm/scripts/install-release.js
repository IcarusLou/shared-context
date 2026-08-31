#!/usr/bin/env node

'use strict';

const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const { hostTarget, installArguments } = require('./install-local');
const { npmEnvironment, run, sha256File } = require('./lib/packaging');
const {
  DEFAULT_ARTIFACTS,
  MAIN_PACKAGE,
  readReleaseManifest,
  validateReleaseMetadata,
} = require('./lib/release');

const REPOSITORY_ROOT = path.resolve(__dirname, '../..');
const DEFAULT_PREFIX = path.join(REPOSITORY_ROOT, 'target/npm-release-test');

function usage() {
  return `Install and verify the prepared NPM release for this Mac.

Usage:
  node npm/scripts/install-release.js [--artifacts PATH] [--prefix PATH]

Options:
  --artifacts PATH   Prepared artifact directory (default: target/npm-release)
  --prefix PATH      Dedicated NPM prefix (default: target/npm-release-test)
  -h, --help         Show this help

The install is offline, ignores lifecycle scripts, and never runs sctx setup.
`;
}

function parseOptions(argv, cwd = process.cwd()) {
  const options = {
    artifactsDirectory: DEFAULT_ARTIFACTS,
    prefix: DEFAULT_PREFIX,
  };
  const mappings = {
    '--artifacts': 'artifactsDirectory',
    '--prefix': 'prefix',
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
    options[property] = path.resolve(cwd, value);
  }
  return options;
}

function installRelease({
  artifactsDirectory = DEFAULT_ARTIFACTS,
  prefix = DEFAULT_PREFIX,
} = {}) {
  const target = hostTarget();
  const { version } = validateReleaseMetadata();
  const artifacts = path.resolve(artifactsDirectory);
  const manifest = readReleaseManifest(artifacts, version);
  const main = manifest.packages.find((item) => item.name === MAIN_PACKAGE);
  const platform = manifest.packages.find((item) => item.name === target.packageName);
  if (!main || !platform) {
    throw new Error(`Release manifest does not support ${process.platform}/${process.arch}`);
  }

  const temporary = fs.mkdtempSync(path.join(os.tmpdir(), 'shared-context-release-install-'));
  try {
    const cache = path.join(temporary, 'npm-cache');
    fs.mkdirSync(prefix, { recursive: true });
    run(
      'npm',
      installArguments({
        prefix: path.resolve(prefix),
        cache,
        mainTarball: path.join(artifacts, main.file),
        platformTarball: path.join(artifacts, platform.file),
      }),
      {
        cwd: REPOSITORY_ROOT,
        env: npmEnvironment(cache),
        stdio: 'inherit',
      },
    );

    const installedBinary = path.join(
      path.resolve(prefix),
      'lib/node_modules',
      target.packageName,
      'bin/sctx',
    );
    const checksum = fs.readFileSync(`${installedBinary}.sha256`, 'utf8');
    const match = /^([a-f0-9]{64})  sctx\n?$/.exec(checksum);
    if (!match || sha256File(installedBinary) !== match[1]) {
      throw new Error(`Installed binary SHA-256 verification failed: ${installedBinary}`);
    }
    run('/usr/bin/codesign', ['--verify', '--strict', '--verbose=2', installedBinary]);
    const command = path.join(path.resolve(prefix), 'bin/sctx');
    const installedVersion = run(command, ['--version']);
    if (installedVersion !== `sctx ${version}`) {
      throw new Error(
        `Installed CLI version does not match package version ${version}: ${installedVersion}`,
      );
    }

    process.stdout.write(`\nRelease install verified: ${installedVersion}\n`);
    process.stdout.write(`Command: ${command}\n`);
    process.stdout.write('No setup was run and no Agent configuration was changed.\n');
    return { command, installedBinary, manifest, prefix: path.resolve(prefix) };
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
  installRelease(options);
}

if (require.main === module) {
  try {
    main();
  } catch (error) {
    process.stderr.write(`install-release: ${error.message}\n`);
    process.exitCode = 1;
  }
}

module.exports = {
  DEFAULT_PREFIX,
  installRelease,
  main,
  parseOptions,
  usage,
};
