'use strict';
// Which package carries the `nupdf` binary for which machine. The main package
// lists every one as an optional dependency, and npm installs only the one
// whose `os`, `cpu` and `libc` match. `target` is the Rust target the release
// workflow builds it from.
const PACKAGES = [
  { name: '@nupdf/darwin-arm64', target: 'aarch64-apple-darwin', os: 'darwin', cpu: 'arm64' },
  { name: '@nupdf/darwin-x64', target: 'x86_64-apple-darwin', os: 'darwin', cpu: 'x64' },
  { name: '@nupdf/linux-arm64-gnu', target: 'aarch64-unknown-linux-gnu', os: 'linux', cpu: 'arm64', libc: 'glibc' },
  { name: '@nupdf/linux-arm64-musl', target: 'aarch64-unknown-linux-musl', os: 'linux', cpu: 'arm64', libc: 'musl' },
  { name: '@nupdf/linux-x64-gnu', target: 'x86_64-unknown-linux-gnu', os: 'linux', cpu: 'x64', libc: 'glibc' },
  { name: '@nupdf/linux-x64-musl', target: 'x86_64-unknown-linux-musl', os: 'linux', cpu: 'x64', libc: 'musl' },
  { name: '@nupdf/win32-x64', target: 'x86_64-pc-windows-msvc', os: 'win32', cpu: 'x64' },
];

// Node reports the glibc it runs on; on musl (Alpine) it reports none.
function currentLibc() {
  if (process.platform !== 'linux') return undefined;
  const header = process.report && process.report.getReport().header;
  return header && header.glibcVersionRuntime ? 'glibc' : 'musl';
}

function packageFor(os, cpu, libc) {
  return PACKAGES.find((p) => p.os === os && p.cpu === cpu && p.libc === libc);
}

function binaryName(os) {
  return os === 'win32' ? 'nupdf.exe' : 'nupdf';
}

module.exports = { PACKAGES, currentLibc, packageFor, binaryName };
