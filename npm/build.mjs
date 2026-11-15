// Writes the npm packages for a release: one per platform, each carrying that
// platform's `nupdf` binary, and the main `nupdf-cli` package that depends on
// them all and runs the right one. (npm refuses the name `nupdf` as too close
// to `jspdf` and `unpdf`.)
//
//   node npm/build.mjs --version 0.1.0 --out dist/npm \
//     --binary aarch64-apple-darwin=target/aarch64-apple-darwin/release/nupdf ...
//
// A target with no --binary gets no package, but the main package still lists
// it, so a release must pass all seven (--all-targets makes a missing one an
// error).
import { chmodSync, copyFileSync, cpSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, '..');
const { PACKAGES, binaryName } = createRequire(import.meta.url)('./nupdf/platform.js');

export function build({ version, out, binaries, allTargets = false }) {
  if (!/^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/.test(version ?? '')) throw new Error(`not a version: ${version}`);
  for (const target of Object.keys(binaries)) {
    if (!PACKAGES.some((p) => p.target === target)) throw new Error(`no package is built from ${target}`);
  }
  const missing = PACKAGES.filter((p) => !binaries[p.target]).map((p) => p.target);
  if (allTargets && missing.length) throw new Error(`no binary given for ${missing.join(', ')}`);

  const main = JSON.parse(readFileSync(join(here, 'nupdf', 'package.json'), 'utf8'));
  const dirs = [];

  for (const p of PACKAGES) {
    if (!binaries[p.target]) continue;
    const dir = join(out, p.name.replace('@nupdf/', 'nupdf-'));
    rmSync(dir, { recursive: true, force: true });
    mkdirSync(dir, { recursive: true });
    const binary = binaryName(p.os);
    copyFileSync(binaries[p.target], join(dir, binary));
    chmodSync(join(dir, binary), 0o755);
    for (const f of ['LICENSE', 'NOTICE.md']) copyFileSync(join(root, f), join(dir, f));
    writeFileSync(join(dir, 'README.md'), `The \`nupdf\` binary for ${p.target}. Install [nupdf-cli](https://www.npmjs.com/package/nupdf-cli), which picks the right one.\n`);
    const manifest = {
      name: p.name,
      version,
      description: `The nupdf binary for ${p.target}`,
      homepage: main.homepage,
      repository: main.repository,
      license: main.license,
      os: [p.os],
      cpu: [p.cpu],
      ...(p.libc ? { libc: [p.libc] } : {}),
      files: [binary, 'README.md', 'LICENSE', 'NOTICE.md'],
      preferUnplugged: true,
    };
    writeFileSync(join(dir, 'package.json'), JSON.stringify(manifest, null, 2) + '\n');
    dirs.push(dir);
  }

  const dir = join(out, 'nupdf');
  rmSync(dir, { recursive: true, force: true });
  cpSync(join(here, 'nupdf'), dir, { recursive: true });
  for (const f of ['README.md', 'LICENSE', 'NOTICE.md']) copyFileSync(join(root, f), join(dir, f));
  main.version = version;
  main.optionalDependencies = Object.fromEntries(PACKAGES.map((p) => [p.name, version]));
  writeFileSync(join(dir, 'package.json'), JSON.stringify(main, null, 2) + '\n');
  dirs.push(dir);
  return dirs;
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const args = process.argv.slice(2);
  const opts = { binaries: {} };
  for (let i = 0; i < args.length; i++) {
    if (args[i] === '--version') opts.version = args[++i];
    else if (args[i] === '--out') opts.out = args[++i];
    else if (args[i] === '--all-targets') opts.allTargets = true;
    else if (args[i] === '--binary') {
      const [target, path] = args[++i].split('=');
      opts.binaries[target] = path;
    } else throw new Error(`unknown argument ${args[i]}`);
  }
  if (!opts.out) throw new Error('--out DIR is needed');
  for (const dir of build(opts)) console.log(dir);
}
