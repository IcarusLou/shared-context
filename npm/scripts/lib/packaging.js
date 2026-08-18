'use strict';

const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawnSync } = require('node:child_process');
const zlib = require('node:zlib');

const NPM_ROOT = path.resolve(__dirname, '../..');
const PACKAGES_ROOT = path.join(NPM_ROOT, 'packages');
const TARGETS = Object.freeze({
  arm64: {
    arch: 'arm64',
    cpu: 'arm64',
    machoArch: 'arm64',
    packageName: '@company/shared-context-darwin-arm64',
    sourceDirectory: 'shared-context-darwin-arm64',
  },
  x64: {
    arch: 'x64',
    cpu: 'x64',
    machoArch: 'x86_64',
    packageName: '@company/shared-context-darwin-x64',
    sourceDirectory: 'shared-context-darwin-x64',
  },
});

function fail(message) {
  throw new Error(message);
}

function parseArguments(argv, required) {
  const result = {};
  for (let index = 0; index < argv.length; index += 2) {
    const flag = argv[index];
    const value = argv[index + 1];
    if (!flag || !flag.startsWith('--') || value === undefined || value.startsWith('--')) {
      fail(`Expected --name value arguments, got ${argv.slice(index).join(' ')}`);
    }
    if (Object.hasOwn(result, flag)) {
      fail(`Duplicate argument ${flag}`);
    }
    result[flag] = value;
  }
  for (const flag of required) {
    if (!result[flag]) {
      fail(`Missing required argument ${flag}`);
    }
  }
  return result;
}

function targetFor(arch) {
  const target = TARGETS[arch];
  if (!target) {
    fail(`Unsupported architecture ${arch}; expected arm64 or x64`);
  }
  return target;
}

function run(command, args, options = {}) {
  const result = spawnSync(command, args, {
    encoding: 'utf8',
    ...options,
  });
  if (result.error) {
    fail(`Cannot execute ${command}: ${result.error.message}`);
  }
  if (result.status !== 0) {
    fail(
      `${command} ${args.join(' ')} failed with exit ${result.status}: ` +
        String(result.stderr || result.stdout || '').trim(),
    );
  }
  return String(result.stdout || '').trim();
}

function sha256Buffer(bytes) {
  return crypto.createHash('sha256').update(bytes).digest('hex');
}

function sha256File(file) {
  return sha256Buffer(fs.readFileSync(file));
}

function canonicalJson(value) {
  return `${JSON.stringify(value, null, 2)}\n`;
}

function writeFile(file, contents, mode = 0o644) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, contents, { mode });
  fs.chmodSync(file, mode);
}

function verifySignedBinary(binary, target) {
  const absolute = path.resolve(binary);
  const stat = fs.statSync(absolute);
  if (!stat.isFile()) {
    fail(`Binary is not a regular file: ${absolute}`);
  }
  run('/usr/bin/codesign', ['--verify', '--strict', '--verbose=2', absolute]);
  const architectures = run('/usr/bin/lipo', ['-archs', absolute]).split(/\s+/);
  if (!architectures.includes(target.machoArch)) {
    fail(
      `Binary ${absolute} does not contain required Mach-O architecture ${target.machoArch}; ` +
        `found ${architectures.join(', ')}`,
    );
  }
  if (architectures.length !== 1) {
    fail(
      `Platform packages require a thin ${target.machoArch} Mach-O; found ${architectures.join(', ')}`,
    );
  }
  return absolute;
}

function npmEnvironment(cache) {
  return {
    ...process.env,
    npm_config_audit: 'false',
    npm_config_cache: cache,
    npm_config_fund: 'false',
    npm_config_registry: 'http://127.0.0.1:9',
    npm_config_update_notifier: 'false',
  };
}

function npmPack(source, destination, cache) {
  fs.mkdirSync(destination, { recursive: true });
  const output = run(
    'npm',
    ['pack', source, '--ignore-scripts', '--json', '--pack-destination', destination],
    { env: npmEnvironment(cache) },
  );
  let report;
  try {
    report = JSON.parse(output);
  } catch (error) {
    fail(`npm pack returned invalid JSON: ${error.message}`);
  }
  if (!Array.isArray(report) || report.length !== 1 || !report[0].filename) {
    fail(`npm pack returned an unexpected report: ${output}`);
  }
  return path.join(destination, report[0].filename);
}

function copyPackageSource(source, destination) {
  fs.cpSync(source, destination, { recursive: true });
}

function buildPlatformPackage({ arch, binary, outputDirectory }) {
  const target = targetFor(arch);
  const signedBinary = verifySignedBinary(binary, target);
  const output = path.resolve(outputDirectory);
  fs.mkdirSync(output, { recursive: true });
  const temporary = fs.mkdtempSync(path.join(os.tmpdir(), 'shared-context-platform-'));
  try {
    const stage = path.join(temporary, target.sourceDirectory);
    copyPackageSource(path.join(PACKAGES_ROOT, target.sourceDirectory), stage);
    const packagedBinary = path.join(stage, 'bin/sctx');
    fs.mkdirSync(path.dirname(packagedBinary), { recursive: true });
    fs.copyFileSync(signedBinary, packagedBinary);
    fs.chmodSync(packagedBinary, 0o755);
    const binarySha256 = sha256File(packagedBinary);
    writeFile(`${packagedBinary}.sha256`, `${binarySha256}  sctx\n`);

    const packed = npmPack(stage, output, path.join(temporary, 'npm-cache'));
    const tarballName = `shared-context-darwin-${arch}.tgz`;
    const tarball = path.join(output, tarballName);
    if (fs.existsSync(tarball)) {
      fs.rmSync(tarball);
    }
    fs.renameSync(packed, tarball);

    const packageJson = JSON.parse(fs.readFileSync(path.join(stage, 'package.json'), 'utf8'));
    const manifest = {
      formatVersion: 1,
      package: target.packageName,
      version: packageJson.version,
      target: {
        os: 'darwin',
        cpu: target.cpu,
        machoArchitecture: target.machoArch,
      },
      binary: {
        path: 'bin/sctx',
        sha256: binarySha256,
        size: fs.statSync(packagedBinary).size,
        signatureVerification: 'codesign --verify --strict --verbose=2',
      },
      tarball: {
        file: tarballName,
        sha256: sha256File(tarball),
        size: fs.statSync(tarball).size,
      },
    };
    const manifestPath = path.join(output, `shared-context-darwin-${arch}.manifest.json`);
    writeFile(manifestPath, canonicalJson(manifest));
    return { manifest, manifestPath, tarball };
  } finally {
    fs.rmSync(temporary, { recursive: true, force: true });
  }
}

function tarHeader(name, size, mode, type) {
  if (Buffer.byteLength(name) > 100) {
    fail(`Deterministic tar path is too long: ${name}`);
  }
  const header = Buffer.alloc(512);
  header.write(name, 0, 100, 'utf8');
  writeOctal(header, mode, 100, 8);
  writeOctal(header, 0, 108, 8);
  writeOctal(header, 0, 116, 8);
  writeOctal(header, size, 124, 12);
  writeOctal(header, 0, 136, 12);
  header.fill(0x20, 148, 156);
  header.write(type, 156, 1, 'ascii');
  header.write('ustar\0', 257, 6, 'ascii');
  header.write('00', 263, 2, 'ascii');
  const checksum = header.reduce((sum, byte) => sum + byte, 0);
  const encoded = checksum.toString(8).padStart(6, '0');
  header.write(encoded, 148, 6, 'ascii');
  header[154] = 0;
  header[155] = 0x20;
  return header;
}

function writeOctal(buffer, value, offset, length) {
  const encoded = value.toString(8).padStart(length - 1, '0');
  if (encoded.length >= length) {
    fail(`Value ${value} does not fit in a ${length}-byte tar field`);
  }
  buffer.write(encoded, offset, length - 1, 'ascii');
  buffer[offset + length - 1] = 0;
}

function listFiles(root) {
  const entries = [];
  function visit(relative) {
    const absolute = path.join(root, relative);
    for (const entry of fs.readdirSync(absolute, { withFileTypes: true })) {
      const child = relative ? path.join(relative, entry.name) : entry.name;
      if (entry.isDirectory()) {
        entries.push({ relative: `${child}/`, directory: true });
        visit(child);
      } else if (entry.isFile()) {
        entries.push({ relative: child, directory: false });
      } else {
        fail(`Unsupported bundle entry type: ${child}`);
      }
    }
  }
  visit('');
  return entries.sort((left, right) => {
    if (left.relative < right.relative) {
      return -1;
    }
    return left.relative > right.relative ? 1 : 0;
  });
}

function createDeterministicTarGz(sourceDirectory, archive, topLevelName) {
  const chunks = [tarHeader(`${topLevelName}/`, 0, 0o755, '5')];
  for (const entry of listFiles(sourceDirectory)) {
    const archivePath = `${topLevelName}/${entry.relative}`;
    if (entry.directory) {
      chunks.push(tarHeader(archivePath, 0, 0o755, '5'));
      continue;
    }
    const contents = fs.readFileSync(path.join(sourceDirectory, entry.relative));
    const mode = entry.relative === 'install' ? 0o755 : 0o644;
    chunks.push(tarHeader(archivePath, contents.length, mode, '0'));
    chunks.push(contents);
    const padding = (512 - (contents.length % 512)) % 512;
    if (padding) {
      chunks.push(Buffer.alloc(padding));
    }
  }
  chunks.push(Buffer.alloc(1024));
  const compressed = zlib.gzipSync(Buffer.concat(chunks), { level: 9, mtime: 0 });
  writeFile(archive, compressed);
}

module.exports = {
  NPM_ROOT,
  PACKAGES_ROOT,
  TARGETS,
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
};
