'use strict';

const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const test = require('node:test');

const {
  LauncherError,
  forward,
  launch,
  packageFor,
  resolveBinary,
  verifyBinary,
} = require('../packages/shared-context/lib/launcher');

test('launcher selects the package matching darwin CPU', () => {
  assert.equal(packageFor('darwin', 'arm64'), '@company/shared-context-darwin-arm64');
  assert.equal(packageFor('darwin', 'x64'), '@company/shared-context-darwin-x64');
  assert.throws(
    () => packageFor('linux', 'x64'),
    /does not support linux\/x64.*darwin\/arm64 and darwin\/x64/,
  );
});

test('missing optional platform package reports the exact repair', () => {
  const missing = new Error('not found');
  missing.code = 'MODULE_NOT_FOUND';
  assert.throws(
    () =>
      resolveBinary({
        platform: 'darwin',
        arch: 'arm64',
        resolve() {
          throw missing;
        },
      }),
    (error) =>
      error instanceof LauncherError &&
      error.message.includes('@company/shared-context-darwin-arm64') &&
      error.message.includes('optional dependencies enabled') &&
      error.message.includes('matching offline bundle'),
  );
});

test('binary verification enforces checksum before code signature', (context) => {
  const temporary = fs.mkdtempSync(path.join(os.tmpdir(), 'sctx-launcher-'));
  context.after(() => fs.rmSync(temporary, { recursive: true, force: true }));
  const binary = path.join(temporary, 'sctx');
  fs.writeFileSync(binary, 'signed fixture');
  fs.writeFileSync(`${binary}.sha256`, `${'0'.repeat(64)}  sctx\n`);
  let signatureChecks = 0;
  assert.throws(
    () =>
      verifyBinary(binary, {
        spawn() {
          signatureChecks += 1;
          return { status: 0 };
        },
      }),
    /SHA-256 mismatch/,
  );
  assert.equal(signatureChecks, 0);

  const digest = require('node:crypto').createHash('sha256').update('signed fixture').digest('hex');
  fs.writeFileSync(`${binary}.sha256`, `${digest}  sctx\n`);
  verifyBinary(binary, {
    spawn(command, args) {
      signatureChecks += 1;
      assert.equal(command, '/usr/bin/codesign');
      assert.deepEqual(args, ['--verify', '--strict', '--verbose=2', binary]);
      return { status: 0 };
    },
  });
  assert.equal(signatureChecks, 1);
});

test('launcher forwards argv, inherited stdio, and native exit code unchanged', () => {
  const calls = [];
  const status = launch(['setup', '--yes', 'space and 中文'], {
    platform: 'darwin',
    arch: 'arm64',
    resolve(request) {
      assert.equal(request, '@company/shared-context-darwin-arm64/bin/sctx');
      return '/fixture/sctx';
    },
    verify(binary) {
      assert.equal(binary, '/fixture/sctx');
    },
    spawn(command, args, options) {
      calls.push({ command, args, options });
      return { status: 37, signal: null };
    },
  });
  assert.equal(status, 37);
  assert.deepEqual(calls, [
    {
      command: '/fixture/sctx',
      args: ['setup', '--yes', 'space and 中文'],
      options: { stdio: 'inherit' },
    },
  ]);
});

test('forward reports a spawn error without converting it to a native exit code', () => {
  assert.throws(
    () =>
      forward('/fixture/sctx', [], {
        spawn() {
          return { error: new Error('permission denied') };
        },
      }),
    /permission denied/,
  );
});

test('forward re-raises the exact child termination signal', () => {
  const signals = [];
  const status = forward('/fixture/sctx', [], {
    spawn() {
      return { status: null, signal: 'SIGTERM' };
    },
    kill(pid, signal) {
      signals.push({ pid, signal });
    },
  });
  assert.deepEqual(signals, [{ pid: process.pid, signal: 'SIGTERM' }]);
  assert.equal(status, 1, 'fallback applies only when an injected kill returns');
});
