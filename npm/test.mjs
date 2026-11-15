// node --test npm/test.mjs
// The end-to-end test needs a built binary for this machine: NUPDF_BIN, or
// target/release/nupdf.
import assert from 'node:assert/strict';
import { execFileSync, spawnSync } from 'node:child_process';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, writeFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { build } from './build.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, '..');
const { PACKAGES, currentLibc, packageFor, binaryName } = createRequire(import.meta.url)('./nupdf/platform.js');
const npm = process.platform === 'win32' ? 'npm.cmd' : 'npm';
const shell = process.platform === 'win32';

test('the packages are the targets the release workflow builds', () => {
  const workflow = readFileSync(join(root, '.github/workflows/release.yml'), 'utf8');
  const built = new Set([...workflow.matchAll(/- \{ target: ([\w-]+), os: [^}]*run: (?:true|false) \}/g)].map((m) => m[1]));
  assert.deepEqual(new Set(PACKAGES.map((p) => p.target)), built);
});

test('each machine gets one package, and an unknown one gets none', () => {
  assert.equal(packageFor('darwin', 'arm64', undefined).name, '@nupdf/darwin-arm64');
  assert.equal(packageFor('linux', 'x64', 'glibc').name, '@nupdf/linux-x64-gnu');
  assert.equal(packageFor('linux', 'arm64', 'musl').name, '@nupdf/linux-arm64-musl');
  assert.equal(packageFor('win32', 'x64', undefined).name, '@nupdf/win32-x64');
  assert.equal(packageFor('win32', 'arm64', undefined), undefined);
  assert.equal(packageFor('freebsd', 'x64', undefined), undefined);
  const keys = PACKAGES.map((p) => `${p.os} ${p.cpu} ${p.libc}`);
  assert.equal(new Set(keys).size, keys.length);
});

test('the build refuses a bad version, an unknown target and a missing one', () => {
  const out = mkdtempSync(join(tmpdir(), 'nupdf-npm-'));
  assert.throws(() => build({ version: 'v1', out, binaries: {} }), /not a version/);
  assert.throws(() => build({ version: '1.2.3', out, binaries: { 'riscv64-linux': 'x' } }), /no package is built from/);
  assert.throws(() => build({ version: '1.2.3', out, binaries: {}, allTargets: true }), /no binary given for/);
});

const host = packageFor(process.platform, process.arch, currentLibc());
const binary = resolve(process.env.NUPDF_BIN ?? join(root, 'target/release', binaryName(process.platform)));
const skip = !host ? 'no package for this machine' : !existsSync(binary) ? `no binary at ${binary}` : false;

test('installed from the packed packages, `nupdf` runs the binary', { skip }, () => {
  const out = mkdtempSync(join(tmpdir(), 'nupdf-npm-'));
  const dirs = build({ version: '9.9.9', out, binaries: { [host.target]: binary } });

  const main = JSON.parse(readFileSync(join(out, 'nupdf/package.json'), 'utf8'));
  assert.equal(main.name, 'nupdf-cli');
  assert.equal(main.version, '9.9.9');
  assert.deepEqual(main.optionalDependencies, Object.fromEntries(PACKAGES.map((p) => [p.name, '9.9.9'])));

  const packed = join(out, 'packed');
  mkdirSync(packed);
  for (const dir of dirs) execFileSync(npm, ['pack', dir, '--pack-destination', packed, '--silent'], { shell, stdio: 'ignore' });
  const tarballs = readdirSync(packed).map((f) => join(packed, f));
  assert.equal(tarballs.length, 2);
  assert.ok(tarballs.some((f) => f.endsWith('nupdf-cli-9.9.9.tgz')));

  // The other platforms' packages are not anywhere npm can find them; being
  // optional, they are passed over.
  const project = mkdtempSync(join(tmpdir(), 'nupdf-npm-project-'));
  writeFileSync(join(project, 'package.json'), '{"name":"project","private":true}\n');
  execFileSync(npm, ['install', '--no-audit', '--no-fund', '--silent', ...tarballs], { shell, cwd: project, stdio: 'inherit' });

  const bin = join(project, 'node_modules/.bin', process.platform === 'win32' ? 'nupdf.cmd' : 'nupdf');
  const version = spawnSync(bin, ['--version'], { shell, encoding: 'utf8' });
  assert.equal(version.status, 0, version.stderr);
  assert.match(version.stdout, /^nupdf \d+\.\d+\.\d+/);

  const info = spawnSync(bin, ['info', join(root, 'corpus/smoke/minimal.pdf')], { shell, encoding: 'utf8' });
  assert.equal(info.status, 0, info.stderr);
  assert.equal(JSON.parse(info.stdout).page_count, 1);

  // The binary's exit code is the command's: 3 is "no such file" (AGENT-INTERFACE).
  const missing = spawnSync(bin, ['info', join(project, 'absent.pdf')], { shell, encoding: 'utf8' });
  assert.notEqual(missing.status, 0);
  const direct = spawnSync(binary, ['info', join(project, 'absent.pdf')], { encoding: 'utf8' });
  assert.equal(missing.status, direct.status);
});
