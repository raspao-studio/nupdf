# The wild tier

How nupdf fares on PDFs from the open web: 100,000 files from SafeDocs' CC-MAIN-2021-31-PDF-UNTRUNCATED crawl (zip files 0000 to 0099), unsanitized, malware included. They are not committed; `corpus/manifest.toml` says where they come from.

Measured on 2026-11-12 at commit de4b887, MuPDF 1.28.6 as the reference, on an M-series Mac.

## Crashes

`cargo xtask crashes --tier wild` runs fifteen nupdf commands on each file, each in its own process with a 60-second limit: `info`, `objects`, `extract`, `search`, `render` (pages 1 to 3 at 36 dpi), `fonts`, `images`, `tables`, `annots`, `forms`, `signatures`, `outline`, `clean`, `flatten` and `sanitize`. That is 1,500,000 runs.

| | First sweep | After the fixes |
|---|---|---|
| Signals (segfaults, aborts, stack overflows) | 0 | 0 |
| Panics | 62 | 0 |
| Over 60 seconds | 692 | 5 |

- **The panics** were one bug: a stroke under an empty clip, where Rust's `f32::clamp` panics on a minimum above its maximum and MuPDF's `fz_clamp` gives the minimum.
- **About half the timeouts** were the machine: the sweep ran fourteen processes at once alongside other work, and those files take seconds on their own.
- **The rest** were two slow paths, both fixed:
  - Shadings hashed a whole embedded ICC profile for every vertex colour.
  - Pages that set an ICC colour space every few operators inflated its profile each time.
  
  MuPDF keeps both in its store; nupdf now does too.
- **The five runs still over 60 seconds** finish, slower than MuPDF, on files of 4 to 43 MB:
  - `tables` (98 s and 110 s against 11 s and 24 s);
  - `sanitize` (48 s against 9 s);
  - `images` (61 s against 54 s);
  - `fonts` (61 s).
  
  They are performance work to come, not hangs.

## Parity

`cargo xtask harness --tier wild --sample 1000` on an evenly spaced sample of 1,000 files. "Both failed" means MuPDF and nupdf both refuse the file; parity counts matches among the files either side reads.

| Check | Match | Mismatch | Oracle failed | Both failed | Parity |
|---|---|---|---|---|---|
| info | 994 | 4 | 0 | 2 | 99.6% |
| objects | 998 | 0 | 0 | 2 | 100% |
| pages | 998 | 0 | 0 | 2 | 100% |
| streams | 998 | 0 | 0 | 2 | 100% |
| images | 998 | 0 | 0 | 2 | 100% |
| operators | 998 | 0 | 0 | 2 | 100% |
| stext | 991 | 2 | 5 | 2 | 99.8% |
| text | 993 | 4 | 1 | 2 | 99.6% |
| search | 998 | 0 | 0 | 2 | 100% |
| fonts | 998 | 0 | 0 | 2 | 100% |
| outline | 998 | 0 | 0 | 2 | 100% |
| links | 998 | 0 | 0 | 2 | 100% |
| annots | 998 | 0 | 0 | 2 | 100% |
| forms | 998 | 0 | 0 | 2 | 100% |
| signatures | 997 | 1 | 0 | 2 | 99.9% |
| tables | 997 | 1 | 0 | 2 | 99.9% |
| clean (BLAKE3 of the written file) | 988 | 7 | 0 | 5 | 99.3% |
| flatten (BLAKE3 of the written file) | 988 | 7 | 0 | 5 | 99.3% |
| render-72 (each page's pixels within the threshold) | 776 files | 222 files | 0 | 2 | 77.8% of files; 12,256 of 13,041 pages (94.0%) |

Rendering is the one that falls short. 94.0% of the pages are within the threshold (with colour management off on both sides), against 99.3% on the standard tier, where M7 asked for 98%. Why the open web's pages differ more has not been triaged yet (`cargo xtask triage FILE --page N` is the tool for it); that is the rendering work left.

`clean` and `flatten` compare written files byte for byte, so AES-encrypted files always differ there: AES writes a random initialization vector.

## Reproducing

```bash
cargo xtask crashes --tier wild --jobs 8
cargo xtask crashes --tier wild --summary
cargo xtask harness --tier wild --sample 1000 --check info
```

The sweep logs every run to `target/crashes/wild.jsonl` and resumes with `--resume`.
