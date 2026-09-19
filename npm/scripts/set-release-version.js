#!/usr/bin/env node

'use strict';

const fs = require('node:fs');
const path = require('node:path');

const {
  PLATFORM_PACKAGES,
  validateReleaseMetadata,
  validateVersion,
} = require('./lib/release');
const { canonicalJson } = require('./lib/packaging');

const REPOSITORY_ROOT = path.resolve(__dirname, '../..');

function usage() {
  return `Synchronize the release version across Cargo and all NPM packages.

Usage:
  node npm/scripts/set-release-version.js <SEMVER>

Example:
  npm run release:version -- 0.2.0

This updates the workspace Cargo version, local workspace entries in Cargo.lock, the private NPM
tooling package, all three publishable packages, and the launcher's optional dependencies. It does
not build, install, tag, or publish anything.
`;
}

function parseOptions(argv) {
  if (argv.length === 1 && (argv[0] === '-h' || argv[0] === '--help')) {
    return { help: true };
  }
  if (argv.length !== 1) {
    throw new Error(`Expected exactly one SemVer version\n\n${usage()}`);
  }
  return { version: validateVersion(argv[0]) };
}

function replaceWorkspaceVersion(contents, version) {
  const pattern = /(^\[workspace\.package\]\s*$[\s\S]*?^version\s*=\s*")[^"]+("\s*$)/m;
  if (!pattern.test(contents)) {
    throw new Error('Cannot find workspace.package.version in Cargo.toml');
  }
  return contents.replace(pattern, `$1${version}$2`);
}

function workspacePackageNames(repositoryRoot) {
  const crates = path.join(repositoryRoot, 'crates');
  const names = new Set();
  for (const entry of fs.readdirSync(crates, { withFileTypes: true })) {
    if (!entry.isDirectory()) {
      continue;
    }
    const manifest = path.join(crates, entry.name, 'Cargo.toml');
    if (!fs.existsSync(manifest)) {
      continue;
    }
    const contents = fs.readFileSync(manifest, 'utf8');
    const packageSection = /^\[package\]\s*$([\s\S]*?)(?=^\[|(?![\s\S]))/m.exec(contents);
    const name = packageSection && /^name\s*=\s*"([^"]+)"\s*$/m.exec(packageSection[1]);
    if (name) {
      names.add(name[1]);
    }
  }
  if (names.size === 0) {
    throw new Error(`No Cargo workspace packages found under ${crates}`);
  }
  return names;
}

function replaceCargoLockVersions(contents, packageNames, version) {
  const updated = new Set();
  const result = contents.replace(
    /(^\[\[package\]\]\s*$[\s\S]*?)(?=^\[\[package\]\]\s*$|(?![\s\S]))/gm,
    (block) => {
      const name = /^name\s*=\s*"([^"]+)"\s*$/m.exec(block);
      if (!name || !packageNames.has(name[1]) || /^source\s*=/m.test(block)) {
        return block;
      }
      if (!/^version\s*=\s*"[^"]+"\s*$/m.test(block)) {
        throw new Error(`Cargo.lock package ${name[1]} has no version`);
      }
      updated.add(name[1]);
      return block.replace(/^version\s*=\s*"[^"]+"\s*$/m, `version = "${version}"`);
    },
  );
  const missing = [...packageNames].filter((name) => !updated.has(name));
  if (missing.length > 0) {
    throw new Error(`Cargo.lock is missing workspace packages: ${missing.join(', ')}`);
  }
  return result;
}

function readJson(file) {
  return JSON.parse(fs.readFileSync(file, 'utf8'));
}

function setReleaseVersion(
  version,
  { repositoryRoot = REPOSITORY_ROOT, npmRoot = path.join(repositoryRoot, 'npm') } = {},
) {
  validateVersion(version);
  const cargoToml = path.join(repositoryRoot, 'Cargo.toml');
  const cargoLock = path.join(repositoryRoot, 'Cargo.lock');
  const rootPackage = path.join(npmRoot, 'package.json');
  const rootLock = path.join(npmRoot, 'package-lock.json');
  const packagesRoot = path.join(npmRoot, 'packages');
  const publishable = [
    path.join(packagesRoot, 'shared-context/package.json'),
    path.join(packagesRoot, 'shared-context-darwin-arm64/package.json'),
    path.join(packagesRoot, 'shared-context-darwin-x64/package.json'),
  ];

  const cargoTomlContents = replaceWorkspaceVersion(fs.readFileSync(cargoToml, 'utf8'), version);
  const cargoLockContents = replaceCargoLockVersions(
    fs.readFileSync(cargoLock, 'utf8'),
    workspacePackageNames(repositoryRoot),
    version,
  );
  const rootPackageJson = readJson(rootPackage);
  rootPackageJson.version = version;
  const rootLockJson = readJson(rootLock);
  rootLockJson.version = version;
  if (!rootLockJson.packages || !rootLockJson.packages['']) {
    throw new Error('NPM package-lock.json is missing its root package entry');
  }
  rootLockJson.packages[''].version = version;

  const packageJson = publishable.map((file) => ({ file, metadata: readJson(file) }));
  for (const item of packageJson) {
    item.metadata.version = version;
  }
  const main = packageJson[0].metadata;
  main.optionalDependencies = Object.fromEntries(
    Object.values(PLATFORM_PACKAGES).map((name) => [name, version]),
  );

  const writes = [
    { file: cargoToml, contents: cargoTomlContents },
    { file: cargoLock, contents: cargoLockContents },
    { file: rootPackage, contents: canonicalJson(rootPackageJson) },
    { file: rootLock, contents: canonicalJson(rootLockJson) },
    ...packageJson.map((item) => ({
      file: item.file,
      contents: canonicalJson(item.metadata),
    })),
  ].map((write) => ({
    ...write,
    original: fs.readFileSync(write.file),
  }));
  const written = [];
  try {
    for (const write of writes) {
      fs.writeFileSync(write.file, write.contents);
      written.push(write);
    }

    const validated = validateReleaseMetadata({ cargoToml, packagesRoot });
    if (validated.version !== version) {
      throw new Error(`Version synchronization produced ${validated.version}, expected ${version}`);
    }
  } catch (error) {
    for (const write of written.reverse()) {
      fs.writeFileSync(write.file, write.original);
    }
    throw error;
  }
  return { files: writes.map((item) => item.file), version };
}

function main(argv = process.argv.slice(2)) {
  const options = parseOptions(argv);
  if (options.help) {
    process.stdout.write(usage());
    return;
  }
  const result = setReleaseVersion(options.version);
  process.stdout.write(`Synchronized release version ${result.version}:\n`);
  for (const file of result.files) {
    process.stdout.write(`- ${path.relative(REPOSITORY_ROOT, file)}\n`);
  }
  process.stdout.write('\nRun npm run release:test before committing and tagging the release.\n');
}

if (require.main === module) {
  try {
    main();
  } catch (error) {
    process.stderr.write(`set-release-version: ${error.message}\n`);
    process.exitCode = 1;
  }
}

module.exports = {
  main,
  parseOptions,
  replaceCargoLockVersions,
  replaceWorkspaceVersion,
  setReleaseVersion,
  usage,
  workspacePackageNames,
};
