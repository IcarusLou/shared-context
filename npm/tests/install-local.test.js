'use strict';

const assert = require('node:assert/strict');
const path = require('node:path');
const test = require('node:test');

const {
  DEFAULT_PREFIX,
  hostTarget,
  installArguments,
  parseOptions,
} = require('../scripts/install-local');

test('local install defaults to a release build in the ignored target tree', () => {
  assert.deepEqual(parseOptions([]), {
    prefix: DEFAULT_PREFIX,
    profile: 'release',
  });
  assert.match(DEFAULT_PREFIX, /\/target\/npm-local$/);
});

test('local install resolves a custom prefix and validates the Cargo profile', () => {
  assert.deepEqual(parseOptions(['--profile', 'debug', '--prefix', 'prefix with 空格'], '/tmp'), {
    prefix: path.resolve('/tmp', 'prefix with 空格'),
    profile: 'debug',
  });
  assert.throws(() => parseOptions(['--profile', 'production']), /expected debug or release/);
  assert.throws(() => parseOptions(['--prefix']), /Missing value for --prefix/);
});

test('host target maps native NPM architectures to thin macOS Rust targets', () => {
  assert.equal(hostTarget('darwin', 'arm64').rustTarget, 'aarch64-apple-darwin');
  assert.equal(hostTarget('darwin', 'x64').rustTarget, 'x86_64-apple-darwin');
  assert.throws(() => hostTarget('linux', 'x64'), /only support macOS/);
});

test('local NPM install is offline, script-free, and contains both tgz packages', () => {
  const arguments_ = installArguments({
    prefix: '/tmp/local-prefix',
    cache: '/tmp/empty-cache',
    mainTarball: '/tmp/shared-context.tgz',
    platformTarball: '/tmp/shared-context-darwin-arm64.tgz',
  });
  assert.ok(arguments_.includes('--global'));
  assert.ok(arguments_.includes('--offline'));
  assert.ok(arguments_.includes('--ignore-scripts'));
  assert.ok(arguments_.includes('--omit=optional'));
  assert.ok(arguments_.includes('/tmp/shared-context.tgz'));
  assert.ok(arguments_.includes('/tmp/shared-context-darwin-arm64.tgz'));
  assert.ok(!arguments_.includes('setup'));
});
