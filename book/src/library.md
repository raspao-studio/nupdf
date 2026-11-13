# As a library

The engine is the `nupdf` crate; the CLI is a thin layer over it. Add it to a project's `Cargo.toml`, from crates.io once 0.1.0 is published:

```toml
[dependencies]
nupdf = "0.1"
```

or from the repository:

```toml
[dependencies]
nupdf = { git = "https://github.com/raspao-studio/nupdf" }
```

This program opens a file, extracts the structured text of every page on four threads, searches each page, and prints what the file got wrong:

```rust,ignore
use nupdf::{Document, parallel, search, stext};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).ok_or("usage: scan <file.pdf>")?;
    let doc = Document::open(std::fs::read(path)?)?;
    let count = doc.page_count().map_err(|e| e.0)?;
    let pages: Vec<i32> = (0..count).collect();

    // Structured text of every page, on up to 4 threads, in page order.
    let opts = stext::Options::default();
    let results = parallel::extract(&doc, &pages, opts, Some(4));
    for (i, page) in results.into_iter().enumerate() {
        let page = page?;
        let hits = search::search_page(&page, "hello", search::options::IGNORE_CASE)?;
        println!("page {}: {} hit(s)", i + 1, hits.len());
        print!("{}", page.text());
    }

    // What was wrong with the file, as data rather than as a failure.
    for d in doc.diagnostics().snapshot() {
        eprintln!("{}: {}", d.code, d.message);
    }
    Ok(())
}
```

A few things to know:

- **A `Document` is shared across threads.** It is parsed once and pages are worked on at once; `parallel::extract` with `None` instead of `Some(4)` runs the pages one after another, as MuPDF does, with the same results.
- **Pages are 0-based in the library** (`doc.page(0)` is the first), 1-based in the CLI.
- **Limits.** `Document::open` uses the default `Limits`; `Document::open_with(data, limits)` sets the largest decoded stream, xref table and the rest, as the CLI's `--max-*` options do.
- **Diagnostics are data.** A damaged file opens if MuPDF would open it, and what nupdf repaired or skipped is in `doc.diagnostics()`, with the same stable codes the CLI reports.
- **The modules follow MuPDF's.** `stext` is `fz_stext`, `search` is MuPDF's search, `redact`, `write`, `form` and the rest likewise; the [architecture](docs/ARCHITECTURE.md) maps each module to the MuPDF file it comes from.

The library's API is not yet stable: until 1.0 a minor release may change it. The CLI's JSON reports are the stable interface, versioned per schema ([agent interface](docs/AGENT-INTERFACE.md)).
