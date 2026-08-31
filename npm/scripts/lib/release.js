'use strict';

const fs = require('node:fs');
const path = require('node:path');

const {
  PACKAGES_ROOT,
  canonicalJson,
  sha256File,
  writeFile,
} = require('./packaging');

const REPOSITORY_ROOT = path.resolve(__dirname, '../../..');
const REGISTRY = 'https://bnpm.byted.org';
const SCOPE = '@bytedance-dev/';
const MAIN_PACKAGE = '@bytedance-dev/shared-context';
const PLATFORM_PACKAGES = Object.freeze({
  arm64: '@bytedance-dev/shared-context-darwin-arm64',
  x64: '@bytedance-dev/shared-context-darwin-x64',
});
const DEFAULT_ARTIFACTS = path.join(REPOSITORY_ROOT, 'target/npm-release');
const SEMVER = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$/;
const DIST_TAG = /^[a-z0-9][a-z0-9._-]*$/;

function readJson(file) {
  return JSON.parse(fs.readFileSync(file, 'utf8'));
}

function validateVersion(version) {
  if (!SEMVER.test(version)) {
    throw new Error(`NPM package version is not valid SemVer: ${version}`);
  }
  return version;
}

function packageDefinitions() {
  return [
    {
      name: PLATFORM_PACKAGES.arm64,
      directory: path.join(PACKAGES_ROOT, 'shared-context-darwin-arm64'),
      file: 'shared-context-darwin-arm64.tgz',
    },
    {
      name: PLATFORM_PACKAGES.x64,
      directory: path.join(PACKAGES_ROOT, 'shared-context-darwin-x64'),
      file: 'shared-context-darwin-x64.tgz',
    },
    {
      name: MAIN_PACKAGE,
      directory: path.join(PACKAGES_ROOT, 'shared-context'),
      file: 'shared-context.tgz',
    },
  ];
}

function workspaceVersion(cargoToml = path.join(REPOSITORY_ROOT, 'Cargo.toml')) {
  const contents = fs.readFileSync(cargoToml, 'utf8');
  const workspacePackage = /^\[workspace\.package\]\s*$([\s\S]*?)(?=^\[|(?![\s\S]))/m.exec(contents);
  const version = workspacePackage && /^version\s*=\s*"([^"]+)"\s*$/m.exec(workspacePackage[1]);
  if (!version) {
    throw new Error(`Cannot read workspace.package.version from ${cargoToml}`);
  }
  return version[1];
}

function validateReleaseMetadata({ cargoToml, packagesRoot = PACKAGES_ROOT } = {}) {
  const definitions = packageDefinitions().map((definition) => ({
    ...definition,
    directory: path.join(packagesRoot, path.basename(definition.directory)),
  }));
  const packages = definitions.map((definition) => ({
    ...definition,
    metadata: readJson(path.join(definition.directory, 'package.json')),
  }));
  const main = packages.find((item) => item.name === MAIN_PACKAGE).metadata;

  validateVersion(main.version);
  for (const item of packages) {
    const metadata = item.metadata;
    if (metadata.name !== item.name || !metadata.name.startsWith(SCOPE)) {
      throw new Error(`Expected scoped package ${item.name}, found ${metadata.name}`);
    }
    if (metadata.version !== main.version) {
      throw new Error(
        `Package version mismatch: ${metadata.name}@${metadata.version}, expected ${main.version}`,
      );
    }
    if (metadata.private === true) {
      throw new Error(`Release package must not be private: ${metadata.name}`);
    }
    if (!metadata.publishConfig || metadata.publishConfig.registry !== REGISTRY) {
      throw new Error(`Package ${metadata.name} must publish to ${REGISTRY}`);
    }
  }

  const expectedOptional = Object.fromEntries(
    Object.values(PLATFORM_PACKAGES).map((name) => [name, main.version]),
  );
  if (JSON.stringify(main.optionalDependencies) !== JSON.stringify(expectedOptional)) {
    throw new Error('Launcher optionalDependencies must pin both platform packages to its version');
  }

  if (cargoToml !== false) {
    const rustVersion = workspaceVersion(cargoToml || path.join(REPOSITORY_ROOT, 'Cargo.toml'));
    if (rustVersion !== main.version) {
      throw new Error(
        `Cargo workspace version ${rustVersion} does not match NPM version ${main.version}`,
      );
    }
  }

  return { definitions, packages, version: main.version };
}

function recommendedDistTag(version) {
  const match = SEMVER.exec(version);
  if (!match) {
    throw new Error(`Invalid SemVer version: ${version}`);
  }
  return match[4] ? match[4].split('.')[0] : 'latest';
}

function validateDistTag(version, tag) {
  if (!DIST_TAG.test(tag)) {
    throw new Error(`Invalid NPM dist-tag: ${tag}`);
  }
  const expected = recommendedDistTag(version);
  if (tag !== expected) {
    throw new Error(`Version ${version} must use dist-tag ${expected}, not ${tag}`);
  }
  return tag;
}

function writeReleaseManifest(artifactsDirectory, version) {
  const artifacts = path.resolve(artifactsDirectory);
  const packages = packageDefinitions().map((definition, index) => {
    const tarball = path.join(artifacts, definition.file);
    if (!fs.statSync(tarball).isFile()) {
      throw new Error(`Release tarball is not a regular file: ${tarball}`);
    }
    return {
      name: definition.name,
      version,
      file: definition.file,
      sha256: sha256File(tarball),
      publishOrder: index + 1,
    };
  });
  const manifest = {
    formatVersion: 1,
    registry: REGISTRY,
    version,
    packages,
  };
  writeFile(path.join(artifacts, 'release-manifest.json'), canonicalJson(manifest));
  writeFile(
    path.join(artifacts, 'SHA256SUMS'),
    `${packages.map((item) => `${item.sha256}  ${item.file}`).join('\n')}\n`,
  );
  return manifest;
}

function readReleaseManifest(artifactsDirectory, expectedVersion) {
  const artifacts = path.resolve(artifactsDirectory);
  const manifest = readJson(path.join(artifacts, 'release-manifest.json'));
  const definitions = packageDefinitions();
  if (
    manifest.formatVersion !== 1 ||
    manifest.registry !== REGISTRY ||
    !Array.isArray(manifest.packages) ||
    manifest.packages.length !== definitions.length
  ) {
    throw new Error('Invalid release manifest header');
  }
  if (expectedVersion && manifest.version !== expectedVersion) {
    throw new Error(
      `Release manifest version ${manifest.version} does not match ${expectedVersion}`,
    );
  }
  for (let index = 0; index < definitions.length; index += 1) {
    const expected = definitions[index];
    const actual = manifest.packages[index];
    if (
      actual.name !== expected.name ||
      actual.version !== manifest.version ||
      actual.file !== expected.file ||
      actual.publishOrder !== index + 1 ||
      !/^[a-f0-9]{64}$/.test(actual.sha256)
    ) {
      throw new Error(`Invalid release package entry at publish order ${index + 1}`);
    }
    const tarball = path.join(artifacts, actual.file);
    const actualSha256 = sha256File(tarball);
    if (actualSha256 !== actual.sha256) {
      throw new Error(
        `Release tarball SHA-256 mismatch for ${actual.file}: expected ${actual.sha256}, got ${actualSha256}`,
      );
    }
  }
  return manifest;
}

function publishArguments(tarball, tag, dryRun = false) {
  const arguments_ = [
    'publish',
    tarball,
    '--registry',
    REGISTRY,
    '--tag',
    tag,
    '--ignore-scripts',
  ];
  if (dryRun) {
    arguments_.push('--dry-run', '--offline');
  }
  return arguments_;
}

module.exports = {
  DEFAULT_ARTIFACTS,
  MAIN_PACKAGE,
  PLATFORM_PACKAGES,
  REGISTRY,
  SCOPE,
  packageDefinitions,
  publishArguments,
  readReleaseManifest,
  recommendedDistTag,
  validateDistTag,
  validateReleaseMetadata,
  validateVersion,
  workspaceVersion,
  writeReleaseManifest,
};
