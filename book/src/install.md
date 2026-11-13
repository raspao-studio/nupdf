# Install

nupdf is one binary, `nupdf`, from the `nupdf-cli` crate. It needs Rust 1.88 or newer to build ([rustup](https://rustup.rs) installs it).

From crates.io, once 0.1.0 is published:

```bash
cargo install --locked nupdf-cli
```

From the repository, at any time:

```bash
cargo install --locked --git https://github.com/raspao-studio/nupdf nupdf-cli
```

Check it runs:

```bash
nupdf --version
```

Nothing else is needed. The one exception is `ocr`, which runs [Tesseract](https://github.com/tesseract-ocr/tesseract) for scanned pages and finds it on `PATH` (or where `--tesseract` says).
