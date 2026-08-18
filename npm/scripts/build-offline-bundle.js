#!/usr/bin/env node

'use strict';

const fs = require('node:fs');
const path = require('node:path');

const {
  PACKAGES_ROOT,
  buildPlatformPackage,
  canonicalJson,
  createDeterministicTarGz,
  npmEnvironment,
  npmPack,
  parseArguments,
  run,
  sha256File,
  targetFor,
  writeFile,
} = require('./lib/packaging');

function installScript() {
  return `#!/bin/sh
set -eu

BUNDLE_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cd "$BUNDLE_DIR"

/usr/bin/shasum -a 256 -c SHA256SUMS

NPM_CACHE_DIR=$(mktemp -d "${'${TMPDIR:-/tmp}'}/shared-context-npm.XXXXXX")
cleanup() {
  rm -rf -- "$NPM_CACHE_DIR"
}
trap cleanup EXIT HUP INT TERM

npm install \\
  --offline \\
  --ignore-scripts \\
  --omit=optional \\
  --no-audit \\
  --no-fund \\
  --cache "$NPM_CACHE_DIR"

"$BUNDLE_DIR/node_modules/.bin/sctx" setup "$@"
`;
}

function assertLocalRootReferences(bundleDirectory, packageJson, packageLock) {
  for (const [name, specifier] of Object.entries(packageJson.dependencies || {})) {
    if (!specifier.startsWith('file:packages/')) {
      throw new Error(`Root dependency ${name} is not a local package tarball: ${specifier}`);
    }
  }
  for (const [location, entry] of Object.entries(packageLock.packages || {})) {
    if (entry.resolved && !entry.resolved.startsWith('file:packages/')) {
      throw new Error(`Lock entry ${location} is not resolved from a local package tarball`);
    }
  }
  if (fs.existsSync(path.join(bundleDirectory, 'node_modules'))) {
    throw new Error('Lock generation unexpectedly created node_modules');
  }
}

function buildOfflineBundle({ arch, binary, outputDirectory }) {
  const target = targetFor(arch);
  const output = path.resolve(outputDirectory);
  fs.mkdirSync(output, { recursive: true });
  const mainPackage = JSON.parse(
    fs.readFileSync(path.join(PACKAGES_ROOT, 'shared-context/package.json'), 'utf8'),
  );
  const bundleName = `shared-context-${mainPackage.version}-darwin-${arch}-offline`;
  const finalDirectory = path.join(output, bundleName);
  const archive = path.join(output, `${bundleName}.tar.gz`);
  if (fs.existsSync(finalDirectory) || fs.existsSync(archive)) {
    throw new Error(
      `Refusing to overwrite existing bundle output; remove it explicitly first: ${bundleName}`,
    );
  }

  const temporary = fs.mkdtempSync(path.join(output, `.${bundleName}-`));
  try {
    const bundle = path.join(temporary, bundleName);
    const packages = path.join(bundle, 'packages');
    fs.mkdirSync(packages, { recursive: true });

    const mainPacked = npmPack(
      path.join(PACKAGES_ROOT, 'shared-context'),
      packages,
      path.join(temporary, 'npm-cache-main'),
    );
    const mainTarball = path.join(packages, 'shared-context.tgz');
    fs.renameSync(mainPacked, mainTarball);

    const platformOutput = path.join(temporary, 'platform-output');
    const platform = buildPlatformPackage({
      arch,
      binary,
      outputDirectory: platformOutput,
    });
    const platformTarballName = `shared-context-darwin-${arch}.tgz`;
    const platformTarball = path.join(packages, platformTarballName);
    fs.copyFileSync(platform.tarball, platformTarball);

    const rootPackage = {
      name: `shared-context-offline-darwin-${arch}`,
      version: mainPackage.version,
      private: true,
      description: `Offline installer for @company/shared-context on darwin/${arch}`,
      dependencies: {
        '@company/shared-context': 'file:packages/shared-context.tgz',
        [target.packageName]: `file:packages/${platformTarballName}`,
      },
    };
    writeFile(path.join(bundle, 'package.json'), canonicalJson(rootPackage));

    run(
      'npm',
      [
        'install',
        '--package-lock-only',
        '--ignore-scripts',
        '--offline',
        '--omit=optional',
        '--no-audit',
        '--no-fund',
        '--force',
        '--os=darwin',
        `--cpu=${target.cpu}`,
      ],
      {
        cwd: bundle,
        env: npmEnvironment(path.join(temporary, 'npm-cache-lock')),
      },
    );
    const packageLockPath = path.join(bundle, 'package-lock.json');
    const packageLock = JSON.parse(fs.readFileSync(packageLockPath, 'utf8'));
    assertLocalRootReferences(bundle, rootPackage, packageLock);

    writeFile(path.join(bundle, 'install'), installScript(), 0o755);

    const payloadNames = [
      'install',
      'package-lock.json',
      'package.json',
      'packages/shared-context.tgz',
      `packages/${platformTarballName}`,
    ];
    const payload = payloadNames.map((relative) => {
      const file = path.join(bundle, relative);
      return {
        path: relative,
        sha256: sha256File(file),
        size: fs.statSync(file).size,
      };
    });
    const manifest = {
      formatVersion: 1,
      package: '@company/shared-context',
      version: mainPackage.version,
      target: {
        os: 'darwin',
        cpu: target.cpu,
        machoArchitecture: target.machoArch,
      },
      binary: platform.manifest.binary,
      payload,
    };
    writeFile(path.join(bundle, 'MANIFEST.json'), canonicalJson(manifest));

    const checksumNames = [...payloadNames, 'MANIFEST.json'].sort();
    const checksums = checksumNames
      .map((relative) => `${sha256File(path.join(bundle, relative))}  ${relative}`)
      .join('\n');
    writeFile(path.join(bundle, 'SHA256SUMS'), `${checksums}\n`);

    fs.renameSync(bundle, finalDirectory);
    createDeterministicTarGz(finalDirectory, archive, bundleName);
    return {
      archive,
      archiveSha256: sha256File(archive),
      bundleDirectory: finalDirectory,
      manifest: path.join(finalDirectory, 'MANIFEST.json'),
      target: manifest.target,
    };
  } finally {
    fs.rmSync(temporary, { recursive: true, force: true });
  }
}

function main(argv = process.argv.slice(2)) {
  const options = parseArguments(argv, ['--arch', '--binary', '--output-dir']);
  process.stdout.write(
    canonicalJson(
      buildOfflineBundle({
        arch: options['--arch'],
        binary: options['--binary'],
        outputDirectory: options['--output-dir'],
      }),
    ),
  );
}

if (require.main === module) {
  try {
    main();
  } catch (error) {
    process.stderr.write(`build-offline-bundle: ${error.message}\n`);
    process.exitCode = 1;
  }
}

module.exports = { assertLocalRootReferences, buildOfflineBundle, installScript, main };
