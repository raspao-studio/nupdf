#!/usr/bin/env node
'use strict';
// Runs the `nupdf` binary that npm installed for this machine, passing on its
// arguments, input, output and exit code.
const { spawnSync } = require('node:child_process');
const { currentLibc, packageFor, binaryName } = require('../platform.js');

const libc = currentLibc();
const pkg = packageFor(process.platform, process.arch, libc);
if (!pkg) {
  const machine = [process.platform, process.arch, libc].filter(Boolean).join(' ');
  console.error(`nupdf: there is no prebuilt binary for ${machine}. Build it with: cargo install --locked nupdf-cli`);
  process.exit(1);
}

let binary;
try {
  binary = require.resolve(`${pkg.name}/${binaryName(process.platform)}`);
} catch {
  console.error(
    `nupdf: the package ${pkg.name}, which carries the binary for this machine, is not installed. ` +
      'Reinstall nupdf-cli without --omit=optional or --no-optional.',
  );
  process.exit(1);
}

const result = spawnSync(binary, process.argv.slice(2), { stdio: 'inherit' });
if (result.error) {
  console.error(`nupdf: could not run ${binary}: ${result.error.message}`);
  process.exit(1);
}
if (result.signal) process.kill(process.pid, result.signal);
process.exit(result.status === null ? 1 : result.status);
