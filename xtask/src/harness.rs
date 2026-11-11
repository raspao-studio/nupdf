// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! Differential harness. Each check runs an oracle script under `mutool run`
//! and the matching `nupdf` command on every file in a corpus tier, compares
//! the JSON they print, and ratchets the results against a committed baseline.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::proc::{self, Status};
use crate::{corpus, oracle};

#[derive(Debug)]
pub struct Check {
    pub name: &'static str,
    pub about: &'static str,
    /// Script under oracle/js, run as `mutool run <script> <file>`.
    pub oracle_script: &'static str,
    /// `nupdf` arguments; the file path is appended, or takes the place of
    /// each `{file}` among them. `{dir}` is a scratch directory for files
    /// nupdf writes; the `"file"` fields of its report name them, and are
    /// replaced by their BLAKE3 hashes as the oracle's are.
    pub nupdf_args: &'static [&'static str],
    pub compare: Compare,
    /// The oracle takes a scratch directory as a second argument and names
    /// files it wrote there in `"file"` fields. The harness replaces each
    /// with `"blake3"`, the hash of that file, before comparing.
    pub oracle_writes_files: bool,
    /// Applied to each file the oracle wrote before it is hashed.
    pub oracle_file_fixup: Option<fn(&mut [u8])>,
    /// Applied to nupdf's report before it is compared: drops what nupdf
    /// reports that the oracle has no way to.
    pub nupdf_fixup: Option<fn(&mut Value)>,
    /// Further oracle arguments, after the file (and the scratch directory).
    pub oracle_args: &'static [&'static str],
    /// Compare what a reader makes of each side's output instead of the
    /// output itself. The oracle script names the file it wrote in its
    /// scratch directory (`clean.pdf` unless it says otherwise); `nupdf`
    /// writes with `-o`. The reader then runs on each written file.
    pub reread: Option<Reader>,
}

/// What reads each side's output back.
#[derive(Debug, Clone, Copy)]
pub enum Reader {
    /// This MuPDF script, with a scratch directory of its own; MuPDF's
    /// distinct warnings while reading are added as `/warnings`.
    Mutool(&'static str),
    /// `qpdf --check`: its exit status (0, or 2 for errors, 3 for
    /// warnings) as `/qpdf/status`, and what it printed, with the file's
    /// path taken out, as `/qpdf/messages`.
    Qpdf,
    /// nupdf's reading checked against the Arlington rules nupdf applies
    /// (`arlington`): the violations, as `/arlington/violations`.
    Arlington,
}

#[derive(Debug, Clone, Copy)]
pub enum Compare {
    /// These JSON pointers must be equal.
    Fields(&'static [&'static str]),
    /// The `counts` must be equal, then every entry of the `items`
    /// collection (an object or an array), reported item by item.
    Collection {
        counts: &'static [&'static str],
        items: &'static str,
    },
    /// The distinct warnings and reported errors on MuPDF's stderr must
    /// equal the distinct `/diagnostics/*/message` values.
    Warnings,
    /// Each page's device calls (`/pages/*/events`) and error must match,
    /// text included, with numbers within a few float steps: MuPDF's build fuses multiply-adds, so a
    /// matrix or a bounding box can differ in its last bits.
    Trace,
}

pub const CHECKS: &[Check] = &[
    Check {
        name: "info",
        about: "document-level facts: effective version, encryption, page count, xref size, repair",
        oracle_script: "oracle/js/info.js",
        nupdf_args: &["info"],
        compare: Compare::Fields(&[
            "/pdf_version",
            "/encryption",
            "/needs_password",
            "/permissions",
            "/page_count",
            "/object_count",
            "/repaired",
            "/revisions",
            "/metadata",
        ]),
        oracle_writes_files: false,
        oracle_file_fixup: None,
        nupdf_fixup: None,
        reread: None,
        oracle_args: &[],
    },
    Check {
        name: "objects",
        about: "every object in the xref, canonically encoded, with raw stream lengths",
        oracle_script: "oracle/js/objects.js",
        nupdf_args: &["objects"],
        compare: Compare::Collection {
            counts: &["/object_count"],
            items: "objects",
        },
        oracle_writes_files: false,
        oracle_file_fixup: None,
        nupdf_fixup: None,
        reread: None,
        oracle_args: &[],
    },
    Check {
        name: "pages",
        about: "page tree lookup and inherited MediaBox, CropBox, Rotate, Resources",
        oracle_script: "oracle/js/pages.js",
        nupdf_args: &["pages"],
        compare: Compare::Collection {
            counts: &["/page_count", "/page_count_after"],
            items: "pages",
        },
        oracle_writes_files: false,
        oracle_file_fixup: None,
        nupdf_fixup: None,
        reread: None,
        oracle_args: &[],
    },
    Check {
        name: "warnings",
        about: "distinct warnings and recovered errors while opening and inspecting a file",
        oracle_script: "oracle/js/info.js",
        nupdf_args: &["info"],
        compare: Compare::Warnings,
        oracle_writes_files: false,
        oracle_file_fixup: None,
        nupdf_fixup: None,
        reread: None,
        oracle_args: &[],
    },
    Check {
        name: "streams",
        about: "every stream decoded for non-image use: length and BLAKE3 of the data, or the error",
        oracle_script: "oracle/js/streams.js",
        nupdf_args: &["streams"],
        compare: Compare::Collection {
            counts: &["/object_count"],
            items: "streams",
        },
        oracle_writes_files: true,
        oracle_file_fixup: None,
        nupdf_fixup: None,
        reread: None,
        oracle_args: &[],
    },
    Check {
        name: "images",
        about: "every stream with an image filter, decoded: length and BLAKE3 of the samples, or the error",
        oracle_script: "oracle/js/images.js",
        nupdf_args: &["streams", "--images"],
        compare: Compare::Collection {
            counts: &["/object_count"],
            items: "streams",
        },
        oracle_writes_files: true,
        oracle_file_fixup: None,
        nupdf_fixup: None,
        reread: None,
        oracle_args: &[],
    },
    Check {
        name: "operators",
        about: "each page's content-stream operators with their operands, as MuPDF's interpreter hands them to a processor, with the page's resources and annotations taken away so that only its content streams count",
        oracle_script: "oracle/js/operators.js",
        nupdf_args: &["operators", "--no-resources"],
        compare: Compare::Collection {
            counts: &["/page_count"],
            items: "pages",
        },
        oracle_writes_files: false,
        oracle_file_fixup: None,
        nupdf_fixup: None,
        reread: None,
        oracle_args: &["bare"],
    },
    Check {
        name: "operators-warnings",
        about: "distinct warnings and recovered errors while doing the same, the content's syntax errors among them",
        oracle_script: "oracle/js/operators.js",
        nupdf_args: &["operators", "--no-resources"],
        compare: Compare::Warnings,
        oracle_writes_files: false,
        oracle_file_fixup: None,
        nupdf_fixup: None,
        reread: None,
        oracle_args: &["bare"],
    },
    Check {
        name: "operators-resources",
        about: "the same with each page's resources kept: which operators are dispatched once the colorspaces, functions, shadings and patterns they name are loaded",
        oracle_script: "oracle/js/operators.js",
        nupdf_args: &["operators"],
        compare: Compare::Collection {
            counts: &["/page_count"],
            items: "pages",
        },
        oracle_writes_files: false,
        oracle_file_fixup: None,
        nupdf_fixup: None,
        reread: None,
        oracle_args: &["resources"],
    },
    Check {
        name: "fonts",
        about: "the fonts each page's resources reach, as `mutool info` lists them: page, object, subtype, name and encoding",
        oracle_script: "oracle/js/inventory.js",
        nupdf_args: &["fonts"],
        compare: Compare::Fields(&["/page_count", "/fonts", "/page_errors"]),
        oracle_writes_files: true,
        oracle_file_fixup: None,
        nupdf_fixup: Some(drop_font_loads),
        reread: None,
        oracle_args: &["fonts"],
    },
    Check {
        name: "font-files",
        about: "each font descriptor's embedded program as `mutool extract` writes it: file name and BLAKE3",
        oracle_script: "oracle/js/inventory.js",
        nupdf_args: &["fonts", "--extract", "{dir}"],
        compare: Compare::Collection {
            counts: &["/object_count"],
            items: "extracted",
        },
        oracle_writes_files: true,
        oracle_file_fixup: None,
        nupdf_fixup: None,
        reread: None,
        oracle_args: &["font-files"],
    },
    Check {
        name: "image-list",
        about: "the images each page's resources reach, as `mutool info` lists them: page, object, size, depth, filters and colorspace",
        oracle_script: "oracle/js/inventory.js",
        nupdf_args: &["images"],
        compare: Compare::Fields(&["/page_count", "/images", "/page_errors"]),
        oracle_writes_files: true,
        oracle_file_fixup: None,
        nupdf_fixup: None,
        reread: None,
        oracle_args: &["images"],
    },
    Check {
        name: "image-pixmaps",
        about: "each image XObject decoded to a pixmap as fz_get_pixmap_from_image does: size, components, alpha, colorspace and the MD5 of the samples, or the error",
        oracle_script: "oracle/js/inventory.js",
        nupdf_args: &["images", "--digests"],
        compare: Compare::Collection {
            counts: &["/object_count"],
            items: "pixmaps",
        },
        oracle_writes_files: true,
        oracle_file_fixup: None,
        nupdf_fixup: None,
        reread: None,
        oracle_args: &["pixmaps"],
    },
    Check {
        name: "trace",
        about: "each page's device calls as MuPDF's run processor makes them for the page's contents (paths, clips, colors, images, shadings, text, groups, masks, layers, structure)",
        oracle_script: "oracle/js/trace.js",
        nupdf_args: &["trace"],
        compare: Compare::Trace,
        oracle_writes_files: false,
        oracle_file_fixup: None,
        nupdf_fixup: None,
        reread: None,
        oracle_args: &[],
    },
    Check {
        name: "clean",
        about: "the document rewritten as `mutool clean` writes it: BLAKE3 of the output",
        oracle_script: "oracle/js/clean.js",
        // The bytes MuPDF writes, whether or not they read back the same.
        nupdf_args: &["clean", "--dry-run", "--no-verify"],
        compare: Compare::Fields(&["/output/blake3"]),
        oracle_writes_files: true,
        oracle_file_fixup: Some(name_nupdf_as_writer),
        nupdf_fixup: None,
        reread: None,
        oracle_args: &[],
    },
    Check {
        name: "roundtrip",
        about: "the `clean` output of each side as MuPDF reads it back: objects, raw stream data, repair, warnings",
        oracle_script: "oracle/js/clean.js",
        nupdf_args: &["clean", "--no-verify"],
        compare: Compare::Collection {
            counts: &["/object_count", "/repaired", "/warnings"],
            items: "objects",
        },
        oracle_writes_files: true,
        oracle_file_fixup: None,
        nupdf_fixup: None,
        reread: Some(Reader::Mutool("oracle/js/objects.js")),
        oracle_args: &[],
    },
    clean_gc(
        "clean-g",
        &["clean", "-g", "--dry-run", "--no-verify"],
        &["garbage=1"],
    ),
    clean_gc(
        "clean-gg",
        &["clean", "-gg", "--dry-run", "--no-verify"],
        &["garbage=2"],
    ),
    clean_gc(
        "clean-ggg",
        &["clean", "-ggg", "--dry-run", "--no-verify"],
        &["garbage=3"],
    ),
    clean_gc(
        "clean-gggg",
        &["clean", "-gggg", "--dry-run", "--no-verify"],
        &["garbage=4"],
    ),
    // mutool clean turns pretty printing on for -a and -d without -z; the
    // write options MuPDF's script interface takes do not.
    clean_streams(
        "clean-d",
        &["clean", "-d", "--dry-run", "--no-verify"],
        &["decompress,pretty"],
    ),
    clean_streams(
        "clean-z",
        &["clean", "-z", "--dry-run", "--no-verify"],
        &["compress"],
    ),
    clean_streams(
        "clean-ze",
        &["clean", "-z", "-e", "100", "--dry-run", "--no-verify"],
        &["compress,compression-effort=100"],
    ),
    clean_streams(
        "clean-a",
        &["clean", "-a", "--dry-run", "--no-verify"],
        &["ascii,pretty"],
    ),
    clean_streams(
        "clean-dfi",
        &["clean", "-dfi", "--dry-run", "--no-verify"],
        &["decompress,compress-fonts,compress-images,pretty"],
    ),
    clean_streams(
        "clean-dzgg",
        &["clean", "-dzgg", "--dry-run", "--no-verify"],
        &["decompress,compress,garbage=2"],
    ),
    clean_objstms(
        "clean-Z",
        &["clean", "-Z", "--dry-run", "--no-verify"],
        &["objstms"],
    ),
    clean_objstms(
        "clean-Zg",
        &["clean", "-Zg", "--dry-run", "--no-verify"],
        &["objstms,garbage=1"],
    ),
    clean_objstms(
        "clean-Zggz",
        &["clean", "-Zggz", "--dry-run", "--no-verify"],
        &["objstms,garbage=2,compress"],
    ),
    Check {
        about: "the document rewritten without encryption, as `mutool clean -D` writes it: BLAKE3 of the output",
        ..clean_gc(
            "clean-D",
            &["clean", "-D", "--dry-run", "--no-verify"],
            &["encrypt=none"],
        )
    },
    roundtrip_encrypted(
        "roundtrip-E40",
        &["clean", "-E", "rc4-40", "-O", "owner", "--no-verify"],
        &["encrypt=rc4-40,owner-password=owner"],
    ),
    roundtrip_encrypted(
        "roundtrip-E128",
        &["clean", "-E", "rc4-128", "-O", "owner", "--no-verify"],
        &["encrypt=rc4-128,owner-password=owner"],
    ),
    roundtrip_encrypted(
        "roundtrip-Eaes128",
        &["clean", "-E", "aes-128", "-O", "owner", "--no-verify"],
        &["encrypt=aes-128,owner-password=owner"],
    ),
    roundtrip_encrypted(
        "roundtrip-Eaes256",
        &["clean", "-E", "aes-256", "-O", "owner", "--no-verify"],
        &["encrypt=aes-256,owner-password=owner"],
    ),
    clean_pages(
        "clean-pages-reverse",
        &["clean", "--pages", "N-1", "--dry-run", "--no-verify"],
        &["pages=N-1"],
    ),
    clean_pages(
        "clean-pages-first",
        &["clean", "--pages", "1", "--dry-run", "--no-verify"],
        &["pages=1"],
    ),
    clean_pages(
        "clean-pages-rest",
        &["clean", "--pages", "2-N", "--dry-run", "--no-verify"],
        &["pages=2-N"],
    ),
    edit(
        "meta-set",
        &[
            "meta",
            "--set",
            "info:Title=nupdf",
            "--set",
            "Author=Été",
            "--dry-run",
            "--no-verify",
        ],
        &["meta=info:Title=nupdf,meta=Author=Été"],
    ),
    edit(
        "meta-strip",
        &["meta", "--strip", "--dry-run", "--no-verify"],
        &["strip-meta"],
    ),
    edit(
        "pages-rotate",
        &["pages", "--rotate", "90", "--dry-run", "--no-verify"],
        &["rotate=90"],
    ),
    edit(
        "pages-select-rotate",
        &[
            "pages",
            "--select",
            "N-1",
            "--rotate",
            "-90",
            "--rotate-pages",
            "1",
            "--dry-run",
            "--no-verify",
        ],
        &["pages=N-1,rotate=-90@1"],
    ),
    incremental(
        "meta-set-incremental",
        &[
            "meta",
            "--set",
            "info:Title=nupdf",
            "--set",
            "Author=Été",
            "--incremental",
            "--dry-run",
            "--no-verify",
        ],
        &["meta=info:Title=nupdf,meta=Author=Été,incremental"],
    ),
    incremental(
        "meta-strip-incremental",
        &[
            "meta",
            "--strip",
            "--incremental",
            "--dry-run",
            "--no-verify",
        ],
        &["strip-meta,incremental"],
    ),
    incremental(
        "pages-rotate-incremental",
        &[
            "pages",
            "--rotate",
            "90",
            "--incremental",
            "--dry-run",
            "--no-verify",
        ],
        &["rotate=90,incremental"],
    ),
    merge(
        "merge",
        &["merge", "--dry-run", "--no-verify", "{file}"],
        &[],
    ),
    merge(
        "merge-twice",
        &["merge", "--dry-run", "--no-verify", "{file}", "{file}"],
        &["twice"],
    ),
    merge(
        "merge-reverse",
        &["merge", "--dry-run", "--no-verify", "{file}", "N-1"],
        &["pages=N-1"],
    ),
    modelled(
        "arlington-clean",
        &["clean", "--no-verify"],
        "oracle/js/clean.js",
        &[],
    ),
    modelled(
        "arlington-merge",
        &["merge", "--no-verify", "{file}"],
        "oracle/js/merge.js",
        &[],
    ),
    modelled(
        "arlington-meta-incremental",
        &[
            "meta",
            "--set",
            "info:Title=nupdf",
            "--incremental",
            "--no-verify",
        ],
        "oracle/js/clean.js",
        &["meta=info:Title=nupdf,incremental"],
    ),
    validated(
        "qpdf-clean",
        &["clean", "--no-verify"],
        "oracle/js/clean.js",
        &[],
    ),
    validated(
        "qpdf-clean-Z",
        &["clean", "-Z", "--no-verify"],
        "oracle/js/clean.js",
        &["objstms"],
    ),
    validated(
        "qpdf-merge",
        &["merge", "--no-verify", "{file}"],
        "oracle/js/merge.js",
        &[],
    ),
    validated(
        "qpdf-meta-incremental",
        &[
            "meta",
            "--set",
            "info:Title=nupdf",
            "--incremental",
            "--no-verify",
        ],
        "oracle/js/clean.js",
        &["meta=info:Title=nupdf,incremental"],
    ),
    Check {
        name: "roundtrip-gggg",
        about: "the `clean -gggg` output of each side as MuPDF reads it back: objects, raw stream data, repair, warnings",
        oracle_script: "oracle/js/clean.js",
        nupdf_args: &["clean", "-gggg", "--no-verify"],
        compare: Compare::Collection {
            counts: &["/object_count", "/repaired", "/warnings"],
            items: "objects",
        },
        oracle_writes_files: true,
        oracle_file_fixup: None,
        nupdf_fixup: None,
        reread: Some(Reader::Mutool("oracle/js/objects.js")),
        oracle_args: &["garbage=4"],
    },
];

/// `clean` keeping some pages, compared as `clean` is.
const fn clean_pages(
    name: &'static str,
    nupdf_args: &'static [&'static str],
    oracle_args: &'static [&'static str],
) -> Check {
    Check {
        about: "the document with only some pages kept, in a given order, as `mutool clean` writes it given page ranges (pdf_rearrange_pages): BLAKE3 of the output",
        ..clean_gc(name, nupdf_args, oracle_args)
    }
}

/// An edit command (`meta`, `pages`), written as `clean` writes, against
/// the same edit made through MuPDF's API before saving.
const fn edit(
    name: &'static str,
    nupdf_args: &'static [&'static str],
    oracle_args: &'static [&'static str],
) -> Check {
    Check {
        about: "the document edited as `meta` or `pages` edit it (pdf_set_metadata, removing /Info and XMP, page /Rotate), then written as `mutool clean` writes it: BLAKE3 of the output",
        ..clean_gc(name, nupdf_args, oracle_args)
    }
}

/// A check's name as a file name that stays apart from the others on a
/// case-insensitive filesystem: `clean-Z` and `clean-z` would be one file,
/// so a capital becomes `_` and the letter.
fn file_stem(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 2);
    for c in name.chars() {
        if c.is_ascii_uppercase() {
            out.push('_');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// `qpdf --check` on each side's output: nupdf's output is as sound as
/// MuPDF's when qpdf says the same of both.
const fn validated(
    name: &'static str,
    nupdf_args: &'static [&'static str],
    oracle_script: &'static str,
    oracle_args: &'static [&'static str],
) -> Check {
    Check {
        name,
        about: "`qpdf --check` on each side's output, as `clean`, `clean -Z`, `merge` and an incremental edit write it: its verdict (0, or 2 for errors, 3 for warnings) and its messages",
        oracle_script,
        nupdf_args,
        compare: Compare::Fields(&["/qpdf/status", "/qpdf/messages"]),
        oracle_writes_files: true,
        oracle_file_fixup: None,
        nupdf_fixup: None,
        reread: Some(Reader::Qpdf),
        oracle_args,
    }
}

/// The Arlington rules nupdf applies, on each side's output: nupdf's
/// output breaks no rule MuPDF's keeps.
const fn modelled(
    name: &'static str,
    nupdf_args: &'static [&'static str],
    oracle_script: &'static str,
    oracle_args: &'static [&'static str],
) -> Check {
    Check {
        name,
        about: "selected Arlington PDF Model rules (required keys, value types, fixed names, direct and indirect values) on the trailer, catalog, page tree, pages, links, outline and document information of each side's output, as `clean`, `merge` and an incremental edit write it: the violations",
        oracle_script,
        nupdf_args,
        compare: Compare::Fields(&["/arlington/violations"]),
        oracle_writes_files: true,
        oracle_file_fixup: None,
        nupdf_fixup: None,
        reread: Some(Reader::Arlington),
        oracle_args,
    }
}

/// An edit appended as a new revision, against MuPDF's incremental save of
/// the same edit.
const fn incremental(
    name: &'static str,
    nupdf_args: &'static [&'static str],
    oracle_args: &'static [&'static str],
) -> Check {
    Check {
        about: "the edit appended to the file as MuPDF's incremental save appends it: the file's bytes, then the objects changed, an xref in the style of the last one read, and a trailer pointing back: BLAKE3 of the output",
        oracle_file_fixup: Some(name_nupdf_in_increment),
        nupdf_fixup: None,
        ..clean_gc(name, nupdf_args, oracle_args)
    }
}

/// `merge` into a new document, against `mutool merge` mirrored through
/// MuPDF's script interface.
const fn merge(
    name: &'static str,
    nupdf_args: &'static [&'static str],
    oracle_args: &'static [&'static str],
) -> Check {
    Check {
        about: "the pages merged into a new document as `mutool merge` merges them (pdf_create_document, a graft map per input, pdf_graft_mapped_page), with their external links and the outline items that lead to them: BLAKE3 of the output",
        oracle_script: "oracle/js/merge.js",
        ..clean_gc(name, nupdf_args, oracle_args)
    }
}

/// `clean -E` with an owner password only, so the output opens without
/// one, as MuPDF reads each side's output back. The bytes cannot be
/// compared: MuPDF draws a new /ID, and AES keys and IVs, at random.
const fn roundtrip_encrypted(
    name: &'static str,
    nupdf_args: &'static [&'static str],
    oracle_args: &'static [&'static str],
) -> Check {
    Check {
        name,
        about: "the document encrypted with a new security handler, as MuPDF reads each side's output back: objects decrypted, raw stream data, repair, warnings",
        oracle_script: "oracle/js/clean.js",
        nupdf_args,
        compare: Compare::Collection {
            counts: &["/object_count", "/repaired", "/warnings"],
            items: "objects",
        },
        oracle_writes_files: true,
        oracle_file_fixup: None,
        nupdf_fixup: None,
        reread: Some(Reader::Mutool("oracle/js/objects.js")),
        oracle_args,
    }
}

/// `clean` writing object streams and an xref stream, compared as `clean`
/// is.
const fn clean_objstms(
    name: &'static str,
    nupdf_args: &'static [&'static str],
    oracle_args: &'static [&'static str],
) -> Check {
    Check {
        about: "the document rewritten as `mutool clean -Z` writes it, objects packed into object streams with an xref stream, alone or with garbage collection and compression: BLAKE3 of the output",
        ..clean_gc(name, nupdf_args, oracle_args)
    }
}

/// `clean` with stream options, compared as `clean` is.
const fn clean_streams(
    name: &'static str,
    nupdf_args: &'static [&'static str],
    oracle_args: &'static [&'static str],
) -> Check {
    Check {
        about: "the document rewritten as `mutool clean` writes it with stream options (decompress, compress, effort, ascii): BLAKE3 of the output",
        ..clean_gc(name, nupdf_args, oracle_args)
    }
}

/// `clean` with garbage collection at one level, compared as `clean` is.
const fn clean_gc(
    name: &'static str,
    nupdf_args: &'static [&'static str],
    oracle_args: &'static [&'static str],
) -> Check {
    Check {
        name,
        about: "the document rewritten as `mutool clean` writes it at one garbage collection level (-g to -gggg): BLAKE3 of the output",
        oracle_script: "oracle/js/clean.js",
        nupdf_args,
        compare: Compare::Fields(&["/output/blake3"]),
        oracle_writes_files: true,
        oracle_file_fixup: Some(name_nupdf_as_writer),
        nupdf_fixup: None,
        reread: None,
        oracle_args,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Both succeeded and every compared field is equal.
    Match,
    /// Both succeeded but some field differs.
    Mismatch,
    /// The oracle failed and nupdf did not. Could be better or worse; look.
    OracleFailed,
    /// nupdf failed on a file the oracle handles.
    NupdfFailed,
    /// Both rejected the file.
    BothFailed,
    Timeout,
    /// A process died from a signal. For nupdf this is always a bug.
    Crash,
}

#[derive(Debug, Serialize)]
pub struct FieldDiff {
    pub field: String,
    pub oracle: Value,
    pub nupdf: Value,
}

#[derive(Debug, Serialize)]
pub struct FileResult {
    pub file: String,
    pub outcome: Outcome,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub diffs: Vec<FieldDiff>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// MuPDF's warnings, kept so later milestones can compare diagnostics.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub oracle_warnings: Vec<String>,
    pub oracle_ms: f64,
    pub nupdf_ms: f64,
}

#[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Baseline {
    pub oracle: String,
    pub checks: BTreeMap<String, BTreeMap<String, Outcome>>,
}

struct Side {
    status: Status,
    json: Option<Value>,
    stderr: String,
    ms: f64,
}

pub fn run(
    root: &Path,
    tier: &str,
    only: Option<&str>,
    update_baseline: bool,
    timeout: Duration,
) -> Result<()> {
    let mutool = oracle::mutool(root)?;
    let oracle_version = oracle::version(root)?;
    let nupdf = crate::build_nupdf(root)?;
    let files = corpus::files(root, tier)?;
    if files.is_empty() {
        bail!("tier {tier} has no files; see `cargo xtask corpus list`");
    }
    let checks: Vec<&Check> = CHECKS
        .iter()
        .filter(|c| only.is_none_or(|name| c.name == name))
        .collect();
    if checks.is_empty() {
        bail!("no check named {}", only.unwrap_or_default());
    }
    let qpdf = find_qpdf();
    if let Some(c) = checks
        .iter()
        .find(|c| matches!(c.reread, Some(Reader::Qpdf)))
        && qpdf.is_none()
    {
        bail!(
            "check {} needs qpdf: install it, or set NUPDF_QPDF to its path",
            c.name
        );
    }

    let out_dir = root.join("target/harness").join(tier);
    std::fs::create_dir_all(&out_dir)?;
    let mut current = Baseline {
        oracle: oracle_version.clone(),
        ..Baseline::default()
    };
    let mut summary = format!(
        "# Harness: tier `{tier}`, oracle MuPDF {oracle_version}\n\n\
         | Check | Files | Match | Mismatch | nupdf failed | Oracle failed | Both failed | Timeout | Crash | Parity |\n\
         |---|---|---|---|---|---|---|---|---|---|\n"
    );

    let mut notes = String::from("\n");
    for check in &checks {
        let _ = writeln!(notes, "- `{}`: {}", check.name, check.about);
        let results = run_check(
            root,
            check,
            &mutool,
            &nupdf,
            qpdf.as_deref(),
            &files,
            timeout,
        );
        std::fs::write(
            out_dir.join(format!("{}.json", file_stem(check.name))),
            serde_json::to_string_pretty(&results)?,
        )?;
        summary.push_str(&summary_row(check.name, &results));
        current.checks.insert(
            check.name.to_string(),
            results
                .iter()
                .map(|r| (r.file.clone(), r.outcome))
                .collect(),
        );
        for r in results.iter().filter(|r| r.outcome != Outcome::Match) {
            let _ = writeln!(
                notes,
                "- `{}` {}: {:?}{}",
                check.name,
                r.file,
                r.outcome,
                describe(r)
            );
        }
    }

    summary.push_str(&notes);
    std::fs::write(out_dir.join("summary.md"), &summary)?;
    println!("{summary}");
    println!("full results: {}", out_dir.display());

    let baseline_path = root.join("harness/baselines").join(format!("{tier}.json"));
    if update_baseline {
        let mut baseline = load_baseline(&baseline_path)?.unwrap_or_default();
        baseline.oracle = current.oracle.clone();
        // Only the checks that ran are replaced.
        baseline.checks.extend(current.checks);
        std::fs::create_dir_all(baseline_path.parent().unwrap_or(root))?;
        std::fs::write(
            &baseline_path,
            serde_json::to_string_pretty(&baseline)? + "\n",
        )?;
        println!("baseline updated: {}", baseline_path.display());
        return Ok(());
    }
    match load_baseline(&baseline_path)? {
        None => println!("no baseline for tier {tier}; create one with --update-baseline"),
        Some(baseline) => ratchet(&baseline, &current)?,
    }
    Ok(())
}

fn run_check(
    root: &Path,
    check: &Check,
    mutool: &Path,
    nupdf: &Path,
    qpdf: Option<&Path>,
    files: &[corpus::CorpusFile],
    timeout: Duration,
) -> Vec<FileResult> {
    let script = root.join(check.oracle_script);
    let model = crate::arlington::Model::load();
    let scratch = root
        .join("target/harness/scratch")
        .join(file_stem(check.name));
    let next = AtomicUsize::new(0);
    let results = Mutex::new(Vec::with_capacity(files.len()));
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get());
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(file) = files.get(index) else {
                        break;
                    };
                    let dir = scratch.join(index.to_string());
                    let (oracle, ours) = match check.reread {
                        None => run_sides(check, &script, mutool, nupdf, &file.path, &dir, timeout),
                        Some(reader) => {
                            let reread = match reader {
                                Reader::Mutool(script) => root.join(script),
                                Reader::Qpdf | Reader::Arlington => PathBuf::new(),
                            };
                            let run = Reread {
                                mutool,
                                script: &reread,
                                qpdf: matches!(reader, Reader::Qpdf).then_some(qpdf).flatten(),
                                arlington: matches!(reader, Reader::Arlington).then_some(&model),
                                timeout,
                            };
                            reread_sides(&run, &script, nupdf, check, &file.path, &dir)
                        }
                    };
                    let _ = std::fs::remove_dir_all(&dir);
                    let result = judge(check, &file.name, oracle, ours);
                    if let Ok(mut results) = results.lock() {
                        results.push(result);
                    }
                }
            });
        }
    });
    let mut results = results.into_inner().unwrap_or_default();
    results.sort_by(|a, b| a.file.cmp(&b.file));
    results
}

/// The oracle script and `nupdf` on the file itself.
fn run_sides(
    check: &Check,
    script: &Path,
    mutool: &Path,
    nupdf: &Path,
    file: &Path,
    dir: &Path,
    timeout: Duration,
) -> (Side, Side) {
    let mut oracle_args = vec![
        std::ffi::OsStr::new("run"),
        script.as_os_str(),
        file.as_os_str(),
    ];
    if check.oracle_writes_files {
        let _ = std::fs::create_dir_all(dir);
        oracle_args.push(dir.as_os_str());
    }
    oracle_args.extend(check.oracle_args.iter().map(std::ffi::OsStr::new));
    let mut oracle = side(mutool, &oracle_args, timeout);
    if check.oracle_writes_files
        && let Some(json) = &mut oracle.json
    {
        hash_written_files(json, dir, check.oracle_file_fixup);
    }
    let ours_dir = dir.join("nupdf");
    if check.nupdf_args.contains(&"{dir}") {
        let _ = std::fs::create_dir_all(&ours_dir);
    }
    let args = nupdf_argv(check, file, &ours_dir);
    let mut ours = side(nupdf, &args, timeout);
    if let Some(json) = &mut ours.json {
        if check.nupdf_args.contains(&"{dir}") {
            hash_written_files(json, &ours_dir, None);
        }
        if let Some(f) = check.nupdf_fixup {
            f(json);
        }
    }
    (oracle, ours)
}

/// The check's `nupdf` arguments with the file in them.
fn nupdf_argv<'a>(check: &'a Check, file: &'a Path, dir: &'a Path) -> Vec<&'a std::ffi::OsStr> {
    let mut args: Vec<&std::ffi::OsStr> = check
        .nupdf_args
        .iter()
        .map(|a| match *a {
            "{file}" => file.as_os_str(),
            "{dir}" => dir.as_os_str(),
            a => std::ffi::OsStr::new(a),
        })
        .collect();
    if !check.nupdf_args.contains(&"{file}") {
        args.push(file.as_os_str());
    }
    args
}

struct Reread<'a> {
    mutool: &'a Path,
    script: &'a Path,
    /// `qpdf --check` reads instead of MuPDF.
    qpdf: Option<&'a Path>,
    /// Or the Arlington rules do.
    arlington: Option<&'a crate::arlington::Model>,
    timeout: Duration,
}

impl Reread<'_> {
    /// MuPDF reading `written`, once `writer` succeeded. A failed write or
    /// read is that side's failure.
    fn after(&self, writer: Side, written: &Path, dir: &Path) -> Side {
        if writer.json.is_none() {
            return writer;
        }
        if let Some(qpdf) = self.qpdf {
            let mut read = qpdf_check(qpdf, written, self.timeout);
            read.ms += writer.ms;
            return read;
        }
        if let Some(model) = self.arlington {
            let start = std::time::Instant::now();
            let json = std::fs::read(written)
                .ok()
                .and_then(|bytes| nupdf::Document::open(bytes).ok())
                .map(|doc| {
                    serde_json::json!({ "arlington": { "violations": model.validate(&doc) } })
                });
            return Side {
                status: Status::Exited(i32::from(json.is_none())),
                json,
                stderr: String::new(),
                ms: writer.ms + start.elapsed().as_secs_f64() * 1000.0,
            };
        }
        let _ = std::fs::create_dir_all(dir);
        let args = [
            std::ffi::OsStr::new("run"),
            self.script.as_os_str(),
            written.as_os_str(),
            dir.as_os_str(),
        ];
        let mut read = side(self.mutool, &args, self.timeout);
        if let Some(Value::Object(map)) = &mut read.json {
            let warnings: std::collections::BTreeSet<&str> = read
                .stderr
                .lines()
                .filter_map(|l| l.strip_prefix("warning: "))
                .collect();
            map.insert("warnings".into(), Value::from(Vec::from_iter(warnings)));
        }
        if let Some(json) = &mut read.json {
            hash_written_files(json, dir, None);
        }
        read.ms += writer.ms;
        read
    }
}

/// Each side writes the document, then MuPDF reads each output back.
fn reread_sides(
    run: &Reread,
    script: &Path,
    nupdf: &Path,
    check: &Check,
    file: &Path,
    dir: &Path,
) -> (Side, Side) {
    let oracle_dir = dir.join("oracle");
    let _ = std::fs::create_dir_all(&oracle_dir);
    let mut oracle_args = vec![
        std::ffi::OsStr::new("run"),
        script.as_os_str(),
        file.as_os_str(),
        oracle_dir.as_os_str(),
    ];
    oracle_args.extend(check.oracle_args.iter().map(std::ffi::OsStr::new));
    let oracle = side(run.mutool, &oracle_args, run.timeout);
    let name = oracle
        .json
        .as_ref()
        .and_then(|j| j.pointer("/output/file"))
        .and_then(Value::as_str)
        .unwrap_or("clean.pdf")
        .to_string();
    let oracle = run.after(oracle, &oracle_dir.join(name), &dir.join("oracle-read"));

    let written = dir.join("nupdf.pdf");
    let mut args = nupdf_argv(check, file, dir);
    args.extend(["-o".as_ref(), written.as_os_str()]);
    let ours = side(nupdf, &args, run.timeout);
    let ours = run.after(ours, &written, &dir.join("nupdf-read"));
    (oracle, ours)
}

/// `qpdf --check` on `written`. Its verdicts, 0 and the 2 and 3 for
/// errors and warnings, are all a reading; only a crash or a timeout is a
/// failure.
fn qpdf_check(qpdf: &Path, written: &Path, timeout: Duration) -> Side {
    let args = [std::ffi::OsStr::new("--check"), written.as_os_str()];
    match proc::run(qpdf, &args, timeout) {
        Ok(run) => {
            let path = written.to_string_lossy();
            let messages: Vec<Value> = String::from_utf8_lossy(&run.stdout)
                .lines()
                .chain(String::from_utf8_lossy(&run.stderr).lines())
                .map(|l| Value::from(l.replace(path.as_ref(), "FILE")))
                .collect();
            let json = match run.status {
                Status::Exited(code) => Some(serde_json::json!({
                    "qpdf": { "status": code, "messages": messages }
                })),
                _ => None,
            };
            Side {
                status: run.status,
                json,
                stderr: String::from_utf8_lossy(&run.stderr).into_owned(),
                ms: run.wall.as_secs_f64() * 1000.0,
            }
        }
        Err(e) => Side {
            status: Status::Exited(-1),
            json: None,
            stderr: e.to_string(),
            ms: 0.0,
        },
    }
}

/// `qpdf` from `$NUPDF_QPDF`, else the first on `PATH`.
fn find_qpdf() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("NUPDF_QPDF") {
        return Some(PathBuf::from(p));
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join("qpdf"))
        .find(|p| p.is_file())
}

fn side(program: &Path, args: &[&std::ffi::OsStr], timeout: Duration) -> Side {
    match proc::run(program, args, timeout) {
        Ok(run) => Side {
            status: run.status,
            // MuPDF's text conversion can emit bytes that are not UTF-8
            // (an overlong NUL, lone surrogates); read both sides lossily.
            json: run
                .succeeded()
                .then(|| serde_json::from_str(&String::from_utf8_lossy(&run.stdout)).ok())
                .flatten(),
            stderr: String::from_utf8_lossy(&run.stderr).into_owned(),
            ms: run.wall.as_secs_f64() * 1000.0,
        },
        Err(e) => Side {
            status: Status::Exited(-1),
            json: None,
            stderr: format!("could not run: {e:#}"),
            ms: 0.0,
        },
    }
}

fn judge(check: &Check, file: &str, oracle: Side, ours: Side) -> FileResult {
    let oracle_warnings = oracle
        .stderr
        .lines()
        .filter_map(|l| l.strip_prefix("warning: "))
        .map(str::to_string)
        .collect();
    let mut result = FileResult {
        file: file.to_string(),
        outcome: Outcome::Match,
        diffs: Vec::new(),
        detail: None,
        oracle_warnings,
        oracle_ms: oracle.ms,
        nupdf_ms: ours.ms,
    };
    let failure = |side: &Side, who: &str| -> Option<String> {
        if side.json.is_some() {
            return None;
        }
        let last = side.stderr.lines().last().unwrap_or("").trim().to_string();
        Some(format!("{who}: {:?} {last}", side.status))
    };

    let crashed = |s: &Side| matches!(s.status, Status::Signaled(_));
    let timed_out = |s: &Side| s.status == Status::TimedOut;
    result.outcome = if crashed(&oracle) || crashed(&ours) {
        Outcome::Crash
    } else if timed_out(&oracle) || timed_out(&ours) {
        Outcome::Timeout
    } else {
        match (&oracle.json, &ours.json) {
            (None, None) => Outcome::BothFailed,
            (None, Some(_)) => Outcome::OracleFailed,
            (Some(_), None) => Outcome::NupdfFailed,
            (Some(a), Some(b)) => {
                result.diffs = match check.compare {
                    Compare::Fields(fields) => compare(fields, a, b),
                    Compare::Collection { counts, items } => {
                        compare_collection(counts, items, a, b)
                    }
                    Compare::Warnings => compare_warnings(&oracle.stderr, b),
                    Compare::Trace => compare_trace(a, b),
                };
                if result.diffs.is_empty() {
                    Outcome::Match
                } else {
                    Outcome::Mismatch
                }
            }
        }
    };
    let details: Vec<String> = [failure(&oracle, "oracle"), failure(&ours, "nupdf")]
        .into_iter()
        .flatten()
        .collect();
    if !details.is_empty() {
        result.detail = Some(details.join("; "));
    }
    result
}

/// Replaces every `"file": name` with `"blake3": <hash of dir/name>`, or
/// with `"blake3": null` when the file is missing.
pub fn hash_written_files(value: &mut Value, dir: &Path, fixup: Option<fn(&mut [u8])>) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(name)) = map.remove("file") {
                let hash = std::fs::read(dir.join(&name)).ok().map(|mut data| {
                    if let Some(f) = fixup {
                        f(&mut data);
                    }
                    blake3::hash(&data).to_hex().to_string()
                });
                map.insert("blake3".into(), hash.map_or(Value::Null, Value::from));
            }
            map.values_mut()
                .for_each(|v| hash_written_files(v, dir, fixup));
        }
        Value::Array(items) => items
            .iter_mut()
            .for_each(|v| hash_written_files(v, dir, fixup)),
        _ => {}
    }
}

/// The `fonts` report's `load` facts: what nupdf loaded each font as,
/// which MuPDF's scripts cannot see.
fn drop_font_loads(json: &mut Value) {
    if let Some(Value::Array(fonts)) = json.get_mut("fonts") {
        for f in fonts {
            if let Value::Object(f) = f {
                f.remove("load");
            }
        }
    }
}

/// MuPDF's reproducible output names MuPDF in its comment line; nupdf's
/// names nupdf, in a line of the same length, so offsets agree.
/// `name_nupdf_as_writer` for an incremental save, which names its writer
/// at the start of the increment: the last such line, after the file.
fn name_nupdf_in_increment(data: &mut [u8]) {
    const MUPDF: &[u8] = b"% Written by MuPDF\n";
    const NUPDF: &[u8] = b"% Written by nupdf\n";
    if let Some(at) = data.windows(MUPDF.len()).rposition(|w| w == MUPDF) {
        data[at..at + MUPDF.len()].copy_from_slice(NUPDF);
    }
}

fn name_nupdf_as_writer(data: &mut [u8]) {
    const MUPDF: &[u8] = b"% Written by MuPDF\n";
    const NUPDF: &[u8] = b"% Written by nupdf\n";
    if let Some(at) = data.windows(MUPDF.len()).take(64).position(|w| w == MUPDF) {
        data[at..at + MUPDF.len()].copy_from_slice(NUPDF);
    }
}

pub fn compare(fields: &[&str], oracle: &Value, ours: &Value) -> Vec<FieldDiff> {
    fields
        .iter()
        .filter_map(|&field| {
            let a = oracle.pointer(field).cloned().unwrap_or(Value::Null);
            let b = ours.pointer(field).cloned().unwrap_or(Value::Null);
            (!json_eq(&a, &b)).then(|| FieldDiff {
                field: field.to_string(),
                oracle: a,
                nupdf: b,
            })
        })
        .collect()
}

/// MuPDF's stderr lines that are diagnostics: `warning: X` and caught
/// errors reported as `<kind> error: X`. Its "... repeated N times" lines
/// are a printing artifact, not diagnostics.
pub fn mupdf_diagnostics(stderr: &str) -> std::collections::BTreeSet<String> {
    const KINDS: [&str; 8] = [
        "format",
        "syntax",
        "generic",
        "library",
        "limit",
        "argument",
        "system",
        "unsupported",
    ];
    stderr
        .lines()
        .filter_map(|line| {
            if let Some(w) = line.strip_prefix("warning: ") {
                return (!w.starts_with("... repeated")).then(|| w.to_string());
            }
            KINDS
                .iter()
                .any(|k| line.starts_with(&format!("{k} error: ")))
                .then(|| line.to_string())
        })
        .collect()
}

pub fn compare_warnings(oracle_stderr: &str, ours: &Value) -> Vec<FieldDiff> {
    let theirs = mupdf_diagnostics(oracle_stderr);
    let mine: std::collections::BTreeSet<String> = ours
        .get("diagnostics")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|d| d.get("message").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let only = |a: &std::collections::BTreeSet<String>, b: &std::collections::BTreeSet<String>| {
        Value::from(a.difference(b).cloned().collect::<Vec<_>>())
    };
    if theirs == mine {
        Vec::new()
    } else {
        vec![FieldDiff {
            field: "/diagnostics".into(),
            oracle: only(&theirs, &mine),
            nupdf: only(&mine, &theirs),
        }]
    }
}

/// Item-by-item comparison of a collection; reports the first few
/// differences.
pub fn compare_collection(
    counts: &[&str],
    items: &str,
    oracle: &Value,
    ours: &Value,
) -> Vec<FieldDiff> {
    const MAX_REPORTED: usize = 5;
    let mut diffs = compare(counts, oracle, ours);
    let as_map = |v: &Value| -> serde_json::Map<String, Value> {
        match v.get(items) {
            Some(Value::Object(m)) => m.clone(),
            Some(Value::Array(list)) => list
                .iter()
                .enumerate()
                .map(|(i, v)| (i.to_string(), v.clone()))
                .collect(),
            _ => serde_json::Map::new(),
        }
    };
    let (a, b) = (as_map(oracle), as_map(ours));
    let mut keys: Vec<&String> = a.keys().chain(b.keys()).collect();
    keys.sort_by_key(|k| k.parse::<u64>().unwrap_or(u64::MAX));
    keys.dedup();
    let mut differing = 0;
    for key in keys {
        let x = a.get(key).unwrap_or(&Value::Null);
        let y = b.get(key).unwrap_or(&Value::Null);
        if !json_eq(x, y) {
            differing += 1;
            if diffs.len() < MAX_REPORTED {
                diffs.push(FieldDiff {
                    field: format!("/{items}/{key}"),
                    oracle: truncate(x),
                    nupdf: truncate(y),
                });
            }
        }
    }
    if differing > MAX_REPORTED {
        diffs.push(FieldDiff {
            field: format!("/{items} (more)"),
            oracle: Value::Null,
            nupdf: Value::from(format!("{differing} {items} differ in total")),
        });
    }
    diffs
}

/// `Compare::Trace`: the page count, then each page's error and calls; a
/// page that differs is reported at its first differing call.
pub fn compare_trace(oracle: &Value, ours: &Value) -> Vec<FieldDiff> {
    const MAX_REPORTED: usize = 5;
    let mut diffs = compare(&["/page_count"], oracle, ours);
    let pages = |v: &Value| {
        v.get("pages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let (a, b) = (pages(oracle), pages(ours));
    let mut differing = 0;
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).unwrap_or(&Value::Null);
        let y = b.get(i).unwrap_or(&Value::Null);
        let events = |v: &Value| {
            v.get("events")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        };
        let (ex, ey) = (events(x), events(y));
        let error = |v: &Value| v.get("error").cloned().unwrap_or(Value::Null);
        let diff = if !json_eq(&error(x), &error(y)) {
            Some(FieldDiff {
                field: format!("/pages/{i}/error"),
                oracle: error(x),
                nupdf: error(y),
            })
        } else {
            (0..ex.len().max(ey.len()))
                .find(|&k| match (ex.get(k), ey.get(k)) {
                    (Some(p), Some(q)) => !json_close(p, q),
                    _ => true,
                })
                .map(|k| FieldDiff {
                    field: format!("/pages/{i}/events/{k} (of {} and {})", ex.len(), ey.len()),
                    oracle: truncate(ex.get(k).unwrap_or(&Value::Null)),
                    nupdf: truncate(ey.get(k).unwrap_or(&Value::Null)),
                })
        };
        if let Some(d) = diff {
            differing += 1;
            if diffs.len() < MAX_REPORTED {
                diffs.push(d);
            }
        }
    }
    if differing > MAX_REPORTED {
        diffs.push(FieldDiff {
            field: "/pages (more)".into(),
            oracle: Value::Null,
            nupdf: Value::from(format!("{differing} pages differ in total")),
        });
    }
    diffs
}

/// `json_eq`, with numbers equal within 1e-4 of the larger (or of 1).
pub fn json_close(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            let (Some(p), Some(q)) = (x.as_f64(), y.as_f64()) else {
                return false;
            };
            p == q || (p - q).abs() <= 1e-4 * p.abs().max(q.abs()).max(1.0)
        }
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| json_close(p, q))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|w| json_close(v, w)))
        }
        _ => a == b,
    }
}

fn truncate(v: &Value) -> Value {
    let text = v.to_string();
    if text.len() <= 400 {
        return v.clone();
    }
    let mut cut = 400;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    Value::from(format!("{}...", &text[..cut]))
}

/// JSON equality where numbers compare by value, so 1 and 1.0 are equal.
/// Non-integers compare at f32 precision: MuPDF stores reals as floats and
/// mujs prints only 16 significant digits of them.
pub fn json_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            let (Some(p), Some(q)) = (x.as_f64(), y.as_f64()) else {
                return false;
            };
            // Integers beyond 2^53 come from JS with only 16 digits.
            let big = p.abs() > 9.007_199_254_740_992e15;
            p == q
                || ((x.is_f64() || y.is_f64()) && p as f32 == q as f32)
                || (big && (p - q).abs() <= p.abs() * 1e-15)
        }
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| json_eq(p, q))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|w| json_eq(v, w)))
        }
        _ => a == b,
    }
}

fn summary_row(name: &str, results: &[FileResult]) -> String {
    let count = |o: Outcome| results.iter().filter(|r| r.outcome == o).count();
    let matched = count(Outcome::Match);
    // Parity counts the files the oracle handled.
    let oracle_ok = matched + count(Outcome::Mismatch) + count(Outcome::NupdfFailed);
    let parity = if oracle_ok == 0 {
        "n/a".to_string()
    } else {
        format!("{:.1}%", 100.0 * matched as f64 / oracle_ok as f64)
    };
    format!(
        "| {name} | {} | {matched} | {} | {} | {} | {} | {} | {} | {parity} |\n",
        results.len(),
        count(Outcome::Mismatch),
        count(Outcome::NupdfFailed),
        count(Outcome::OracleFailed),
        count(Outcome::BothFailed),
        count(Outcome::Timeout),
        count(Outcome::Crash),
    )
}

fn describe(r: &FileResult) -> String {
    let mut parts: Vec<String> = r
        .diffs
        .iter()
        .map(|d| format!("{} oracle={} nupdf={}", d.field, d.oracle, d.nupdf))
        .collect();
    parts.extend(r.detail.clone());
    if parts.is_empty() {
        String::new()
    } else {
        format!(" ({})", parts.join("; "))
    }
}

fn load_baseline(path: &PathBuf) -> Result<Option<Baseline>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Fails when a file that matched in the baseline no longer does.
pub fn ratchet(baseline: &Baseline, current: &Baseline) -> Result<()> {
    if baseline.oracle != current.oracle {
        bail!(
            "baseline was recorded with MuPDF {}, oracle is {}; re-baseline with --update-baseline",
            baseline.oracle,
            current.oracle
        );
    }
    let mut regressions = Vec::new();
    let mut improvements = 0;
    for (check, now) in &current.checks {
        let Some(before) = baseline.checks.get(check) else {
            continue;
        };
        for (file, outcome) in now {
            match before.get(file) {
                Some(Outcome::Match) if *outcome != Outcome::Match => {
                    regressions.push(format!("{check} {file}: match -> {outcome:?}"));
                }
                Some(previous) if *previous != Outcome::Match && *outcome == Outcome::Match => {
                    improvements += 1;
                }
                _ => {}
            }
        }
    }
    if improvements > 0 {
        println!("{improvements} file(s) newly match; run with --update-baseline to lock them in");
    }
    if !regressions.is_empty() {
        bail!("parity regressed:\n  {}", regressions.join("\n  "));
    }
    println!("no regressions against baseline");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn traces_compare_text_with_float_tolerance() {
        let trace = |gid: i64, x: f64| {
            json!({"page_count": 1, "pages": [{"events": [
                ["set_default_colorspaces"],
                ["fill_text", [["Helvetica", [12.0, 0.0, 0.0, 12.0], 0, 0, 2, "", [[gid, 65, x, 10.0]]]]]
            ]}]})
        };
        assert!(compare_trace(&trace(36, 10.0), &trace(36, 10.000_000_1)).is_empty());
        let diffs = compare_trace(&trace(36, 10.0), &trace(37, 10.0));
        assert_eq!(diffs.len(), 1);
        assert!(
            diffs[0].field.starts_with("/pages/0/events/1"),
            "{}",
            diffs[0].field
        );
        assert!(json_close(
            &json!([1.0, 612.0]),
            &json!([1.000_000_1, 612.000_01])
        ));
        assert!(!json_close(&json!(1.0), &json!(1.01)));
    }

    fn side(status: Status, json: Option<Value>) -> Side {
        Side {
            status,
            json,
            stderr: String::new(),
            ms: 1.0,
        }
    }

    #[test]
    fn compares_only_declared_fields() {
        let diffs = compare(
            &["/pdf_version"],
            &json!({"pdf_version": "1.7", "page_count": 3}),
            &json!({"pdf_version": "1.7", "doc_id": "b3:00"}),
        );
        assert!(diffs.is_empty());
        let diffs = compare(
            &["/pdf_version"],
            &json!({"pdf_version": "1.7"}),
            &json!({}),
        );
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].nupdf, Value::Null);
    }

    #[test]
    fn written_files_become_hashes() {
        let dir = std::env::temp_dir().join(format!("nupdf-harness-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("4"), b"abc").unwrap();
        let mut v =
            serde_json::json!({"streams": {"4": {"len": 3, "file": "4"}, "5": {"file": "5"}}});
        hash_written_files(&mut v, &dir, None);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"streams": {
                "4": {"len": 3, "blake3": blake3::hash(b"abc").to_hex().to_string()},
                "5": {"blake3": null},
            }})
        );
    }

    #[test]
    fn mupdf_stderr_becomes_a_diagnostic_set() {
        let set = mupdf_diagnostics(
            "format error: cannot find startxref\nwarning: trying to repair broken xref\n\
             warning: ... repeated 3 times...\nwarning: trying to repair broken xref\nnoise\n",
        );
        assert_eq!(
            set.into_iter().collect::<Vec<_>>(),
            vec![
                "format error: cannot find startxref",
                "trying to repair broken xref"
            ]
        );
    }

    #[test]
    fn numbers_compare_by_value() {
        assert!(json_eq(&json!({"r": 1}), &json!({"r": 1.0})));
        assert!(json_eq(
            &json!(1.200000047683716),
            &json!(1.2000000476837158)
        ));
        assert!(!json_eq(&json!(16777217), &json!(16777216)));
        assert!(json_eq(
            &json!(-8546744073709553000i64),
            &json!(-8546744073709551616i64)
        ));
        assert!(!json_eq(&json!([1, 2]), &json!([1, 3])));
    }

    #[test]
    fn object_diffs_name_the_objects() {
        let a = json!({"object_count": 3, "objects": {"1": 5, "2": {"n": "A"}}});
        let b = json!({"object_count": 3, "objects": {"1": 5.0, "2": {"n": "B"}}});
        let diffs = compare_collection(&["/object_count"], "objects", &a, &b);
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].field, "/objects/2");
    }

    #[test]
    fn classifies_outcomes() {
        let check = &CHECKS[0];
        let ok = |v: &str| side(Status::Exited(0), Some(json!({"pdf_version": v})));
        let failed = || side(Status::Exited(3), None);
        let judge = |a, b| super::judge(check, "f.pdf", a, b).outcome;

        assert_eq!(judge(ok("1.7"), ok("1.7")), Outcome::Match);
        assert_eq!(judge(ok("1.7"), ok("1.4")), Outcome::Mismatch);
        assert_eq!(judge(ok("1.7"), failed()), Outcome::NupdfFailed);
        assert_eq!(judge(failed(), ok("1.7")), Outcome::OracleFailed);
        assert_eq!(judge(failed(), failed()), Outcome::BothFailed);
        assert_eq!(
            judge(ok("1.7"), side(Status::Signaled(11), None)),
            Outcome::Crash
        );
        assert_eq!(
            judge(side(Status::TimedOut, None), ok("1.7")),
            Outcome::Timeout
        );
    }

    #[test]
    fn collects_oracle_warnings() {
        let mut oracle = side(Status::Exited(0), Some(json!({"pdf_version": "1.4"})));
        oracle.stderr =
            "format error: cannot find startxref\nwarning: trying to repair broken xref\n".into();
        let result = super::judge(
            &CHECKS[0],
            "f.pdf",
            oracle,
            side(Status::Exited(0), Some(json!({"pdf_version": "1.4"}))),
        );
        assert_eq!(result.oracle_warnings, vec!["trying to repair broken xref"]);
    }

    fn baseline(entries: &[(&str, Outcome)]) -> Baseline {
        let files = entries.iter().map(|(f, o)| (f.to_string(), *o)).collect();
        Baseline {
            oracle: "1.28.6".into(),
            checks: BTreeMap::from([("info".to_string(), files)]),
        }
    }

    #[test]
    fn result_files_stay_apart_without_case() {
        let stems: std::collections::HashSet<String> = CHECKS
            .iter()
            .map(|c| file_stem(c.name).to_ascii_lowercase())
            .collect();
        assert_eq!(stems.len(), CHECKS.len());
        assert_eq!(file_stem("clean-Zggz"), "clean-_zggz");
    }

    #[test]
    fn ratchet_rejects_lost_matches_and_accepts_gains() {
        let before = baseline(&[("a.pdf", Outcome::Match), ("b.pdf", Outcome::Mismatch)]);
        let gained = baseline(&[("a.pdf", Outcome::Match), ("b.pdf", Outcome::Match)]);
        assert!(ratchet(&before, &gained).is_ok());
        let lost = baseline(&[
            ("a.pdf", Outcome::NupdfFailed),
            ("b.pdf", Outcome::Mismatch),
        ]);
        assert!(ratchet(&before, &lost).is_err());
    }

    #[test]
    fn ratchet_refuses_a_different_oracle() {
        let before = baseline(&[("a.pdf", Outcome::Match)]);
        let mut after = baseline(&[("a.pdf", Outcome::Match)]);
        after.oracle = "1.29.0".into();
        assert!(ratchet(&before, &after).is_err());
    }
}
