# Test data

Inputs that unit tests read with `include_bytes!`, too large or too
specific to build in the test itself.

- `jbig2-endless-export-runs.jb2`: an embedded JBIG2 page whose symbol
  dictionary decodes as endless empty export runs (found by the `images`
  fuzz target; see crates/nupdf/src/hayro_jbig2/NUPDF.md).
