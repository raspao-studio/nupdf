#!/bin/bash -eu
# OSS-Fuzz build for nupdf (projects/nupdf/build.sh upstream): every fuzz
# target, with the smoke fixtures and base fonts as seed corpora, as CI
# seeds them.
cd "$SRC/nupdf"
cargo fuzz build -O
for target in $(cargo fuzz list); do
  cp "fuzz/target/x86_64-unknown-linux-gnu/release/$target" "$OUT/"
done
seed() { (cd "$2" && zip -q -r "$OUT/$1_seed_corpus.zip" .); }
tmp=$(mktemp -d)
for t in open_walk run stext redact render navigation forms; do
  mkdir -p "$tmp/$t"
  for f in corpus/smoke/*.pdf; do
    if [ "$t" = stext ]; then (printf '\000'; cat "$f") > "$tmp/$t/$(basename "$f")"; else cp "$f" "$tmp/$t/"; fi
  done
  seed "$t" "$tmp/$t"
done
mkdir -p "$tmp/font"
for f in crates/nupdf/fonts/urw/*.cff; do
  (printf '\000'; cat "$f") > "$tmp/font/$(basename "$f").0"
  (printf '\004'; cat "$f") > "$tmp/font/$(basename "$f").4"
done
seed font "$tmp/font"
