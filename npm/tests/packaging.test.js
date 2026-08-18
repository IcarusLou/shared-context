'use strict';

const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawnSync } = require('node:child_process');
const test = require('node:test');

const { buildOfflineBundle, installScript } = require('../scripts/build-offline-bundle');
const {
  PACKAGES_ROOT,
  buildPlatformPackage,
  npmPack,
  run,
  sha256File,
} = require('../scripts/lib/packaging');

function readJson(file) {
  return JSON.parse(fs.readFileSync(file, 'utf8'));
}

function tarEntries(tarball) {
  return run('/usr/bin/tar', ['-tzf', tarball]).split('\n').filter(Boolean).sort();
}

function extract(tarball, destination) {
  fs.mkdirSync(destination, { recursive: true });
  run('/usr/bin/tar', ['-xzf', tarball, '-C', destination]);
}

function signedSctx(arch, temporary) {
  const repository = path.resolve(__dirname, '../..');
  const rustTarget = arch === 'arm64' ? 'aarch64-apple-darwin' : 'x86_64-apple-darwin';
  run('cargo', ['build', '--locked', '--bin', 'sctx', '--target', rustTarget], {
    cwd: repository,
  });
  const source = path.join(repository, 'target', rustTarget, 'debug/sctx');
  const destination = path.join(temporary, `sctx-${arch}`);
  fs.copyFileSync(source, destination);
  fs.chmodSync(destination, 0o755);
  run('/usr/bin/codesign', ['--force', '--sign', '-', destination]);
  assert.equal(
    run('/usr/bin/lipo', ['-archs', destination]),
    arch === 'arm64' ? 'arm64' : 'x86_64',
  );
  return destination;
}

function assertChecksums(bundle) {
  const lines = fs.readFileSync(path.join(bundle, 'SHA256SUMS'), 'utf8').trim().split('\n');
  assert.ok(lines.length >= 6);
  for (const line of lines) {
    const match = /^([a-f0-9]{64})  (.+)$/.exec(line);
    assert.ok(match, `invalid checksum line: ${line}`);
    assert.equal(sha256File(path.join(bundle, match[2])), match[1]);
  }
}

test('package metadata has exact optional, os, and cpu contracts with no lifecycle mutation', () => {
  const main = readJson(path.join(PACKAGES_ROOT, 'shared-context/package.json'));
  assert.deepEqual(main.optionalDependencies, {
    '@company/shared-context-darwin-arm64': main.version,
    '@company/shared-context-darwin-x64': main.version,
  });
  assert.equal(main.scripts, undefined);

  for (const [arch, cpu] of [
    ['arm64', 'arm64'],
    ['x64', 'x64'],
  ]) {
    const platform = readJson(
      path.join(PACKAGES_ROOT, `shared-context-darwin-${arch}/package.json`),
    );
    assert.deepEqual(platform.os, ['darwin']);
    assert.deepEqual(platform.cpu, [cpu]);
    assert.equal(platform.version, main.version);
    assert.equal(platform.scripts, undefined);
  }
});

test('npm pack main package contains only the thin launcher surface', (context) => {
  const temporary = fs.mkdtempSync(path.join(os.tmpdir(), 'sctx-main-pack-'));
  context.after(() => fs.rmSync(temporary, { recursive: true, force: true }));
  const tarball = npmPack(
    path.join(PACKAGES_ROOT, 'shared-context'),
    temporary,
    path.join(temporary, 'cache'),
  );
  assert.deepEqual(tarEntries(tarball), [
    'package/README.md',
    'package/bin/sctx.js',
    'package/lib/launcher.js',
    'package/package.json',
  ]);
  const verbose = run('/usr/bin/tar', ['-tvzf', tarball]);
  assert.match(verbose, /-rwxr-xr-x.*package\/bin\/sctx\.js/);
});

test('offline install entrypoint invokes setup even without user arguments', (context) => {
  const temporary = fs.mkdtempSync(path.join(os.tmpdir(), 'sctx-install-entrypoint-'));
  context.after(() => fs.rmSync(temporary, { recursive: true, force: true }));
  const bundle = path.join(temporary, 'bundle');
  const fakeBin = path.join(temporary, 'fake-bin');
  const invocation = path.join(temporary, 'invocation');
  fs.mkdirSync(path.join(bundle, 'node_modules/.bin'), { recursive: true });
  fs.mkdirSync(fakeBin, { recursive: true });
  fs.writeFileSync(path.join(bundle, 'install'), installScript(), { mode: 0o755 });
  fs.writeFileSync(path.join(bundle, 'payload'), 'offline fixture\n');
  fs.writeFileSync(
    path.join(bundle, 'SHA256SUMS'),
    `${sha256File(path.join(bundle, 'payload'))}  payload\n`,
  );
  fs.writeFileSync(path.join(fakeBin, 'npm'), '#!/bin/sh\nexit 0\n', { mode: 0o755 });
  fs.writeFileSync(
    path.join(bundle, 'node_modules/.bin/sctx'),
    `#!/bin/sh\nprintf '%s\\n' "$@" > "${invocation}"\n`,
    { mode: 0o755 },
  );
  run(path.join(bundle, 'install'), [], {
    cwd: bundle,
    env: { ...process.env, PATH: `${fakeBin}:/usr/bin:/bin` },
  });
  assert.equal(fs.readFileSync(invocation, 'utf8'), 'setup\n');
});

test(
  'platform packs and per-architecture bundles contain one signed thin Mach-O',
  { skip: process.platform !== 'darwin', timeout: 300_000 },
  (context) => {
    const temporary = fs.mkdtempSync(path.join(os.tmpdir(), 'sctx-platform-pack-'));
    context.after(() => fs.rmSync(temporary, { recursive: true, force: true }));

    for (const arch of ['arm64', 'x64']) {
      const signed = signedSctx(arch, temporary);
      const output = path.join(temporary, `platform-${arch}`);
      const built = buildPlatformPackage({ arch, binary: signed, outputDirectory: output });
      assert.deepEqual(tarEntries(built.tarball), [
        'package/README.md',
        'package/bin/sctx',
        'package/bin/sctx.sha256',
        'package/package.json',
      ]);
      const extracted = path.join(temporary, `platform-extracted-${arch}`);
      extract(built.tarball, extracted);
      const packagedBinary = path.join(extracted, 'package/bin/sctx');
      assert.equal(
        run('/usr/bin/lipo', ['-archs', packagedBinary]),
        arch === 'arm64' ? 'arm64' : 'x86_64',
      );
      run('/usr/bin/codesign', ['--verify', '--strict', '--verbose=2', packagedBinary]);
      assert.equal(
        fs.readFileSync(`${packagedBinary}.sha256`, 'utf8'),
        `${sha256File(packagedBinary)}  sctx\n`,
      );

      const bundleOutput = path.join(temporary, `bundle-${arch}`);
      const bundle = buildOfflineBundle({ arch, binary: signed, outputDirectory: bundleOutput });
      const files = fs.readdirSync(path.join(bundle.bundleDirectory, 'packages')).sort();
      assert.deepEqual(files, [
        `shared-context-darwin-${arch}.tgz`,
        'shared-context.tgz',
      ]);
      assert.ok(!files.some((file) => file.includes(arch === 'arm64' ? 'x64' : 'arm64')));
      assertChecksums(bundle.bundleDirectory);
      const root = readJson(path.join(bundle.bundleDirectory, 'package.json'));
      assert.deepEqual(Object.values(root.dependencies).sort(), [
        `file:packages/shared-context-darwin-${arch}.tgz`,
        'file:packages/shared-context.tgz',
      ].sort());
      const lock = readJson(path.join(bundle.bundleDirectory, 'package-lock.json'));
      for (const entry of Object.values(lock.packages)) {
        if (entry.resolved) {
          assert.match(entry.resolved, /^file:packages\//);
        }
      }
      const archiveEntries = tarEntries(bundle.archive);
      assert.ok(archiveEntries.every((entry) => entry.startsWith(`${path.basename(bundle.bundleDirectory)}/`)));
    }
  },
);

test(
  'arm64 real CLI completes the offline setup --demo loop without Registry access',
  { skip: process.platform !== 'darwin' || process.arch !== 'arm64', timeout: 600_000 },
  (context) => {
    const temporary = fs.mkdtempSync(path.join(os.tmpdir(), 'sctx-offline-smoke-'));
    context.after(() => fs.rmSync(temporary, { recursive: true, force: true }));
    const signed = signedSctx('arm64', temporary);
    const output = path.join(temporary, 'output');
    const built = buildOfflineBundle({ arch: 'arm64', binary: signed, outputDirectory: output });
    const home = path.join(temporary, 'home with 空格');
    const root = path.join(home, '.shared-context');
    const cache = path.join(temporary, 'empty-cache');
    fs.mkdirSync(home, { recursive: true });

    const installEnvironment = {
      ...process.env,
      HOME: home,
      npm_config_cache: cache,
      npm_config_registry: 'http://127.0.0.1:9',
    };
    run(
      'npm',
      [
        'install',
        '--offline',
        '--ignore-scripts',
        '--omit=optional',
        '--no-audit',
        '--no-fund',
      ],
      { cwd: built.bundleDirectory, env: installEnvironment },
    );
    assert.equal(fs.existsSync(path.join(home, '.cursor/mcp.json')), false);
    assert.equal(fs.existsSync(path.join(home, '.codex/config.toml')), false);
    fs.mkdirSync(path.join(home, '.cursor'), { recursive: true });
    fs.writeFileSync(
      path.join(home, '.cursor/mcp.json'),
      `${JSON.stringify({
        userSetting: { 中文: true },
        mcpServers: { existing: { command: 'keep user MCP' } },
      }, null, 2)}\n`,
    );

    const version = run(path.join(built.bundleDirectory, 'node_modules/.bin/sctx'), ['--version'], {
      cwd: built.bundleDirectory,
      env: installEnvironment,
    });
    assert.match(version, /^sctx 0\.1\.0$/);

    const started = Date.now();
    const result = spawnSync(
      path.join(built.bundleDirectory, 'install'),
      ['--demo', '--agents', 'cursor', '--root', root, '--yes'],
      {
        cwd: built.bundleDirectory,
        encoding: 'utf8',
        env: installEnvironment,
      },
    );
    assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`);
    assert.ok(Date.now() - started < 180_000, 'offline setup --demo exceeded three minutes');
    assert.ok(fs.existsSync(path.join(root, 'bin/current/sctx')));
    const eventPaths = run(
      'git',
      ['-C', path.join(root, 'repository'), 'ls-tree', '-r', '--name-only', 'HEAD'],
    ).split('\n').filter((entry) => entry.startsWith('events/'));
    assert.equal(eventPaths.length, 4);
    const search = JSON.parse(run(
      path.join(built.bundleDirectory, 'node_modules/.bin/sctx'),
      ['--json', 'search', '--query', 'searchable CLI MCP', '--status', 'accepted'],
      { cwd: built.bundleDirectory, env: installEnvironment },
    ));
    assert.equal(search.data.results.length, 1);
    assert.equal(
      search.data.results[0].statement,
      'Published demo context is searchable through CLI and MCP.',
    );
    const cursor = readJson(path.join(home, '.cursor/mcp.json'));
    assert.equal(cursor.mcpServers['shared-context'].command, path.join(root, 'bin/current/sctx'));
    assert.match(fs.readFileSync(path.join(built.bundleDirectory, 'install'), 'utf8'), /--offline/);

    const rebuilt = JSON.parse(run(
      path.join(built.bundleDirectory, 'node_modules/.bin/sctx'),
      ['--json', 'index', 'rebuild'],
      { cwd: built.bundleDirectory, env: installEnvironment },
    ));
    assert.equal(rebuilt.command, 'index.rebuild');
    assert.equal(rebuilt.data.rebuilt, true);
    assert.match(rebuilt.tree, /^[0-9a-f]{40}$/);
    assert.ok(Number.isSafeInteger(rebuilt.generation) && rebuilt.generation > 0);

    const uninstalled = JSON.parse(run(
      path.join(built.bundleDirectory, 'node_modules/.bin/sctx'),
      ['--json', 'uninstall', '--root', root],
      { cwd: built.bundleDirectory, env: installEnvironment },
    ));
    assert.equal(uninstalled.repository_retained, true);
    assert.ok(fs.existsSync(path.join(root, 'repository/.git')));
    assert.equal(fs.existsSync(path.join(root, 'bin/current')), false);
    const restoredCursor = readJson(path.join(home, '.cursor/mcp.json'));
    assert.equal(restoredCursor.mcpServers['shared-context'], undefined);
    assert.equal(restoredCursor.mcpServers.existing.command, 'keep user MCP');
    assert.equal(restoredCursor.userSetting.中文, true);
  },
);

test(
  'offline manifest and archive are reproducible for identical signed input',
  { skip: process.platform !== 'darwin', timeout: 300_000 },
  (context) => {
    const temporary = fs.mkdtempSync(path.join(os.tmpdir(), 'sctx-reproducible-'));
    context.after(() => fs.rmSync(temporary, { recursive: true, force: true }));
    const signed = signedSctx('arm64', temporary);
    const first = buildOfflineBundle({
      arch: 'arm64',
      binary: signed,
      outputDirectory: path.join(temporary, 'first'),
    });
    const second = buildOfflineBundle({
      arch: 'arm64',
      binary: signed,
      outputDirectory: path.join(temporary, 'second'),
    });
    assert.equal(
      fs.readFileSync(first.manifest, 'utf8'),
      fs.readFileSync(second.manifest, 'utf8'),
    );
    assert.equal(first.archiveSha256, second.archiveSha256);
  },
);
