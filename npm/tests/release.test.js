'use strict';

const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const test = require('node:test');

const { parseOptions: parsePrepareOptions } = require('../scripts/prepare-release');
const {
  parseOptions: parseVersionOptions,
  setReleaseVersion,
} = require('../scripts/set-release-version');
const {
  ciTagVersion,
  parseOptions: parsePublishOptions,
  publishEnvironment,
} = require('../scripts/publish-release');
const {
  DEFAULT_ARTIFACTS,
  MAIN_PACKAGE,
  PLATFORM_PACKAGES,
  REGISTRY,
  packageDefinitions,
  publishArguments,
  readReleaseManifest,
  recommendedDistTag,
  validateDistTag,
  validateReleaseMetadata,
  workspaceVersion,
  writeReleaseManifest,
} = require('../scripts/lib/release');

test('release metadata uses the internal scope, registry, and one synchronized version', () => {
  const release = validateReleaseMetadata();
  assert.equal(release.version, workspaceVersion());
  assert.deepEqual(
    release.definitions.map((item) => item.name),
    [PLATFORM_PACKAGES.arm64, PLATFORM_PACKAGES.x64, MAIN_PACKAGE],
  );
  for (const item of release.packages) {
    assert.ok(item.metadata.name.startsWith('@bytedance-dev/'));
    assert.equal(item.metadata.publishConfig.registry, REGISTRY);
    assert.equal(item.metadata.version, release.version);
  }
});

test('dist-tags must correspond to stable and prerelease SemVer versions', () => {
  assert.equal(recommendedDistTag('1.2.3'), 'latest');
  assert.equal(recommendedDistTag('1.2.3-alpha.4'), 'alpha');
  assert.equal(recommendedDistTag('1.2.3-rc.1'), 'rc');
  assert.equal(validateDistTag('1.2.3', 'latest'), 'latest');
  assert.equal(validateDistTag('1.2.3-beta.0', 'beta'), 'beta');
  assert.throws(() => validateDistTag('1.2.3-beta.0', 'latest'), /must use dist-tag beta/);
  assert.throws(() => validateDistTag('1.2.3', 'next'), /must use dist-tag latest/);
});

test('release manifest fixes platform-first publish order and detects tarball changes', (context) => {
  const temporary = fs.mkdtempSync(path.join(os.tmpdir(), 'sctx-release-manifest-'));
  context.after(() => fs.rmSync(temporary, { recursive: true, force: true }));
  for (const definition of packageDefinitions()) {
    fs.writeFileSync(path.join(temporary, definition.file), definition.name);
  }
  const written = writeReleaseManifest(temporary, '1.2.3');
  assert.deepEqual(
    written.packages.map((item) => item.name),
    [PLATFORM_PACKAGES.arm64, PLATFORM_PACKAGES.x64, MAIN_PACKAGE],
  );
  assert.deepEqual(written.packages.map((item) => item.publishOrder), [1, 2, 3]);
  assert.deepEqual(readReleaseManifest(temporary, '1.2.3'), written);

  fs.appendFileSync(path.join(temporary, 'shared-context.tgz'), 'tampered');
  assert.throws(() => readReleaseManifest(temporary, '1.2.3'), /SHA-256 mismatch/);
});

test('publish arguments pin the internal registry and disable lifecycle scripts', () => {
  assert.deepEqual(publishArguments('/tmp/package.tgz', 'latest'), [
    'publish',
    '/tmp/package.tgz',
    '--registry',
    REGISTRY,
    '--tag',
    'latest',
    '--ignore-scripts',
  ]);
  const dryRun = publishArguments('/tmp/package.tgz', 'beta', true);
  assert.ok(dryRun.includes('--dry-run'));
  assert.ok(dryRun.includes('--offline'));
});

test('release CLIs require paired binaries and accept a matching CI version tag', () => {
  assert.deepEqual(parsePrepareOptions([], '/tmp', {}), {
    outputDirectory: DEFAULT_ARTIFACTS,
    signingIdentity: '-',
  });
  assert.throws(
    () => parsePrepareOptions(['--arm64-binary', 'sctx'], '/tmp', {}),
    /must be provided together/,
  );
  assert.deepEqual(
    parsePublishOptions(['--artifacts', 'artifacts', '--tag', 'beta', '--dry-run'], '/tmp'),
    {
      artifactsDirectory: path.join('/tmp', 'artifacts'),
      dryRun: true,
      tag: 'beta',
    },
  );
  assert.equal(
    ciTagVersion({ GITHUB_REF_TYPE: 'tag', GITHUB_REF_NAME: 'v1.2.3-beta.0' }),
    '1.2.3-beta.0',
  );
  assert.equal(ciTagVersion({ GITHUB_REF_TYPE: 'branch', GITHUB_REF_NAME: 'main' }), undefined);
  assert.deepEqual(publishEnvironment({ PATH: '/bin' }, '/tmp/npm-cache'), {
    PATH: '/bin',
    npm_config_audit: 'false',
    npm_config_cache: '/tmp/npm-cache',
    npm_config_fund: 'false',
    npm_config_update_notifier: 'false',
  });
});

test('release version command synchronizes Cargo, lockfiles, packages, and optional dependencies', (context) => {
  const repository = path.resolve(__dirname, '../..');
  const temporary = fs.mkdtempSync(path.join(os.tmpdir(), 'sctx-release-version-'));
  context.after(() => fs.rmSync(temporary, { recursive: true, force: true }));
  fs.copyFileSync(path.join(repository, 'Cargo.toml'), path.join(temporary, 'Cargo.toml'));
  fs.copyFileSync(path.join(repository, 'Cargo.lock'), path.join(temporary, 'Cargo.lock'));
  fs.mkdirSync(path.join(temporary, 'crates'));
  for (const entry of fs.readdirSync(path.join(repository, 'crates'), { withFileTypes: true })) {
    const source = path.join(repository, 'crates', entry.name, 'Cargo.toml');
    if (!entry.isDirectory() || !fs.existsSync(source)) {
      continue;
    }
    const destination = path.join(temporary, 'crates', entry.name);
    fs.mkdirSync(destination);
    fs.copyFileSync(source, path.join(destination, 'Cargo.toml'));
  }
  fs.mkdirSync(path.join(temporary, 'npm'));
  fs.copyFileSync(path.join(repository, 'npm/package.json'), path.join(temporary, 'npm/package.json'));
  fs.copyFileSync(
    path.join(repository, 'npm/package-lock.json'),
    path.join(temporary, 'npm/package-lock.json'),
  );
  fs.cpSync(path.join(repository, 'npm/packages'), path.join(temporary, 'npm/packages'), {
    recursive: true,
  });

  const version = '9.8.7-beta.1';
  setReleaseVersion(version, { repositoryRoot: temporary });
  setReleaseVersion(version, { repositoryRoot: temporary });
  assert.equal(workspaceVersion(path.join(temporary, 'Cargo.toml')), version);
  assert.equal(
    validateReleaseMetadata({
      cargoToml: path.join(temporary, 'Cargo.toml'),
      packagesRoot: path.join(temporary, 'npm/packages'),
    }).version,
    version,
  );
  const rootPackage = JSON.parse(
    fs.readFileSync(path.join(temporary, 'npm/package.json'), 'utf8'),
  );
  const rootLock = JSON.parse(
    fs.readFileSync(path.join(temporary, 'npm/package-lock.json'), 'utf8'),
  );
  assert.equal(rootPackage.version, version);
  assert.equal(rootLock.version, version);
  assert.equal(rootLock.packages[''].version, version);
  const main = JSON.parse(
    fs.readFileSync(path.join(temporary, 'npm/packages/shared-context/package.json'), 'utf8'),
  );
  assert.deepEqual(main.optionalDependencies, {
    [PLATFORM_PACKAGES.arm64]: version,
    [PLATFORM_PACKAGES.x64]: version,
  });
  assert.match(
    fs.readFileSync(path.join(temporary, 'Cargo.lock'), 'utf8'),
    new RegExp(`name = "sctx-cli"\\nversion = "${version.replaceAll('.', '\\.')}`),
  );
  assert.deepEqual(parseVersionOptions([version]), { version });
  assert.throws(() => parseVersionOptions(['not-semver']), /valid SemVer/);
});
