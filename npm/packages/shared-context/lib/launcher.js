'use strict';

const crypto = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');
const { spawnSync } = require('node:child_process');

const PLATFORM_PACKAGES = Object.freeze({
  'darwin-arm64': '@bytedance-dev/shared-context-darwin-arm64',
  'darwin-x64': '@bytedance-dev/shared-context-darwin-x64',
});

class LauncherError extends Error {
  constructor(message) {
    super(message);
    this.name = 'LauncherError';
  }
}

function packageFor(platform, arch) {
  const packageName = PLATFORM_PACKAGES[`${platform}-${arch}`];
  if (!packageName) {
    throw new LauncherError(
      `Shared Context does not support ${platform}/${arch}; supported targets are darwin/arm64 and darwin/x64`,
    );
  }
  return packageName;
}

function resolveBinary({
  platform = process.platform,
  arch = process.arch,
  resolve = require.resolve,
} = {}) {
  const packageName = packageFor(platform, arch);
  try {
    return {
      packageName,
      binary: resolve(`${packageName}/bin/sctx`),
    };
  } catch (error) {
    if (error && error.code === 'MODULE_NOT_FOUND') {
      throw new LauncherError(
        `Missing optional platform package ${packageName} for ${platform}/${arch}. ` +
          'Reinstall @bytedance-dev/shared-context with optional dependencies enabled, or use the matching offline bundle.',
      );
    }
    throw error;
  }
}

function readExpectedDigest(binary) {
  const checksumPath = `${binary}.sha256`;
  let checksum;
  try {
    checksum = fs.readFileSync(checksumPath, 'utf8');
  } catch (error) {
    throw new LauncherError(
      `Cannot read packaged binary checksum ${checksumPath}: ${error.message}`,
    );
  }
  const match = /^([a-f0-9]{64})  sctx\n?$/.exec(checksum);
  if (!match) {
    throw new LauncherError(`Invalid packaged binary checksum file: ${checksumPath}`);
  }
  return match[1];
}

function sha256File(file) {
  let bytes;
  try {
    bytes = fs.readFileSync(file);
  } catch (error) {
    throw new LauncherError(`Cannot read packaged binary ${file}: ${error.message}`);
  }
  return crypto.createHash('sha256').update(bytes).digest('hex');
}

function verifyBinary(binary, { spawn = spawnSync } = {}) {
  const expected = readExpectedDigest(binary);
  const actual = sha256File(binary);
  if (!crypto.timingSafeEqual(Buffer.from(actual), Buffer.from(expected))) {
    throw new LauncherError(
      `Packaged binary SHA-256 mismatch for ${binary}: expected ${expected}, got ${actual}`,
    );
  }

  const verification = spawn(
    '/usr/bin/codesign',
    ['--verify', '--strict', '--verbose=2', binary],
    { encoding: 'utf8' },
  );
  if (verification.error) {
    throw new LauncherError(
      `Cannot execute macOS signature verification for ${binary}: ${verification.error.message}`,
    );
  }
  if (verification.status !== 0) {
    const detail = String(verification.stderr || verification.stdout || '').trim();
    throw new LauncherError(
      `Packaged binary signature verification failed for ${binary}` +
        (detail ? `: ${detail}` : ''),
    );
  }
}

function forward(
  binary,
  argv,
  { spawn = spawnSync, kill = (pid, signal) => process.kill(pid, signal) } = {},
) {
  const result = spawn(binary, argv, { stdio: 'inherit' });
  if (result.error) {
    throw new LauncherError(`Cannot execute packaged binary ${binary}: ${result.error.message}`);
  }
  if (result.signal) {
    kill(process.pid, result.signal);
    return 1;
  }
  return result.status === null ? 1 : result.status;
}

function launch(argv = process.argv.slice(2), dependencies = {}) {
  const resolved = resolveBinary(dependencies);
  (dependencies.verify || verifyBinary)(resolved.binary, dependencies);
  return forward(resolved.binary, argv, dependencies);
}

function main() {
  try {
    process.exitCode = launch();
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    process.stderr.write(`sctx launcher: ${message}\n`);
    process.exitCode = 1;
  }
}

module.exports = {
  LauncherError,
  forward,
  launch,
  main,
  packageFor,
  readExpectedDigest,
  resolveBinary,
  sha256File,
  verifyBinary,
};
