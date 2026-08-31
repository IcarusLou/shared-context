#!/usr/bin/env node

'use strict';

const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const { run } = require('./lib/packaging');
const {
  DEFAULT_ARTIFACTS,
  REGISTRY,
  publishArguments,
  readReleaseManifest,
  recommendedDistTag,
  validateDistTag,
  validateReleaseMetadata,
} = require('./lib/release');

function usage() {
  return `Publish a prepared Shared Context release to the internal NPM registry.

Usage:
  node npm/scripts/publish-release.js [options]

Options:
  --artifacts PATH          Prepared artifact directory (default: target/npm-release)
  --tag NAME                NPM dist-tag (default: latest, or the SemVer prerelease label)
  --confirm-version VERSION Required outside a v<VERSION> CI tag build
  --dry-run                  Run npm publish --dry-run without uploading
  -h, --help                 Show this help

The two platform packages are always published before the launcher package. Authentication comes
from the caller's NPM configuration; the script never reads or writes a token.
`;
}

function parseOptions(argv, cwd = process.cwd()) {
  const options = { artifactsDirectory: DEFAULT_ARTIFACTS, dryRun: false };
  const mappings = {
    '--artifacts': 'artifactsDirectory',
    '--tag': 'tag',
    '--confirm-version': 'confirmVersion',
  };
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index];
    if (argument === '-h' || argument === '--help') {
      options.help = true;
      continue;
    }
    if (argument === '--dry-run') {
      options.dryRun = true;
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
    options[property] = property === 'artifactsDirectory' ? path.resolve(cwd, value) : value;
  }
  return options;
}

function ciTagVersion(environment = process.env) {
  if (environment.GITHUB_REF_TYPE !== 'tag' || !environment.GITHUB_REF_NAME) {
    return undefined;
  }
  return environment.GITHUB_REF_NAME.startsWith('v')
    ? environment.GITHUB_REF_NAME.slice(1)
    : undefined;
}

function publishEnvironment(environment, cache) {
  return {
    ...environment,
    npm_config_audit: 'false',
    npm_config_cache: cache,
    npm_config_fund: 'false',
    npm_config_update_notifier: 'false',
  };
}

function publishRelease({
  artifactsDirectory = DEFAULT_ARTIFACTS,
  tag,
  confirmVersion,
  dryRun = false,
  environment = process.env,
} = {}) {
  const { version } = validateReleaseMetadata();
  const confirmed = confirmVersion || ciTagVersion(environment);
  if (!confirmed) {
    throw new Error(
      `Refusing to publish without --confirm-version ${version} or a v${version} CI tag`,
    );
  }
  if (confirmed !== version) {
    throw new Error(
      `Confirmed version ${confirmed} does not match source version ${version}. ` +
        `Set it first with: npm run release:version -- ${confirmed}`,
    );
  }

  const distTag = validateDistTag(version, tag || recommendedDistTag(version));
  const artifacts = path.resolve(artifactsDirectory);
  const manifest = readReleaseManifest(artifacts, version);
  const temporary = fs.mkdtempSync(path.join(os.tmpdir(), 'shared-context-release-publish-'));
  try {
    const npmEnvironment = publishEnvironment(environment, path.join(temporary, 'npm-cache'));
    if (!dryRun) {
      const user = run('npm', ['whoami', '--registry', REGISTRY], { env: npmEnvironment });
      process.stdout.write(`Authenticated to ${REGISTRY} as ${user}.\n`);
    }

    for (const item of manifest.packages) {
      const tarball = path.join(artifacts, item.file);
      process.stdout.write(
        `${dryRun ? 'Checking' : 'Publishing'} ${item.name}@${item.version} (${distTag})...\n`,
      );
      run('npm', publishArguments(tarball, distTag, dryRun), {
        env: npmEnvironment,
        stdio: 'inherit',
      });
    }
  } finally {
    fs.rmSync(temporary, { recursive: true, force: true });
  }
  process.stdout.write(
    dryRun
      ? `\nDry run passed for all packages; nothing was uploaded to ${REGISTRY}.\n`
      : `\nPublished all packages to ${REGISTRY} with dist-tag ${distTag}.\n`,
  );
  return { distTag, dryRun, manifest };
}

function main(argv = process.argv.slice(2)) {
  const options = parseOptions(argv);
  if (options.help) {
    process.stdout.write(usage());
    return;
  }
  publishRelease(options);
}

if (require.main === module) {
  try {
    main();
  } catch (error) {
    process.stderr.write(`publish-release: ${error.message}\n`);
    process.exitCode = 1;
  }
}

module.exports = {
  ciTagVersion,
  main,
  parseOptions,
  publishEnvironment,
  publishRelease,
  usage,
};
