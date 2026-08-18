#!/usr/bin/env node

'use strict';

const {
  buildPlatformPackage,
  canonicalJson,
  parseArguments,
} = require('./lib/packaging');

function main(argv = process.argv.slice(2)) {
  const options = parseArguments(argv, ['--arch', '--binary', '--output-dir']);
  const result = buildPlatformPackage({
    arch: options['--arch'],
    binary: options['--binary'],
    outputDirectory: options['--output-dir'],
  });
  process.stdout.write(
    canonicalJson({
      manifest: result.manifestPath,
      tarball: result.tarball,
      tarballSha256: result.manifest.tarball.sha256,
    }),
  );
}

if (require.main === module) {
  try {
    main();
  } catch (error) {
    process.stderr.write(`build-platform-package: ${error.message}\n`);
    process.exitCode = 1;
  }
}

module.exports = { main };
