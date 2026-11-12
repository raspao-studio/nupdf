// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! `nupdf`: JSON reports on stdout, JSON errors on stderr, meaningful exit
//! codes. The contract is specified in docs/AGENT-INTERFACE.md.

mod annot_edits;
mod batch;
mod commands;
mod diff;
mod extract;
mod guide;
mod inventory;
mod mcp;
mod md5;
mod navigation;
mod objects;
mod ocr;
mod redact;
mod render;
mod schemas;
mod search;
mod stext;
mod tables;
mod trace;

use std::io::Write;
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use nupdf::header::{self, Kind};
use nupdf::{DocId, Document};
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(
    name = "nupdf",
    version,
    about = "Memory-safe PDF toolkit built for agents"
)]
struct Cli {
    /// Pretty-print JSON output.
    #[arg(long, global = true)]
    pretty: bool,
    /// Largest decoded size of a single stream, in bytes.
    #[arg(long, global = true, default_value_t = nupdf::Limits::default().max_decoded_bytes)]
    max_decoded_bytes: usize,
    /// Largest cross-reference table a document may have, in object slots.
    #[arg(long, global = true, default_value_t = nupdf::Limits::default().max_xref_entries)]
    max_xref_entries: usize,
    /// Password for an encrypted document (user or owner).
    #[arg(long, global = true)]
    password: Option<String>,
    /// Stop after this many seconds, with exit 5 and `limit.timeout`.
    #[arg(long, global = true)]
    timeout: Option<f64>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Identify a file and report what nupdf can tell about it.
    Info { file: PathBuf },
    /// Dump every object in the cross-reference table as canonical JSON.
    Objects { file: PathBuf },
    /// List pages with their inherited MediaBox, CropBox, Rotate and
    /// Resources; or, with --select, --delete or --rotate, write the document
    /// with its pages changed.
    Pages {
        file: PathBuf,
        /// Keep only these pages, in this order, as `clean --pages`
        /// (`1-3,5`, `N` for the last, `5-1` backwards).
        #[arg(long, conflicts_with = "delete")]
        select: Option<String>,
        /// Drop these pages.
        #[arg(long)]
        delete: Option<String>,
        /// Turn pages clockwise by this many degrees, a multiple of 90.
        #[arg(long, allow_negative_numbers = true)]
        rotate: Option<i32>,
        /// The pages --rotate turns, after --select or --delete; all by
        /// default.
        #[arg(long, requires = "rotate")]
        rotate_pages: Option<String>,
        #[command(flatten)]
        write: commands::WriteArgs,
    },
    /// Show the document information dictionary; or, with --set or
    /// --strip, write the document with it changed.
    Meta {
        file: PathBuf,
        /// Set an entry, as `KEY=VALUE` (repeatable), as MuPDF's
        /// `pdf_set_metadata` does.
        #[arg(long = "set", value_name = "KEY=VALUE")]
        set: Vec<String>,
        /// Remove /Info and the catalog's XMP /Metadata, before any --set.
        #[arg(long)]
        strip: bool,
        #[command(flatten)]
        write: commands::WriteArgs,
    },
    /// Remove what the document can do besides show itself: JavaScript,
    /// Launch, SubmitForm and ImportData, GoToR/GoToE actions and XFA,
    /// and by choice web links and embedded files. The output is scanned
    /// again before it is written (exit 4 if anything is left).
    Sanitize {
        file: PathBuf,
        /// Keep these (repeatable or comma-separated): javascript, launch,
        /// data (submit and import), remote, xfa.
        #[arg(long, value_delimiter = ',')]
        keep: Vec<String>,
        /// Web links: keep, remove, or allow:HOST,... (those hosts and
        /// their subdomains kept, the rest removed).
        #[arg(long, default_value = "keep")]
        uri: String,
        /// Remove embedded files too.
        #[arg(long)]
        attachments: bool,
        #[command(flatten)]
        write: commands::WriteArgs,
    },
    /// Draw annotations and form fields into their pages and remove them,
    /// as MuPDF's bake does (links stay; with fields, the form goes). An
    /// annotation without an appearance gets the one MuPDF makes.
    Flatten {
        file: PathBuf,
        /// Annotations only: leave the form fields.
        #[arg(long, conflicts_with = "widgets_only")]
        annots_only: bool,
        /// Form fields only: leave the annotations.
        #[arg(long)]
        widgets_only: bool,
        #[command(flatten)]
        write: commands::WriteArgs,
    },
    /// Make scanned pages searchable: each page is drawn, Tesseract reads
    /// it, and the words go on the page as invisible text. Only the pages
    /// that look scanned (no text, an image over most of the page) unless
    /// --all or --pages.
    Ocr {
        file: PathBuf,
        /// These pages, scanned or not (e.g. 1-3,7).
        #[arg(long)]
        pages: Option<String>,
        /// Every page, scanned or not.
        #[arg(long)]
        all: bool,
        /// Tesseract's language codes, joined by + (eng, deu, eng+fra).
        #[arg(long, default_value = "eng")]
        lang: String,
        /// The resolution pages are drawn at for Tesseract.
        #[arg(long, default_value_t = 300.0)]
        dpi: f32,
        /// The Tesseract program.
        #[arg(long, default_value = "tesseract")]
        tesseract: PathBuf,
        /// Leave out words Tesseract is less sure of (0 to 100).
        #[arg(long, default_value_t = 0.0)]
        min_confidence: f32,
        /// Seconds Tesseract may take on a page.
        #[arg(long, default_value_t = 120)]
        page_timeout: u64,
        #[command(flatten)]
        write: commands::WriteArgs,
    },
    /// Write each range of pages to its own file in a directory, as
    /// `clean --pages` writes it: one page per file by default.
    Split {
        file: PathBuf,
        /// The directory for the parts, named after the input.
        #[arg(short, long)]
        output: PathBuf,
        /// Pages in each part.
        #[arg(long, default_value_t = 1)]
        pages_per_file: usize,
        /// The parts as page lists separated by `;`, such as `1-3;4-N`.
        #[arg(long)]
        ranges: Option<String>,
        /// Report what would be written, and write nothing.
        #[arg(long)]
        dry_run: bool,
        /// Skip reading each part back.
        #[arg(long)]
        no_verify: bool,
    },
    /// Merge the pages of several documents into a new one, with their
    /// external links and outline items, as `mutool merge` does.
    Merge {
        /// Input files, each followed by its pages if not all of them
        /// (`1-3,5`, `N`, `N-1`), as `mutool merge` takes them.
        #[arg(required = true, allow_negative_numbers = true)]
        inputs: Vec<String>,
        #[command(flatten)]
        write: commands::WriteArgs,
    },
    /// Encrypt with a new security handler, as `clean -E`.
    Encrypt {
        file: PathBuf,
        #[arg(long, default_value = "aes-256", value_parser = ["rc4-40", "rc4-128", "aes-128", "aes-256"])]
        method: String,
        /// Empty, the output opens without a password.
        #[arg(long, default_value = "")]
        user_password: String,
        #[arg(long, default_value = "")]
        owner_password: String,
        /// Permission flags, as /P (-1 grants everything).
        #[arg(long, default_value_t = -1, allow_negative_numbers = true)]
        permissions: i32,
        #[command(flatten)]
        write: commands::WriteArgs,
    },
    /// Write without encryption, as `clean -D`.
    Decrypt {
        file: PathBuf,
        #[command(flatten)]
        write: commands::WriteArgs,
    },
    /// Decode every stream: length and BLAKE3 of the data. Streams with an
    /// image filter are listed but not decoded, unless --images.
    Streams {
        file: PathBuf,
        /// Decode only the streams with an image filter.
        #[arg(long)]
        images: bool,
    },
    /// List each page's content-stream operators with their operands, as
    /// MuPDF's interpreter hands them to a processor.
    Operators {
        file: PathBuf,
        /// Process each page with empty resources, so that the result
        /// depends on its content streams alone.
        #[arg(long)]
        no_resources: bool,
    },
    /// List each page's device calls, as MuPDF's run processor makes them
    /// for a page's contents.
    Trace { file: PathBuf },
    /// Each page's structured text, as MuPDF's stext device gathers it
    /// from the page's contents: blocks, lines and characters with their
    /// quads, origins, fonts and sizes.
    Stext {
        file: PathBuf,
        /// MuPDF's stext options, comma-separated: preserve-ligatures,
        /// preserve-whitespace, preserve-spans, preserve-images,
        /// dehyphenate, inhibit-spaces, clip (on unless =no), clip-rect=
        /// x0:y0:x1:y1, use-cid-for-unknown-unicode,
        /// use-gid-for-unknown-unicode, ignore-actualtext, vectors,
        /// lazy-vectors, fuzzy-vectors, segment, paragraph-break,
        /// table-hunt; and nupdf's own reading-order (DECISIONS D38).
        #[arg(long, default_value = "")]
        options: String,
        /// Work on this many threads, a page per thread at a time; the
        /// output is the same at every count (DECISIONS D37).
        #[arg(long)]
        threads: Option<usize>,
    },
    /// Extract the pages' text with content ids (p3, p3.b2, p3.b2.l4,
    /// p3.b2.l4.s1): as JSON with blocks, lines and spans and their boxes
    /// and fonts, as JSON lines (a page per line), as plain text (as
    /// `mutool draw -F txt` writes it) or as Markdown.
    Extract {
        file: PathBuf,
        #[arg(long, value_enum, default_value = "json")]
        format: extract::Format,
        /// The pages, in MuPDF's syntax (`1-3,7`, `N` for the last).
        #[arg(long)]
        pages: Option<String>,
        /// MuPDF's stext options, as for `stext`.
        #[arg(long, default_value = "")]
        options: String,
        /// Stop before the block that would take the text past this many
        /// characters; the first block is always given.
        #[arg(long)]
        max_chars: Option<usize>,
        /// Stop after this many blocks (or pages, without blocks).
        #[arg(long)]
        max_items: Option<usize>,
        /// Start where an earlier call's `next_cursor` says.
        #[arg(long)]
        cursor: Option<String>,
        /// The JSON fields to give, comma-separated: page, bbox, blocks,
        /// type, lines, wmode, dir, spans, font, size, bold, italic, color,
        /// text, chars. Text goes at the deepest level kept. Everything but
        /// chars by default.
        #[arg(long)]
        fields: Option<String>,
        /// Work on this many threads, a page per thread at a time; the
        /// output is the same at every count (DECISIONS D37).
        #[arg(long)]
        threads: Option<usize>,
    },
    /// Find a needle in the pages' text, as MuPDF searches: each hit with
    /// its quads, its text and the content ids of its characters.
    Search {
        file: PathBuf,
        needle: String,
        /// MuPDF's search options, comma-separated: exact, ignore-case,
        /// ignore-diacritics, regexp, keep-lines, keep-paragraphs,
        /// keep-hyphens.
        #[arg(long, default_value = "")]
        options: String,
        /// MuPDF's stext options, as for `stext`.
        #[arg(long, default_value = "")]
        stext_options: String,
        /// The pages, in MuPDF's syntax (`1-3,7`, `N` for the last).
        #[arg(long)]
        pages: Option<String>,
        /// Stop after this many hits.
        #[arg(long)]
        max_items: Option<usize>,
        /// Work on this many threads, a page per thread at a time; the
        /// output is the same at every count (DECISIONS D37).
        #[arg(long)]
        threads: Option<usize>,
    },
    /// Remove text, images and line art under targets, and prove it is
    /// gone: the targets' hits are found on each page, the pages are
    /// redacted as MuPDF's pdf_redact_page does, the document is
    /// rewritten whole, and the output is searched again before it is
    /// kept. A target still found fails with exit 4 and writes nothing.
    Redact {
        file: PathBuf,
        /// Text to redact, matched as MuPDF's search matches it (across
        /// line breaks, ligatures decomposed). Repeatable.
        #[arg(long)]
        text: Vec<String>,
        /// A regular expression to redact. Repeatable.
        #[arg(long)]
        regex: Vec<String>,
        /// A preset: email, phone, card, ssn, iban, or pii for all of them.
        #[arg(long, value_parser = ["pii", "email", "phone", "card", "ssn", "iban"])]
        preset: Vec<String>,
        /// A region, `p3:[x0,y0,x1,y1]` in page space. Repeatable.
        #[arg(long)]
        region: Vec<String>,
        /// A content id from `extract` or `search`. Repeatable.
        #[arg(long)]
        id: Vec<String>,
        /// A `search` report whose hits to redact.
        #[arg(long, value_name = "FILE")]
        hits: Option<PathBuf>,
        /// Match text and patterns ignoring case.
        #[arg(long)]
        ignore_case: bool,
        /// The stext options targets are found with. Clipping is off and
        /// hidden layers are run by default, so text clipped away or in
        /// hidden optional content is found too.
        #[arg(long, default_value = "clip=no,all-layers")]
        stext_options: String,
        /// Only these pages, in MuPDF's syntax.
        #[arg(long)]
        pages: Option<String>,
        /// Fill each redacted area in black.
        #[arg(long)]
        boxes: bool,
        /// Images under a target: none, remove, pixels (blank the pixels
        /// covered; whole images removed until re-encoding lands) or
        /// remove-unless-invisible.
        #[arg(long, default_value = "pixels")]
        images: String,
        /// Line art under a target: none, covered (removed when a target
        /// covers it) or touched (removed when a target touches it).
        #[arg(long, default_value = "covered")]
        line_art: String,
        /// Where to write, replacing the file atomically.
        #[arg(short, long, required_unless_present = "dry_run")]
        output: Option<PathBuf>,
        /// Report the plan, the quads on each page, and write nothing.
        #[arg(long)]
        dry_run: bool,
        /// With --dry-run: write each page that has targets as
        /// page-NNNN.png into this directory, the areas to be removed
        /// shaded red.
        #[arg(long, value_name = "DIR", requires = "dry_run")]
        preview: Option<PathBuf>,
        /// The previews' resolution.
        #[arg(long, default_value_t = 72.0)]
        preview_dpi: f32,
        /// Write the output even when the check finds targets left; the
        /// report still gives what it found. For comparisons, such as
        /// with MuPDF's redaction, which leaves what this check catches.
        #[arg(long)]
        no_verify: bool,
    },
    /// Check a document for the targets, class by class (content text,
    /// structure, annotations and forms, metadata, outline and names,
    /// embedded files, hidden layers, images, history, JavaScript). Any
    /// leak fails with exit 4.
    Verify {
        file: PathBuf,
        /// A `redact` report: its targets, and the areas whose image
        /// pixels must be blank.
        #[arg(long, value_name = "REPORT")]
        against: Option<PathBuf>,
        /// Text to look for. Repeatable.
        #[arg(long)]
        text: Vec<String>,
        /// A regular expression to look for. Repeatable.
        #[arg(long)]
        regex: Vec<String>,
        /// A preset: email, phone, card, ssn, iban, or pii.
        #[arg(long, value_parser = ["pii", "email", "phone", "card", "ssn", "iban"])]
        preset: Vec<String>,
        /// Match ignoring case.
        #[arg(long)]
        ignore_case: bool,
    },
    /// Render pages as `mutool draw` paints them (page contents, without
    /// annotations), into page-NNNN.png (or .ppm) files.
    Render {
        file: PathBuf,
        /// Where to write the images.
        #[arg(short, long, value_name = "DIR")]
        output: PathBuf,
        /// The pages, in MuPDF's syntax (`1-3,7`, `N` for the last); all
        /// without it.
        #[arg(long)]
        pages: Option<String>,
        /// Resolution in dots per inch.
        #[arg(long, default_value_t = 72.0)]
        dpi: f32,
        #[arg(long, value_enum, default_value = "png")]
        format: render::Format,
        /// A page larger than this many pixels is not rendered.
        #[arg(long, default_value_t = 100_000_000)]
        max_pixels: u64,
        /// Only this part of each page, `x0,y0,x1,y1` in points from the
        /// page's top left.
        #[arg(long)]
        region: Option<String>,
        /// Render pages on this many threads; the images do not depend on
        /// it.
        #[arg(long, default_value_t = 1)]
        threads: usize,
        /// Convert colors by MuPDF's formulas instead of its ICC profiles,
        /// as `mutool draw -N`.
        #[arg(long)]
        no_icc: bool,
    },
    /// Each page's tables as rows of cells, found as MuPDF's table hunting
    /// finds them (`stext --options table-hunt`).
    Tables {
        file: PathBuf,
        /// Pages to look at, as for `extract`.
        #[arg(long)]
        pages: Option<String>,
        /// MuPDF's stext options, as for `stext`; table hunting is always on.
        #[arg(long, default_value = "")]
        options: String,
        /// `json` (the report, with each cell's box) or `csv` (the tables'
        /// rows, a blank line between tables; the report goes to stderr).
        #[arg(long, value_enum, default_value = "json")]
        format: TableFormat,
        /// Work on this many threads, a page per thread at a time; the
        /// output is the same at every count.
        #[arg(long)]
        threads: Option<usize>,
    },
    /// Each page's annotations (links, popups and form widgets aside):
    /// type, box, text, author, colours, dates, and the points of lines,
    /// polygons, ink and text markup; or, with --add, --delete or
    /// --highlight, write the document with annotations added or removed
    /// as MuPDF's annotation API makes them, appearances included.
    Annots {
        file: PathBuf,
        /// Make every annotation's appearance again and give it (for the
        /// harness: what MuPDF's appearance synthesis would write).
        #[arg(long, hide = true)]
        synthesize: bool,
        /// Add an annotation (repeatable): JSON in page space with the
        /// listing's field names, `{"page":1,"type":"Highlight",
        /// "quad_points":[[8 numbers]]}`; also "rect", "ink_list", "line",
        /// "vertices", "contents", "author", "subject", "color",
        /// "interior_color", "opacity", "icon", "border_width", "flags",
        /// "open" and "popup". A listed annotation with "page" added can be
        /// given as it is.
        #[arg(long = "add", value_name = "JSON")]
        add: Vec<String>,
        /// Delete an annotation and its popup by listing id (`p1.a2`;
        /// repeatable). Deletions come first, by the ids of the listing
        /// before any change.
        #[arg(long = "delete", value_name = "ID")]
        delete: Vec<String>,
        /// Add a Highlight over each hit of a search for this text
        /// (repeatable): one annotation per hit, with its quads.
        #[arg(long = "highlight", value_name = "TEXT")]
        highlight: Vec<String>,
        #[command(flatten)]
        write: commands::WriteArgs,
    },
    /// Each page's form widgets: field name, label, type, flags, value,
    /// options, maximum length, whether read-only or signed; and whether
    /// the form is an XFA form. With --set or --toggle, fills fields as
    /// MuPDF's widget API fills them (JavaScript off) and writes the file.
    Forms {
        file: PathBuf,
        /// Make every widget's appearance again and give it (for the
        /// harness: what MuPDF's appearance synthesis would write).
        #[arg(long, hide = true)]
        synthesize: bool,
        /// Set a field, as `NAME=VALUE` (repeatable), NAME its full name as
        /// listed. For a check box or radio button, VALUE is the on
        /// state's name, or Off. Sets go first, in order, then toggles.
        #[arg(long = "set", value_name = "NAME=VALUE")]
        set: Vec<String>,
        /// Toggle a check box or radio button (repeatable).
        #[arg(long, value_name = "NAME")]
        toggle: Vec<String>,
        /// Set the first text field to "nupdf" and toggle the first check
        /// box (for the harness).
        #[arg(long, hide = true)]
        fill_first: bool,
        #[command(flatten)]
        write: commands::WriteArgs,
    },
    /// Each page's signature fields: signed or not, the signatory, reason,
    /// time, the byte ranges signed, whether the file changed after signing,
    /// and whether the signed digest matches the signed bytes.
    Signatures { file: PathBuf },
    /// The outline (bookmarks): each entry's title, link, whether it is
    /// open, the page it leads to, and the entries below it.
    Outline { file: PathBuf },
    /// Each page's links: their boxes, where they lead (a URI, and the
    /// page for a link inside the document).
    Links { file: PathBuf },
    /// Each page's label, as a viewer shows it (`iv`, `A-3`), from the
    /// document's page labels or else its number.
    Labels { file: PathBuf },
    /// The embedded files: name, file name, type, size, dates, and whether
    /// the data matches its checksum; --extract writes each one out.
    Attachments {
        file: PathBuf,
        /// Write each embedded file into this directory, as
        /// attachment-NNNN-<name>.
        #[arg(long)]
        extract: Option<PathBuf>,
    },
    /// List the fonts the pages use, as `mutool info` finds them, with
    /// what each loads as; --extract writes the embedded font programs as
    /// `mutool extract` does.
    Fonts {
        file: PathBuf,
        /// Write each embedded program as font-NNNN.ext into this directory.
        #[arg(long, value_name = "DIR")]
        extract: Option<PathBuf>,
    },
    /// List the images the pages use, as `mutool info` finds them;
    /// --extract writes every image XObject as `mutool extract` does.
    Images {
        file: PathBuf,
        /// Decode every image XObject and give its pixmap's size,
        /// components and the MD5 of its samples, as MuPDF's Pixmap
        /// computeMD5 does.
        #[arg(long)]
        digests: bool,
        /// Write each image XObject as image-NNNN.ext into this directory.
        #[arg(long, value_name = "DIR")]
        extract: Option<PathBuf>,
        /// How --extract writes images.
        #[arg(long, value_enum, default_value = "auto")]
        format: inventory::ImageFormat,
    },
    /// Rewrite the whole document as `mutool clean` does: every object in a
    /// plain xref table, object and xref streams unpacked, encryption kept.
    /// The output is read back and compared object by object before it is
    /// written.
    Clean {
        file: PathBuf,
        /// Where to write, replacing the file atomically.
        #[arg(short, long, required_unless_present = "dry_run")]
        output: Option<PathBuf>,
        /// Garbage collect, as `mutool clean -g`: once drops objects
        /// nothing reaches, -gg also renumbers, -ggg also merges duplicate
        /// objects, -gggg also duplicate streams.
        #[arg(short, action = clap::ArgAction::Count)]
        garbage: u8,
        /// Write streams decoded, as `mutool clean -d`. XML metadata and
        /// JPEG 2000 images stay as they are.
        #[arg(short, action = clap::ArgAction::Count)]
        decompress: u8,
        /// Deflate streams that have no filter, as `mutool clean -z`; a
        /// 1-bit image is CCITT G4 encoded instead. Brotli (-zz) is not
        /// supported.
        #[arg(short = 'z', action = clap::ArgAction::Count)]
        compress: u8,
        /// Compress font streams that have no filter, and keep them encoded.
        #[arg(short = 'f', action = clap::ArgAction::Count)]
        compress_fonts: u8,
        /// Compress image streams that have no filter, and keep them encoded.
        #[arg(short = 'i', action = clap::ArgAction::Count)]
        compress_images: u8,
        /// Hex-encode binary streams, and write strings outside printable
        /// ASCII in hex.
        #[arg(short = 'a', action = clap::ArgAction::Count)]
        ascii: u8,
        /// Compression effort: 0 for zlib's default level, 1 to 100 for
        /// levels 0 to 9.
        #[arg(short = 'e', default_value_t = 0, allow_negative_numbers = true)]
        effort: i32,
        /// Object syntax: -t compact, -tt a dictionary entry per line. The
        /// default is compact, or a line per entry with -a or -d and no -z.
        #[arg(short = 't', action = clap::ArgAction::Count)]
        tight: u8,
        /// Pack objects into object streams and write an xref stream, as
        /// `mutool clean -Z`. The version becomes at least 1.5.
        #[arg(short = 'Z', action = clap::ArgAction::Count)]
        objstms: u8,
        /// Rewrite content streams, as `mutool clean -c`: every page's
        /// contents become one new stream, and the forms, patterns and
        /// glyphs they use are rewritten in place.
        #[arg(short = 'c', action = clap::ArgAction::Count)]
        clean_contents: u8,
        /// Sanitize content streams, as `mutool clean -s`: rewritten as
        /// with -c, with redundant graphics state dropped and the
        /// resources cut to what the contents use.
        #[arg(short = 's', action = clap::ArgAction::Count)]
        sanitize: u8,
        /// Write without encryption, as `mutool clean -D`.
        #[arg(short = 'D', conflicts_with = "encrypt")]
        decrypt: bool,
        /// Encrypt with a new security handler: rc4-40, rc4-128, aes-128 or
        /// aes-256, as `mutool clean -E`.
        #[arg(short = 'E', value_parser = ["rc4-40", "rc4-128", "aes-128", "aes-256"])]
        encrypt: Option<String>,
        /// Owner password for -E.
        #[arg(short = 'O', default_value = "")]
        owner_password: String,
        /// User password for -E. Empty, the output opens without one.
        #[arg(short = 'U', default_value = "")]
        user_password: String,
        /// Permission flags for -E, as /P (-1 grants everything).
        #[arg(short = 'P', default_value_t = -1, allow_negative_numbers = true)]
        permissions: i32,
        /// Keep only these pages, in this order, as `mutool clean`'s page
        /// arguments: `1-3,5`, `N` for the last page, `5-1` backwards.
        #[arg(long)]
        pages: Option<String>,
        /// Report what would be written, and write nothing.
        #[arg(long)]
        dry_run: bool,
        /// Skip reading the output back. It is then exactly what MuPDF
        /// writes, even where that does not read back the same.
        #[arg(long)]
        no_verify: bool,
    },
    /// Show one object (`12`, `12 0 R`) or the trailer (`trailer`).
    Show {
        file: PathBuf,
        target: String,
        /// Include the decoded stream data as text (lossy UTF-8).
        #[arg(long)]
        data: bool,
        /// Cap on the bytes of stream data included with --data.
        #[arg(long, default_value_t = 65536)]
        max_bytes: usize,
    },
    /// Compare two PDFs: their text (pages lined up, then lines, with
    /// content ids), their structure (pages, sizes, metadata, outline,
    /// fonts, annotations) and, on request, their pixels.
    Diff {
        a: PathBuf,
        b: PathBuf,
        /// What to compare, comma-separated.
        #[arg(
            long,
            value_enum,
            value_delimiter = ',',
            default_value = "text,structure"
        )]
        by: Vec<diff::By>,
        /// MuPDF's stext options for the text, as for `stext`.
        #[arg(long, default_value = "")]
        options: String,
        /// Resolution pages are drawn at for `pixels`.
        #[arg(long, default_value_t = 36.0)]
        dpi: f32,
        /// Most differences reported of each kind; past it, `truncated`.
        #[arg(long, default_value_t = 500)]
        max_items: usize,
    },
    /// Run a command on many files: each as `nupdf <command> <file> ...`,
    /// several at once, a JSON line per file in the order given, then the
    /// summary. `{stem}`, `{name}` and `{index}` in the command's arguments
    /// become each file's name without extension, its name and its number.
    Batch {
        /// Files, directories (their PDFs, at any depth) and glob patterns
        /// (`docs/**/*.pdf`).
        #[arg(required = true)]
        inputs: Vec<String>,
        /// Files worked on at once; 0 for one per core.
        #[arg(long, default_value_t = 0)]
        jobs: usize,
        /// Write the files' lines here instead of to stdout; the summary
        /// still goes to stdout.
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Take up an earlier run: skip the files --output already has a
        /// line for, if they have not changed, and add to it.
        #[arg(long)]
        resume: bool,
        /// Seconds each file may take, after which it fails with
        /// `limit.timeout`.
        #[arg(long)]
        item_timeout: Option<f64>,
        /// The command and its arguments, after `--`.
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// The JSON Schema of a report: a command's (`extract`), or one of its
    /// parts (`extract.page`, `batch.item`, `error`); without a name, the
    /// names there are.
    Schema { name: Option<String> },
    /// How to use nupdf, written for models: conventions, where to start,
    /// and every command with its options. Plain text, not JSON.
    Guide {
        /// One command's options in full.
        command: Option<String>,
        /// The Claude Code skill (SKILL.md) made of the guide.
        #[arg(long, conflicts_with = "command")]
        skill: bool,
    },
    /// Serve the commands as tools of a Model Context Protocol server, over
    /// stdin and stdout.
    Mcp {
        /// Seconds a call may run before it fails with `limit.timeout`,
        /// unless the call gives its own `timeout`.
        #[arg(long, default_value_t = 300.0)]
        call_timeout: f64,
        /// Largest output a call may return, in bytes; a call over it fails
        /// with `limit.output` (use the commands' budgets instead).
        #[arg(long, default_value_t = 8 << 20)]
        max_output_bytes: usize,
    },
}

/// How `tables` writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum TableFormat {
    Json,
    Csv,
}

/// Exit codes from docs/AGENT-INTERFACE.md. Usage errors (2) come from clap.
#[derive(Debug, Clone, Copy)]
enum Exit {
    Internal = 1,
    Usage = 2,
    Input = 3,
    Verify = 4,
    Limit = 5,
}

#[derive(Debug, Serialize)]
struct Diagnostic {
    code: &'static str,
    severity: &'static str,
    message: String,
}

#[derive(Debug, Serialize)]
struct InfoReport {
    schema: &'static str,
    doc_id: String,
    file_size: usize,
    kind: &'static str,
    /// Effective version: the header's unless /Root/Version overrides it.
    pdf_version: String,
    /// Version in the header; 1.7 when there is no header.
    header_version: String,
    /// Bytes of junk before the header, or null when there is no header.
    header_offset: Option<usize>,
    /// MuPDF's description of the security handler, or "None".
    encryption: String,
    /// The empty password does not open the document. Pages are not
    /// counted then, unless --password opened it.
    needs_password: bool,
    /// What the document allows, by MuPDF's permission names.
    permissions: serde_json::Map<String, serde_json::Value>,
    page_count: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    page_count_error: Option<String>,
    /// Slots in the cross-reference table.
    object_count: usize,
    revisions: usize,
    /// Absolute offsets of the xref sections, newest first (empty after repair).
    revision_offsets: Vec<u64>,
    linearized: bool,
    repaired: bool,
    encrypted: bool,
    /// Info dictionary entries that are non-empty text strings.
    metadata: serde_json::Map<String, serde_json::Value>,
    diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Serialize)]
struct ErrorReport<'a> {
    schema: &'static str,
    error: ErrorBody<'a>,
}

#[derive(Debug, Serialize)]
struct ErrorBody<'a> {
    code: &'a str,
    message: String,
}

struct Failure {
    exit: Exit,
    code: &'static str,
    message: String,
}

/// The stack commands run on. Content streams nest as deep as MuPDF's
/// graphics state stack lets them (about a thousand forms), and each level
/// is several frames of the interpreter; the pages are only touched as
/// they are used.
const STACK_SIZE: usize = 1 << 30;

/// A command's exit code: 0, or an [`Exit`] as a number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ExitCode(u8);

impl ExitCode {
    const SUCCESS: ExitCode = ExitCode(0);

    fn from(code: u8) -> ExitCode {
        ExitCode(code)
    }
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    if let Command::Mcp {
        call_timeout,
        max_output_bytes,
    } = &cli.command
    {
        return std::process::ExitCode::from(mcp::serve(*call_timeout, *max_output_bytes));
    }
    match std::thread::Builder::new()
        .stack_size(STACK_SIZE)
        .spawn(move || run_timed(cli))
        .map(std::thread::JoinHandle::join)
    {
        Ok(Ok(code)) => std::process::ExitCode::from(code.0),
        _ => std::process::ExitCode::FAILURE,
    }
}

/// [`run`] under `--timeout`, if given: a command that runs out of time
/// writes nothing and fails with exit 5.
fn run_timed(cli: Cli) -> ExitCode {
    let deadline = cli.timeout.and_then(|t| {
        std::time::Instant::now()
            .checked_add(std::time::Duration::try_from_secs_f64(t.max(0.0)).ok()?)
    });
    let pretty = cli.pretty;
    let interrupt = deadline.map(|d| nupdf::interrupt::Interrupt::new(Some(d)));
    let code = nupdf::interrupt::within(interrupt.clone(), || run(cli));
    match interrupt.and_then(|i| i.reason()) {
        Some(_) => fail(timed_out(), pretty),
        None => code,
    }
}

/// What a command run in-process wrote, and its exit code.
struct Ran {
    code: ExitCode,
    out: Vec<u8>,
    err: Vec<u8>,
}

/// The command `line` (`nupdf` and its arguments) run as the CLI runs it,
/// on a thread of its own under `interrupt`, with what it writes kept: the
/// MCP server's calls and `batch`'s files.
fn run_line(
    line: &[String],
    interrupt: Option<std::sync::Arc<nupdf::interrupt::Interrupt>>,
) -> Result<Ran, Failure> {
    use clap::Parser;
    let cli = Cli::try_parse_from(line).map_err(|e| Failure {
        exit: Exit::Usage,
        code: "usage.bad_argument",
        message: e.render().to_string().trim().to_string(),
    })?;
    let ran = std::thread::Builder::new()
        .stack_size(STACK_SIZE)
        .spawn(move || {
            capture::keep(|| {
                nupdf::interrupt::within(interrupt, || {
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(cli)))
                })
            })
        })
        .map(std::thread::JoinHandle::join);
    match ran {
        Ok(Ok((Ok(code), out, err))) => Ok(Ran { code, out, err }),
        _ => Err(Failure {
            exit: Exit::Internal,
            code: "internal.panic",
            message: "the command stopped on a bug in nupdf".into(),
        }),
    }
}

/// The failure of a call that ran out of time.
fn timed_out() -> Failure {
    Failure {
        exit: Exit::Limit,
        code: "limit.timeout",
        message: "the command ran out of time (--timeout)".into(),
    }
}

fn run(cli: Cli) -> ExitCode {
    let limits = nupdf::Limits {
        max_decoded_bytes: cli.max_decoded_bytes,
        max_xref_entries: cli.max_xref_entries,
        ..nupdf::Limits::default()
    };
    let password = cli.password.as_deref();
    let open = |file: &Path| open(file, limits, password);
    let result = match &cli.command {
        Command::Info { file } => open(file).and_then(|opened| {
            // On a copy of its own, so that drawing the pages to look at
            // them adds nothing to what `info` says of the file.
            let text = Document::open(opened.0.data().to_vec())
                .map(|copy| {
                    if let Some(pw) = password {
                        let _ = copy.authenticate(pw);
                    }
                    ocr::info_text(&copy)
                })
                .unwrap_or(serde_json::Value::Null);
            let mut v = info(opened, password.is_some()).and_then(to_json)?;
            v["text"] = text;
            Ok(v)
        }),
        Command::Objects { file } => match open(file) {
            Ok((doc, facts)) => return stream_objects(&doc, &facts, cli.pretty),
            Err(failure) => Err(failure),
        },
        Command::Pages {
            file,
            select,
            delete,
            rotate,
            rotate_pages,
            write,
        } => {
            let edits = commands::PageEdits {
                select: select.as_deref(),
                delete: delete.as_deref(),
                rotate: *rotate,
                rotate_pages: rotate_pages.as_deref(),
            };
            open(file).and_then(|(doc, facts)| {
                if edits.any() {
                    commands::edit_pages(&doc, &facts, &edits, write, password)
                } else {
                    Ok(report("nupdf.pages/v1", &facts, &doc, objects::pages(&doc)))
                }
            })
        }
        Command::Meta {
            file,
            set,
            strip,
            write,
        } => open(file)
            .and_then(|(doc, facts)| commands::meta(&doc, &facts, set, *strip, write, password)),
        Command::Flatten {
            file,
            annots_only,
            widgets_only,
            write,
        } => open(file).and_then(|(doc, facts)| {
            commands::flatten(&doc, &facts, !widgets_only, !annots_only, write, password)
        }),
        Command::Ocr {
            file,
            pages,
            all,
            lang,
            dpi,
            tesseract,
            min_confidence,
            page_timeout,
            write,
        } => open(file).and_then(|(doc, facts)| {
            let args = ocr::OcrArgs {
                pages: pages.as_deref(),
                all: *all,
                lang,
                dpi: *dpi,
                tesseract,
                min_confidence: *min_confidence,
                timeout: *page_timeout,
            };
            ocr::ocr(&doc, &facts, &args, write, password)
        }),
        Command::Sanitize {
            file,
            keep,
            uri,
            attachments,
            write,
        } => commands::sanitize_policy(keep, uri, *attachments).and_then(|policy| {
            open(file)
                .and_then(|(doc, facts)| commands::sanitize(&doc, &facts, &policy, write, password))
        }),
        Command::Split {
            file,
            output,
            pages_per_file,
            ranges,
            dry_run,
            no_verify,
        } => commands::split(
            file,
            &open,
            output,
            *pages_per_file,
            ranges.as_deref(),
            *dry_run,
            *no_verify,
            password,
        ),
        Command::Merge { inputs, write } => commands::merge(inputs, &open, write, password),
        Command::Encrypt {
            file,
            method,
            user_password,
            owner_password,
            permissions,
            write,
        } => {
            let encrypt = nupdf::crypt::NewMethod::from_name(method).map(|method| {
                nupdf::write::Encrypt::New {
                    method,
                    owner: owner_password.clone(),
                    user: user_password.clone(),
                    permissions: *permissions,
                }
            });
            open(file).and_then(|(doc, facts)| {
                let encrypt = encrypt.ok_or_else(|| Failure {
                    exit: Exit::Usage,
                    code: "usage.bad_value",
                    message: "unknown encryption method".into(),
                })?;
                commands::encryption(&doc, &facts, encrypt, write, password)
            })
        }
        Command::Decrypt { file, write } => open(file).and_then(|(doc, facts)| {
            commands::encryption(&doc, &facts, nupdf::write::Encrypt::Remove, write, password)
        }),
        Command::Streams { file, images } => open(file).map(|(doc, facts)| {
            report(
                "nupdf.streams/v1",
                &facts,
                &doc,
                objects::streams(&doc, *images),
            )
        }),
        Command::Operators { file, no_resources } => open(file).map(|(doc, facts)| {
            report(
                "nupdf.operators/v1",
                &facts,
                &doc,
                objects::operators(&doc, *no_resources),
            )
        }),
        Command::Trace { file } => open(file)
            .map(|(doc, facts)| report("nupdf.trace/v1", &facts, &doc, trace::trace(&doc))),
        Command::Extract {
            file,
            format,
            pages,
            options,
            max_chars,
            max_items,
            cursor,
            fields,
            threads,
        } => {
            let usage = |message: String| Failure {
                exit: Exit::Usage,
                code: "usage.bad_option",
                message,
            };
            let opts = match nupdf::stext::Options::parse(options) {
                Ok(o) => o,
                Err(e) => return fail(usage(e), cli.pretty),
            };
            let (doc, facts) = match open(file) {
                Ok(v) => v,
                Err(failure) => return fail(failure, cli.pretty),
            };
            let args = extract::Args {
                format: *format,
                pages: pages.as_deref(),
                options: opts,
                max_chars: *max_chars,
                max_items: *max_items,
                cursor: cursor.as_deref(),
                fields: fields.as_deref(),
                threads: *threads,
                pretty: cli.pretty,
            };
            let extracted = match extract::extract(&doc, &facts.doc_id, &args) {
                Ok(e) => e,
                Err(e) => return fail(usage(e), cli.pretty),
            };
            return emit_extracted(extracted, &facts, &doc, cli.pretty);
        }
        Command::Search {
            file,
            needle,
            options,
            stext_options,
            pages,
            max_items,
            threads,
        } => {
            let usage = |message: String| Failure {
                exit: Exit::Usage,
                code: "usage.bad_option",
                message,
            };
            nupdf::search::parse_options(options)
                .map_err(usage)
                .and_then(|o| {
                    nupdf::stext::Options::parse(stext_options)
                        .map_err(usage)
                        .map(|s| (o, s))
                })
                .and_then(|(o, s)| open(file).map(|(doc, facts)| (o, s, doc, facts)))
                .and_then(|(o, s, doc, facts)| {
                    let args = search::Args {
                        needle,
                        options: o,
                        stext: s,
                        pages: pages.as_deref(),
                        max_items: *max_items,
                        threads: *threads,
                    };
                    search::search(&doc, &args)
                        .map_err(usage)
                        .map(|body| report("nupdf.search/v1", &facts, &doc, body))
                })
        }
        Command::Redact {
            file,
            text,
            regex,
            preset,
            region,
            id,
            hits,
            ignore_case,
            stext_options,
            pages,
            boxes,
            images,
            line_art,
            output,
            dry_run,
            preview,
            preview_dpi,
            no_verify,
        } => open(file).and_then(|(doc, facts)| {
            let usage = |message: String| Failure {
                exit: Exit::Usage,
                code: "usage.bad_option",
                message,
            };
            if !(*preview_dpi > 0.0 && *preview_dpi <= 1200.0) {
                return Err(usage(format!(
                    "bad --preview-dpi {preview_dpi}: expected more than 0, at most 1200"
                )));
            }
            let preview = match preview {
                Some(dir) => Some((output_dir(dir)?, *preview_dpi)),
                None => None,
            };
            let mut targets: Vec<redact::Target> = Vec::new();
            targets.extend(text.iter().cloned().map(redact::Target::Text));
            targets.extend(regex.iter().cloned().map(redact::Target::Regex));
            targets.extend(preset.iter().cloned().map(redact::Target::Preset));
            for r in region {
                targets.push(redact::parse_region(r).map_err(usage)?);
            }
            targets.extend(id.iter().cloned().map(redact::Target::Id));
            if let Some(path) = hits {
                let data = std::fs::read_to_string(path).map_err(|e| Failure {
                    exit: Exit::Input,
                    code: "input.unreadable",
                    message: format!("cannot read {}: {e}", path.display()),
                })?;
                targets.push(redact::parse_hits(&data, &facts.doc_id).map_err(usage)?);
            }
            if targets.is_empty() {
                return Err(usage(
                    "nothing to redact: give --text, --regex, --preset, --region, --id or --hits"
                        .into(),
                ));
            }
            let args = redact::Args {
                targets: &targets,
                ignore_case: *ignore_case,
                stext: nupdf::stext::Options::parse(stext_options).map_err(usage)?,
                options: nupdf::redact::RedactOptions {
                    black_boxes: *boxes,
                    image_method: redact::image_method(images).map_err(usage)?,
                    line_art: redact::line_art(line_art).map_err(usage)?,
                    text: nupdf::redact::TextMethod::Remove,
                },
                pages: pages.as_deref(),
            };
            redact_command(
                &doc,
                &facts,
                &args,
                output.as_deref(),
                (*dry_run, *no_verify),
                password,
                preview,
            )
        }),
        Command::Verify {
            file,
            against,
            text,
            regex,
            preset,
            ignore_case,
        } => open(file).and_then(|(doc, facts)| {
            let usage = |message: String| Failure {
                exit: Exit::Usage,
                code: "usage.bad_option",
                message,
            };
            let mut targets: Vec<redact::Target> = Vec::new();
            targets.extend(text.iter().cloned().map(redact::Target::Text));
            targets.extend(regex.iter().cloned().map(redact::Target::Regex));
            targets.extend(preset.iter().cloned().map(redact::Target::Preset));
            let mut plan = None;
            if let Some(path) = against {
                let data = std::fs::read_to_string(path).map_err(|e| Failure {
                    exit: Exit::Input,
                    code: "input.unreadable",
                    message: format!("cannot read {}: {e}", path.display()),
                })?;
                let (t, p) = redact::parse_against(&data).map_err(usage)?;
                targets.extend(t);
                plan = Some(p);
            }
            if targets.is_empty() && plan.is_none() {
                return Err(usage(
                    "nothing to look for: give --against, --text, --regex or --preset".into(),
                ));
            }
            let t = redact::verify_targets(&targets, *ignore_case, plan.as_ref()).map_err(usage)?;
            let v = nupdf::verify::verify(&doc, &t);
            let mut body = serde_json::Map::new();
            body.insert("verified".into(), v.verified().into());
            body.insert("verification".into(), redact::verification_json(&v));
            let out = report("nupdf.verify/v1", &facts, &doc, body.into());
            if v.verified() {
                Ok(out)
            } else {
                // The report goes to stdout before the failure.
                emit(&mut standard_output(), &out, false);
                Err(Failure {
                    exit: Exit::Verify,
                    code: "verify.failed",
                    message: "targets were found".into(),
                })
            }
        }),
        Command::Stext {
            file,
            options,
            threads,
        } => {
            let opts = match nupdf::stext::Options::parse(options) {
                Ok(o) => o,
                Err(message) => {
                    let usage = Failure {
                        exit: Exit::Usage,
                        code: "usage.bad_option",
                        message,
                    };
                    return fail(usage, cli.pretty);
                }
            };
            let (doc, facts) = match open(file) {
                Ok(v) => v,
                Err(failure) => return fail(failure, cli.pretty),
            };
            let body = stext::stext(&doc, opts, *threads, cli.pretty);
            let stdout = &mut standard_output();
            if cli.pretty {
                let report = StextReport {
                    facts: &facts,
                    doc: &doc,
                    body,
                };
                emit(stdout, &report, true);
            } else {
                let mut out = std::io::BufWriter::new(stdout);
                // A closed pipe is not ours to report.
                let written = body.write_compact(
                    &mut out,
                    "nupdf.stext/v1",
                    &facts.doc_id,
                    &diagnostics(&doc),
                );
                if written.is_ok() {
                    let _ = writeln!(out);
                }
                let _ = out.flush();
            }
            return ExitCode::SUCCESS;
        }
        Command::Tables {
            file,
            pages,
            options,
            format,
            threads,
        } => {
            let opts = match nupdf::stext::Options::parse(options) {
                Ok(o) => o,
                Err(e) => {
                    return fail(
                        Failure {
                            exit: Exit::Usage,
                            code: "usage.bad_option",
                            message: e,
                        },
                        cli.pretty,
                    );
                }
            };
            let (doc, facts) = match open(file) {
                Ok(v) => v,
                Err(failure) => return fail(failure, cli.pretty),
            };
            // Counted before any page loads: loading one can repair the document.
            let page_count = doc.page_count().ok();
            let count = page_count.unwrap_or(0);
            let indices: Vec<i32> = match pages {
                Some(spec) => nupdf::rearrange::parse_page_ranges(spec, count, &doc),
                None => (0..count).collect(),
            };
            let body = tables::tables(&doc, page_count, &indices, opts, *threads);
            if *format == TableFormat::Csv {
                let mut out = standard_output();
                let _ = out.write_all(tables::csv(&body).as_bytes());
                let _ = out.flush();
                let mut body = body;
                if let Some(m) = body.as_object_mut() {
                    m.remove("pages");
                }
                emit(
                    &mut standard_error(),
                    &report("nupdf.tables/v1", &facts, &doc, body),
                    cli.pretty,
                );
                return ExitCode::SUCCESS;
            }
            Ok(report("nupdf.tables/v1", &facts, &doc, body))
        }
        Command::Outline { file } => open(file).and_then(|(doc, facts)| {
            let body = navigation::outline(&doc).map_err(|e| Failure {
                exit: Exit::Input,
                code: "input.outline",
                message: nupdf::diag::report::strip(&e).to_string(),
            })?;
            Ok(report("nupdf.outline/v1", &facts, &doc, body))
        }),
        Command::Forms {
            file,
            set,
            toggle,
            fill_first,
            write,
            ..
        } if !set.is_empty() || !toggle.is_empty() || *fill_first => {
            open(file).and_then(|(doc, facts)| {
                commands::fill(&doc, &facts, set, toggle, *fill_first, write, password)
            })
        }
        Command::Annots {
            file,
            add,
            delete,
            highlight,
            write,
            ..
        } if !(add.is_empty() && delete.is_empty() && highlight.is_empty()) => {
            open(file).and_then(|(doc, facts)| {
                let edits = annot_edits::AnnotEdits {
                    add,
                    delete,
                    highlight,
                };
                annot_edits::edit_annots(&doc, &facts, &edits, write, password)
            })
        }
        Command::Links { file }
        | Command::Labels { file }
        | Command::Forms { file, .. }
        | Command::Signatures { file }
        | Command::Annots { file, .. } => open(file).and_then(|(doc, facts)| {
            let (schema, body) = match &cli.command {
                Command::Links { .. } => ("nupdf.links/v1", navigation::links(&doc)),
                Command::Forms { synthesize, .. } => {
                    ("nupdf.forms/v1", navigation::forms(&doc, *synthesize))
                }
                Command::Signatures { .. } => ("nupdf.signatures/v1", navigation::signatures(&doc)),
                Command::Annots { synthesize, .. } => {
                    ("nupdf.annots/v1", navigation::annots(&doc, *synthesize))
                }
                _ => ("nupdf.labels/v1", navigation::labels(&doc)),
            };
            let body = body.map_err(|e| Failure {
                exit: Exit::Input,
                code: "input.page_tree",
                message: e,
            })?;
            Ok(report(schema, &facts, &doc, body))
        }),
        Command::Attachments { file, extract } => open(file).and_then(|(doc, facts)| {
            let dir = extract.as_deref().map(output_dir).transpose()?;
            Ok(report(
                "nupdf.attachments/v1",
                &facts,
                &doc,
                navigation::attachments(&doc, dir),
            ))
        }),
        Command::Fonts { file, extract } => open(file).and_then(|(doc, facts)| {
            let dir = extract.as_deref().map(output_dir).transpose()?;
            Ok(report(
                "nupdf.fonts/v1",
                &facts,
                &doc,
                inventory::fonts(&doc, dir),
            ))
        }),
        Command::Render {
            file,
            output,
            pages,
            dpi,
            format,
            max_pixels,
            region,
            threads,
            no_icc,
        } => open(file).and_then(|(doc, facts)| {
            let region = region
                .as_deref()
                .map(render::parse_region)
                .transpose()
                .map_err(|message| Failure {
                    exit: Exit::Usage,
                    code: "usage.bad_option",
                    message,
                })?;
            if !(*dpi > 0.0 && *dpi <= 9600.0) {
                return Err(Failure {
                    exit: Exit::Usage,
                    code: "usage.bad_option",
                    message: format!("bad --dpi {dpi}: expected more than 0, at most 9600"),
                });
            }
            let dir = output_dir(output)?;
            let settings = render::Settings {
                dpi: *dpi,
                region,
                max_pixels: *max_pixels,
                threads: (*threads).max(1),
                icc: !*no_icc,
            };
            let body = render::render(&doc, pages.as_deref(), dir, *format, &settings).map_err(
                |message| Failure {
                    exit: Exit::Input,
                    code: "input.page_tree",
                    message,
                },
            )?;
            Ok(report("nupdf.render/v1", &facts, &doc, body))
        }),
        Command::Images {
            file,
            digests,
            extract,
            format,
        } => open(file).and_then(|(doc, facts)| {
            let dir = extract.as_deref().map(output_dir).transpose()?;
            Ok(report(
                "nupdf.images/v1",
                &facts,
                &doc,
                inventory::images(&doc, *digests, dir, *format),
            ))
        }),
        Command::Clean {
            file,
            output,
            garbage,
            decompress,
            compress,
            compress_fonts,
            compress_images,
            ascii,
            effort,
            tight,
            objstms,
            clean_contents,
            sanitize,
            decrypt,
            encrypt,
            owner_password,
            user_password,
            permissions,
            pages,
            dry_run,
            no_verify,
        } => clean_options(&CleanFlags {
            garbage: *garbage,
            decompress: *decompress,
            compress: *compress,
            fonts: *compress_fonts,
            images: *compress_images,
            ascii: *ascii,
            tight: *tight,
            objstms: *objstms,
            effort: *effort,
            encrypt: match (encrypt, *decrypt) {
                (Some(method), _) => nupdf::crypt::NewMethod::from_name(method).map(|method| {
                    nupdf::write::Encrypt::New {
                        method,
                        owner: owner_password.clone(),
                        user: user_password.clone(),
                        permissions: *permissions,
                    }
                }),
                (None, true) => Some(nupdf::write::Encrypt::Remove),
                (None, false) => Some(nupdf::write::Encrypt::Keep),
            },
        })
        .and_then(|options| open(file).map(|opened| (options, opened)))
        .and_then(|(options, (doc, facts))| {
            let kept = match pages {
                Some(spec) => Some(keep_pages(&doc, spec)?),
                None => None,
            };
            if *clean_contents > 0 || *sanitize > 0 {
                // pdf_clean_file loads every object first, so a repair
                // cannot lose the rewritten streams.
                nupdf::write::check_document(&doc);
                nupdf::content_clean::clean_content_streams(
                    &doc,
                    *sanitize > 0,
                    options.ascii,
                    options.pretty,
                )
                .map_err(|e| Failure {
                    exit: Exit::Input,
                    code: "input.content_stream",
                    message: nupdf::diag::report::strip(&e).to_string(),
                })?;
            }
            let plan = Clean {
                options,
                output: output.as_deref(),
                apply: !dry_run,
                verify: !no_verify,
                password,
                pages: kept,
            };
            clean(&doc, &facts, &plan)
        }),
        Command::Show {
            file,
            target,
            data,
            max_bytes,
        } => open(file).and_then(|opened| show(opened, target, *data, *max_bytes)),
        Command::Diff {
            a,
            b,
            by,
            options,
            dpi,
            max_items,
        } => nupdf::stext::Options::parse(options)
            .map_err(|e| Failure {
                exit: Exit::Usage,
                code: "usage.bad_option",
                message: e,
            })
            .and_then(|options| Ok((options, open(a)?, open(b)?)))
            .map(|(options, (da, fa), (db, fb))| {
                let args = diff::Args {
                    by,
                    options,
                    dpi: dpi.clamp(1.0, 600.0),
                    max_items: *max_items,
                };
                let body = diff::diff(&da, &db, &args);
                let side = |path: &Path, doc: &Document, facts: &FileFacts| {
                    serde_json::json!({
                        "file": path.to_string_lossy(),
                        "doc_id": facts.doc_id,
                        "page_count": doc.page_count().ok(),
                    })
                };
                let mut out = serde_json::Map::new();
                out.insert("schema".into(), "nupdf.diff/v1".into());
                out.insert("doc_id".into(), fa.doc_id.clone().into());
                out.insert("a".into(), side(a, &da, &fa));
                out.insert("b".into(), side(b, &db, &fb));
                if let serde_json::Value::Object(fields) = body {
                    out.extend(fields);
                }
                let mut diags = Vec::new();
                for (which, doc) in [("a", &da), ("b", &db)] {
                    if let serde_json::Value::Array(list) = diagnostics(doc) {
                        for mut d in list {
                            d["document"] = which.into();
                            diags.push(d);
                        }
                    }
                }
                out.insert("diagnostics".into(), diags.into());
                serde_json::Value::Object(out)
            }),
        Command::Batch {
            inputs,
            jobs,
            output,
            resume,
            item_timeout,
            command,
        } => batch::run(&batch::Args {
            inputs,
            jobs: *jobs,
            output: output.as_deref(),
            resume: *resume,
            item_timeout: *item_timeout,
            command,
        })
        .map(|summary| {
            let mut out = serde_json::Map::new();
            out.insert("schema".into(), "nupdf.batch/v1".into());
            out.insert("doc_id".into(), serde_json::Value::Null);
            if let serde_json::Value::Object(fields) = summary {
                out.extend(fields);
            }
            out.insert("diagnostics".into(), serde_json::json!([]));
            serde_json::Value::Object(out)
        }),
        Command::Schema { name } => match name {
            None => Ok(serde_json::json!({
                "schema": "nupdf.schema/v1",
                "doc_id": null,
                "schemas": schemas::SCHEMAS.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
                "diagnostics": [],
            })),
            Some(name) => {
                // `extract`, or the report's own `nupdf.extract/v1`.
                let short = name
                    .strip_prefix("nupdf.")
                    .map_or(name.as_str(), |n| n.split('/').next().unwrap_or(n));
                match schemas::SCHEMAS.iter().find(|(n, _)| *n == short) {
                    Some((_, text)) => {
                        let mut out = standard_output();
                        let _ = out.write_all(text.as_bytes());
                        let _ = out.flush();
                        return ExitCode::SUCCESS;
                    }
                    None => Err(Failure {
                        exit: Exit::Usage,
                        code: "usage.bad_command",
                        message: format!("no report named {name}: `nupdf schema` lists them"),
                    }),
                }
            }
        },
        Command::Guide { command, skill } => {
            let text = match (command, skill) {
                (_, true) => Some(guide::skill()),
                (Some(c), false) => guide::command(c),
                (None, false) => Some(guide::guide()),
            };
            let Some(text) = text else {
                return fail(
                    Failure {
                        exit: Exit::Usage,
                        code: "usage.bad_command",
                        message: format!("no command named {}", command.as_deref().unwrap_or("")),
                    },
                    cli.pretty,
                );
            };
            let mut out = standard_output();
            let _ = out.write_all(text.as_bytes());
            let _ = out.flush();
            return ExitCode::SUCCESS;
        }
        Command::Mcp { .. } => Err(Failure {
            exit: Exit::Usage,
            code: "usage.bad_command",
            message: "the MCP server cannot be started from within a call".into(),
        }),
    };
    match result {
        Ok(report) => {
            emit(&mut standard_output(), &report, cli.pretty);
            ExitCode::SUCCESS
        }
        Err(failure) => fail(failure, cli.pretty),
    }
}

fn fail(failure: Failure, pretty: bool) -> ExitCode {
    let report = ErrorReport {
        schema: "nupdf.error/v1",
        error: ErrorBody {
            code: failure.code,
            message: failure.message,
        },
    };
    emit(&mut standard_error(), &report, pretty);
    ExitCode::from(failure.exit as u8)
}

/// Writes `value` as one line of JSON. Serializing can only fail on a write
/// error, and a closed pipe is not ours to report.
fn emit(out: &mut impl Write, value: &impl Serialize, pretty: bool) {
    let mut out = std::io::BufWriter::new(out);
    let written = if pretty {
        serde_json::to_writer_pretty(&mut out, value)
    } else {
        serde_json::to_writer(&mut out, value)
    };
    if written.is_ok() {
        let _ = writeln!(out);
    }
    let _ = out.flush();
}

/// Standard output without `Stdout`'s line buffering, which looks for the
/// last newline in everything written: for compact JSON, a pass over the
/// whole report on one thread after the pages are done. Where the
/// descriptor cannot be duplicated, `Stdout` itself.
fn standard_output() -> Box<dyn Write> {
    if let Some(out) = capture::writer(capture::Stream::Out) {
        return out;
    }
    let _ = std::io::stdout().flush();
    #[cfg(unix)]
    {
        use std::os::fd::AsFd;
        if let Ok(fd) = std::io::stdout().as_fd().try_clone_to_owned() {
            return Box::new(std::fs::File::from(fd));
        }
    }
    Box::new(std::io::stdout().lock())
}

/// Standard error, or where a call's errors are kept.
fn standard_error() -> Box<dyn Write> {
    capture::writer(capture::Stream::Err).unwrap_or_else(|| Box::new(std::io::stderr()))
}

/// Where a call's output goes when it does not go to the process's own
/// streams: the MCP server runs each call with its output kept, and an
/// interrupted call's output goes nowhere.
mod capture {
    use std::cell::RefCell;
    use std::io::Write;
    use std::rc::Rc;

    #[derive(Clone, Copy)]
    pub enum Stream {
        Out,
        Err,
    }

    type Buffer = Rc<RefCell<Vec<u8>>>;

    thread_local! {
        static KEPT: RefCell<Option<(Buffer, Buffer)>> = const { RefCell::new(None) };
    }

    /// A writer for `stream` if this thread's output is kept or dropped.
    pub fn writer(stream: Stream) -> Option<Box<dyn Write>> {
        if nupdf::interrupt::interrupted() {
            return Some(Box::new(std::io::sink()));
        }
        KEPT.with(|k| {
            k.borrow().as_ref().map(|(out, err)| {
                let b = match stream {
                    Stream::Out => out.clone(),
                    Stream::Err => err.clone(),
                };
                Box::new(Kept(b)) as Box<dyn Write>
            })
        })
    }

    struct Kept(Buffer);

    impl Write for Kept {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(data);
            Ok(data.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// `f` with this thread's output kept; gives back what it wrote to
    /// stdout and to stderr.
    pub fn keep<R>(f: impl FnOnce() -> R) -> (R, Vec<u8>, Vec<u8>) {
        let (out, err): (Buffer, Buffer) = Default::default();
        let before = KEPT.with(|k| k.replace(Some((out.clone(), err.clone()))));
        let r = f();
        KEPT.with(|k| *k.borrow_mut() = before);
        let take = |b: Buffer| std::mem::take(&mut *b.borrow_mut());
        (r, take(out), take(err))
    }
}

/// A `stext` report with its pages already written as JSON.
struct StextReport<'a> {
    facts: &'a FileFacts,
    doc: &'a Document,
    body: stext::Body,
}

impl Serialize for StextReport<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = s.serialize_map(Some(5))?;
        map.serialize_entry("schema", "nupdf.stext/v1")?;
        map.serialize_entry("doc_id", &self.facts.doc_id)?;
        self.body.serialize_into(&mut map)?;
        map.serialize_entry("diagnostics", &diagnostics(self.doc))?;
        map.end()
    }
}

/// `objects` reports can be far larger than the file (one entry per declared
/// slot), so they are written while objects load instead of built first.
fn stream_objects(doc: &Document, facts: &FileFacts, pretty: bool) -> ExitCode {
    struct Report<'a> {
        facts: &'a FileFacts,
        doc: &'a Document,
    }
    impl Serialize for Report<'_> {
        fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            use serde::ser::SerializeMap;
            let dump = objects::Dump::new(self.doc);
            let mut map = s.serialize_map(Some(5))?;
            map.serialize_entry("schema", "nupdf.objects/v1")?;
            map.serialize_entry("doc_id", &self.facts.doc_id)?;
            map.serialize_entry("object_count", &dump.count)?;
            map.serialize_entry("objects", &dump)?;
            // Last, so it includes what loading the objects reported.
            map.serialize_entry("diagnostics", &diagnostics(self.doc))?;
            map.end()
        }
    }
    emit(&mut standard_output(), &Report { facts, doc }, pretty);
    ExitCode::SUCCESS
}

/// Writes what `extract` gave in its format.
fn emit_extracted(
    extracted: extract::Extracted,
    facts: &FileFacts,
    doc: &Document,
    pretty: bool,
) -> ExitCode {
    const SCHEMA: &str = "nupdf.extract/v1";
    match extracted {
        extract::Extracted::Json(body) => {
            let report = extract::Report {
                schema: SCHEMA,
                doc_id: &facts.doc_id,
                body: &body,
                diagnostics: diagnostics(doc),
            };
            emit(&mut standard_output(), &report, pretty);
        }
        extract::Extracted::Lines(pages, body) => {
            let mut out = standard_output();
            for page in &pages {
                let line = extract::Line {
                    schema: "nupdf.extract.page/v1",
                    doc_id: &facts.doc_id,
                    page,
                };
                emit(&mut out, &line, false);
            }
            emit(&mut out, &report(SCHEMA, facts, doc, body), false);
        }
        extract::Extracted::Text(text, body) => {
            let mut out = standard_output();
            // A closed pipe is not ours to report.
            let _ = out.write_all(text.as_bytes());
            let _ = out.flush();
            emit(
                &mut standard_error(),
                &report(SCHEMA, facts, doc, body),
                pretty,
            );
        }
    }
    ExitCode::SUCCESS
}

fn to_json(report: InfoReport) -> Result<serde_json::Value, Failure> {
    serde_json::to_value(report).map_err(|e| Failure {
        exit: Exit::Internal,
        code: "internal.serialize",
        message: e.to_string(),
    })
}

/// What `open` learned about the file before parsing it.
#[derive(Debug, Clone)]
struct FileFacts {
    header: Option<header::Header>,
    doc_id: String,
    file_size: usize,
}

/// A file's bytes and what they say about it, read and hashed once.
fn read_file(path: &Path) -> Result<(std::sync::Arc<[u8]>, FileFacts), Failure> {
    let data: std::sync::Arc<[u8]> = std::fs::read(path)
        .map_err(|e| Failure {
            exit: Exit::Input,
            code: "input.unreadable",
            message: format!("cannot read {}: {e}", path.display()),
        })?
        .into();
    let facts = FileFacts {
        header: header::find(&data).ok(),
        doc_id: DocId::of(&data).to_string(),
        file_size: data.len(),
    };
    Ok((data, facts))
}

fn open(
    path: &Path,
    limits: nupdf::Limits,
    password: Option<&str>,
) -> Result<(Document, FileFacts), Failure> {
    let (data, facts) = match mcp::cached_file(path) {
        Some(found) => found,
        None => read_file(path)?,
    };
    let doc = Document::open_with(data, limits).map_err(|e| Failure {
        exit: if e.code.starts_with("limit.") {
            Exit::Limit
        } else {
            Exit::Input
        },
        code: if facts.header.is_none() {
            "input.not_pdf"
        } else {
            e.code
        },
        message: e.message,
    })?;
    if let Some(password) = password {
        let access = doc.authenticate(password).map_err(|e| Failure {
            exit: Exit::Input,
            code: "input.encryption",
            message: nupdf::diag::report::strip(&e).to_string(),
        })?;
        if access == 0 {
            return Err(Failure {
                exit: Exit::Input,
                code: "input.wrong_password",
                message: "the password opens the document neither as user nor as owner".into(),
            });
        }
    }
    Ok((doc, facts))
}

/// Wraps a command's fields in the report envelope every command shares:
/// `schema` and `doc_id` first, `diagnostics` last.
/// A directory to write into, made if it is missing.
fn output_dir(dir: &Path) -> Result<&Path, Failure> {
    std::fs::create_dir_all(dir).map_err(|e| Failure {
        exit: Exit::Usage,
        code: "usage.bad_output",
        message: format!("cannot create {}: {e}", dir.display()),
    })?;
    Ok(dir)
}

fn report(
    schema: &str,
    facts: &FileFacts,
    doc: &Document,
    body: serde_json::Value,
) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    out.insert("schema".into(), schema.into());
    out.insert("doc_id".into(), facts.doc_id.clone().into());
    if let serde_json::Value::Object(fields) = body {
        out.extend(fields);
    }
    out.insert("diagnostics".into(), diagnostics(doc));
    serde_json::Value::Object(out)
}

fn diagnostics(doc: &Document) -> serde_json::Value {
    doc.diagnostics()
        .snapshot()
        .into_iter()
        .map(|d| serde_json::json!({ "code": d.code, "severity": "warning", "message": d.message }))
        .collect()
}

/// `mutool clean`'s stream and syntax flags, counted as it counts them.
struct CleanFlags {
    garbage: u8,
    decompress: u8,
    compress: u8,
    fonts: u8,
    images: u8,
    ascii: u8,
    tight: u8,
    objstms: u8,
    effort: i32,
    /// `None` for a method nupdf does not know.
    encrypt: Option<nupdf::write::Encrypt>,
}

/// `pdf_write_options` as `mutool clean` sets them from its flags.
fn clean_options(f: &CleanFlags) -> Result<nupdf::write::WriteOptions, Failure> {
    if f.compress > 1 {
        return Err(Failure {
            exit: Exit::Usage,
            code: "usage.unsupported",
            message: "Brotli compression (-zz) is not supported".into(),
        });
    }
    let pretty = match f.tight {
        0 => (f.ascii > 0 || f.decompress > 0) && f.compress == 0,
        1 => false,
        _ => true,
    };
    Ok(nupdf::write::WriteOptions {
        pretty,
        ascii: f.ascii > 0,
        garbage: f.garbage.min(4),
        decompress: f.decompress > 0,
        compress: f.compress > 0,
        compress_images: f.images > 0,
        compress_fonts: f.fonts > 0,
        effort: f.effort.clamp(0, 100),
        objstms: f.objstms > 0,
        encrypt: f.encrypt.clone().ok_or_else(|| Failure {
            exit: Exit::Usage,
            code: "usage.bad_value",
            message: "unknown encryption method".into(),
        })?,
        incremental: false,
    })
}

/// `redact`: plan, apply, rewrite, check, write.
/// A preview of each page with targets: the page with its areas shaded,
/// as page-NNNN.png in `dir`.
fn previews(doc: &Document, plan: &redact::Plan, dir: &Path, dpi: f32) -> Vec<serde_json::Value> {
    let ctm = nupdf::geometry::Matrix::scale(dpi / 72.0, dpi / 72.0);
    let mut out = Vec::new();
    for (index, quads) in plan.quads.iter().enumerate() {
        if quads.is_empty() {
            continue;
        }
        let mut entry = serde_json::json!({ "page": index + 1 });
        let rendered = doc.page(index as i32).map_err(|e| e.0).and_then(|page| {
            nupdf::draw::render_page_marked(doc, &page, ctm, 100_000_000, quads, true)
        });
        let path = dir.join(format!("page-{:04}.png", index + 1));
        match rendered.and_then(|r| {
            r.pix
                .to_pixmap(Some(nupdf::colorspace::ColorSpace::Rgb))
                .png()
        }) {
            Ok(png) => match write_file(&path, &png) {
                Ok(()) => entry["file"] = path.display().to_string().into(),
                Err(e) => entry["error"] = e.to_string().into(),
            },
            Err(e) => entry["error"] = e.into(),
        }
        out.push(entry);
    }
    out
}

fn redact_command(
    doc: &Document,
    facts: &FileFacts,
    args: &redact::Args,
    output: Option<&Path>,
    (dry_run, no_verify): (bool, bool),
    password: Option<&str>,
    preview: Option<(&Path, f32)>,
) -> Result<serde_json::Value, Failure> {
    let input = |message: String| Failure {
        exit: Exit::Input,
        code: "input.redaction",
        message: nupdf::diag::report::strip(&message).to_string(),
    };
    if doc.needs_password() && password.is_none() {
        return Err(Failure {
            exit: Exit::Input,
            code: "input.needs_password",
            message: "the empty password does not open the document: pass --password".into(),
        });
    }
    // Every object loaded first, so a repair cannot lose the edits.
    nupdf::write::check_document(doc);
    let plan = redact::plan(doc, args).map_err(input)?;
    let mut fields = serde_json::Map::new();
    fields.insert("applied".into(), (!dry_run).into());
    if dry_run {
        fields.extend(redact::describe(args, &plan, None));
        if let Some((dir, dpi)) = preview {
            fields.insert("previews".into(), previews(doc, &plan, dir, dpi).into());
        }
        fields.insert("output".into(), serde_json::Value::Null);
        fields.insert("verification".into(), serde_json::Value::Null);
        return Ok(report("nupdf.redact/v1", facts, doc, fields.into()));
    }
    let m = redact::matcher(args.targets, args.ignore_case).map_err(input)?;
    let scrubbed = redact::apply(doc, &plan, args.options, &m).map_err(input)?;
    let options = nupdf::write::WriteOptions {
        pretty: false,
        ascii: false,
        garbage: 1,
        decompress: false,
        compress: true,
        compress_images: false,
        compress_fonts: false,
        effort: 0,
        objstms: false,
        encrypt: nupdf::write::Encrypt::Keep,
        incremental: false,
    };
    let bytes = nupdf::write::save(doc, &options).map_err(|e| Failure {
        exit: Exit::Input,
        code: "input.unloadable_object",
        message: nupdf::diag::report::strip(e.message.lines().last().unwrap_or_default())
            .to_string(),
    })?;
    let out_doc = Document::open(bytes.clone()).map_err(|e| Failure {
        exit: Exit::Verify,
        code: "verify.failed",
        message: format!("the output does not open: {}", e.message),
    })?;
    if let Some(p) = password {
        let _ = out_doc.authenticate(p);
    }
    let targets =
        redact::verify_targets(args.targets, args.ignore_case, Some(&plan)).map_err(input)?;
    let v = nupdf::verify::verify(&out_doc, &targets);
    let verified = v.verified();
    fields.extend(redact::describe(args, &plan, Some(&v.hits_after)));
    fields.insert(
        "scrubbed".into(),
        serde_json::json!({
            "metadata": scrubbed.metadata,
            "xmp": scrubbed.xmp,
            "outline": scrubbed.outline,
            "names": scrubbed.names,
            "embedded_files": scrubbed.embedded_files,
            "annotations": scrubbed.annotations,
            "fields": scrubbed.fields,
            "javascript": scrubbed.javascript,
            "structure": scrubbed.structure,
            "layers": scrubbed.layers,
            "page_labels": scrubbed.page_labels,
            "thumbnails": scrubbed.thumbnails,
        }),
    );
    let hash = blake3::hash(&bytes);
    fields.insert(
        "output".into(),
        serde_json::json!({
            "path": output.map(|p| p.display().to_string()),
            "doc_id": DocId::from_hash(&hash).to_string(),
            "file_size": bytes.len(),
        }),
    );
    fields.insert("verification".into(), redact::verification_json(&v));
    fields.insert("verified".into(), verified.into());
    if !verified && !no_verify {
        let classes: Vec<&str> = nupdf::verify::Class::ALL
            .iter()
            .filter(|c| v.findings.iter().any(|f| f.class == **c))
            .map(|c| c.name())
            .collect();
        return Err(Failure {
            exit: Exit::Verify,
            code: "verify.failed",
            message: format!(
                "targets are still in the output ({}); nothing was written",
                classes.join(", ")
            ),
        });
    }
    if let Some(path) = output {
        write_atomically(path, &bytes).map_err(|e| Failure {
            exit: Exit::Input,
            code: "output.unwritable",
            message: format!("cannot write {}: {e}", path.display()),
        })?;
    }
    Ok(report("nupdf.redact/v1", facts, doc, fields.into()))
}

/// `pdf_clean_file`'s page subset: every object loaded first, so a repair
/// cannot lose the edits, then the pages kept, as 0-based indices.
fn keep_pages(doc: &Document, spec: &str) -> Result<Vec<i32>, Failure> {
    let fail = |message: String| Failure {
        exit: Exit::Input,
        code: "input.page_tree",
        message,
    };
    nupdf::write::check_document(doc);
    let count = doc.page_count().map_err(|e| fail(e.0))?;
    let pages = nupdf::rearrange::parse_page_ranges(spec, count, doc);
    nupdf::rearrange::rearrange_pages(doc, &pages)
        .map_err(|e| fail(nupdf::diag::report::strip(&e.0).to_string()))?;
    Ok(pages)
}

struct Clean<'a> {
    options: nupdf::write::WriteOptions,
    output: Option<&'a Path>,
    apply: bool,
    verify: bool,
    password: Option<&'a str>,
    /// The pages kept, when only some are.
    pages: Option<Vec<i32>>,
}

fn clean(doc: &Document, facts: &FileFacts, plan: &Clean) -> Result<serde_json::Value, Failure> {
    let body = serde_json::json!({
        "garbage": plan.options.garbage,
        "streams": {
            "decompress": plan.options.decompress,
            "compress": plan.options.compress,
            "compress_fonts": plan.options.compress_fonts,
            "compress_images": plan.options.compress_images,
            "ascii": plan.options.ascii,
            "effort": plan.options.effort,
        },
        "syntax": if plan.options.pretty { "pretty" } else { "compact" },
        "objstms": plan.options.objstms,
        "pages": plan.pages.as_ref().map(|p| p.iter().map(|i| i + 1).collect::<Vec<_>>()),
        "encryption": encryption_json(&plan.options.encrypt),
    });
    write_document(doc, facts, plan, "nupdf.clean/v1", body)
}

fn encryption_json(encrypt: &nupdf::write::Encrypt) -> serde_json::Value {
    match encrypt {
        nupdf::write::Encrypt::Keep => serde_json::json!("keep"),
        nupdf::write::Encrypt::Remove => serde_json::json!("none"),
        nupdf::write::Encrypt::New {
            method,
            permissions,
            ..
        } => serde_json::json!({
            "method": method.name(),
            "permissions": permissions,
        }),
    }
}

/// Saves `doc` as `plan` says, reads it back unless told not to, writes it
/// unless a dry run, and reports `body` with the output and verification.
fn write_document(
    doc: &Document,
    facts: &FileFacts,
    plan: &Clean,
    schema: &str,
    body: serde_json::Value,
) -> Result<serde_json::Value, Failure> {
    let (output, verification) = save_checked(doc, plan)?;
    let mut fields = serde_json::Map::new();
    fields.insert("applied".into(), plan.apply.into());
    if let serde_json::Value::Object(b) = body {
        fields.extend(b);
    }
    fields.insert("output".into(), output);
    fields.insert("verification".into(), verification);
    Ok(report(
        schema,
        facts,
        doc,
        serde_json::Value::Object(fields),
    ))
}

/// The save, check and write of `write_document`: the output's report and
/// the verification's.
fn save_checked(
    doc: &Document,
    plan: &Clean,
) -> Result<(serde_json::Value, serde_json::Value), Failure> {
    // pdf_clean_file: "cannot authenticate password".
    if doc.needs_password() && plan.password.is_none() {
        return Err(Failure {
            exit: Exit::Input,
            code: "input.needs_password",
            message: "the empty password does not open the document: pass --password".into(),
        });
    }
    let bytes = nupdf::write::save(doc, &plan.options).map_err(|e| {
        let message =
            nupdf::diag::report::strip(e.message.lines().last().unwrap_or_default()).to_string();
        // MuPDF's refusals of an incremental write are argument errors.
        if plan.options.incremental && message.starts_with("Can't do incremental") {
            Failure {
                exit: Exit::Usage,
                code: "usage.incremental",
                message,
            }
        } else {
            Failure {
                exit: Exit::Input,
                code: "input.unloadable_object",
                message,
            }
        }
    })?;
    let verification = plan.verify.then(|| {
        let v = nupdf::write::verify(doc, &bytes, plan.password, &plan.options);
        if v.verified() {
            Ok(v)
        } else {
            Err(Failure {
                exit: Exit::Verify,
                code: "verify.failed",
                message: verify_failure(&v),
            })
        }
    });
    let verification = verification.transpose()?;
    if plan.apply
        && let Some(path) = plan.output
    {
        write_atomically(path, &bytes).map_err(|e| Failure {
            exit: Exit::Input,
            code: "output.unwritable",
            message: format!("cannot write {}: {e}", path.display()),
        })?;
    }
    let hash = blake3::hash(&bytes);
    let mut output = serde_json::json!({
        "path": plan.output.map(|p| p.display().to_string()),
        "doc_id": DocId::from_hash(&hash).to_string(),
        "len": bytes.len(),
        "blake3": hash.to_hex().to_string(),
    });
    if let Some(signatures) = signatures_kept(doc, &bytes, plan) {
        output["signatures"] = signatures;
    }
    let verification = verification.map_or(serde_json::Value::Null, |v| {
        serde_json::json!({
            "verified": true,
            "objects": v.objects,
        })
    });
    Ok((output, verification))
}

/// For a signed input: each signature's digest check in the input and in
/// the output, by field name. A signature that held and no longer does is
/// a warning, since only an incremental save keeps the signed bytes.
fn signatures_kept(doc: &Document, bytes: &[u8], plan: &Clean) -> Option<serde_json::Value> {
    let before = nupdf::signature::summary(doc);
    if before.is_empty() {
        return None;
    }
    let after = Document::open(bytes.to_vec())
        .map(|out| {
            if let Some(pw) = plan.password {
                let _ = out.authenticate(pw);
            }
            nupdf::signature::summary(&out)
        })
        .unwrap_or_default();
    let list: Vec<serde_json::Value> = before
        .iter()
        .map(|(name, was)| {
            let now = after
                .iter()
                .find(|(n, _)| n == name)
                .map_or("Not signed.", |(_, s)| s.as_str());
            if was == nupdf::signature::OK && now != nupdf::signature::OK {
                doc.diagnostics().warn(
                    "signature.invalidated",
                    format!(
                        "the output breaks signature {name:?} ({now}); --incremental keeps the signed bytes"
                    ),
                );
            }
            serde_json::json!({ "name": name, "before": was, "after": now })
        })
        .collect();
    Some(list.into())
}

/// Why the written document failed its self-check.
fn verify_failure(v: &nupdf::write::Verification) -> String {
    if !v.reopened {
        return "the output does not open".into();
    }
    if v.repaired {
        return "the output needs a repair to open".into();
    }
    const SHOWN: usize = 10;
    let mut nums: Vec<String> = v
        .mismatched
        .iter()
        .take(SHOWN)
        .map(|&n| match n {
            0 => "the trailer".to_string(),
            n => n.to_string(),
        })
        .collect();
    if v.mismatched.len() > SHOWN {
        nums.push(format!("and {} more", v.mismatched.len() - SHOWN));
    }
    format!(
        "{} of {} objects read back differently from the input: {}",
        v.mismatched.len(),
        v.objects,
        nums.join(", ")
    )
}

/// Writes a file a command makes, unless the call was stopped: what it
/// made since comes from pages cut short.
pub(crate) fn write_file(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if nupdf::interrupt::interrupted() {
        return Err(std::io::Error::other(nupdf::interrupt::MESSAGE));
    }
    std::fs::write(path, data)
}

/// Writes `data` next to `path` and renames it into place, so a reader
/// never sees a partial file.
fn write_atomically(path: &Path, data: &[u8]) -> std::io::Result<()> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty());
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp_name = format!(".{name}.nupdf-{}-{n}.tmp", std::process::id());
    let tmp = dir.map_or_else(|| PathBuf::from(&tmp_name), |d| d.join(&tmp_name));
    let written = write_file(&tmp, data).and_then(|()| {
        // A call stopped meanwhile made its output from damaged pages.
        if nupdf::interrupt::interrupted() {
            return Err(std::io::Error::other(nupdf::interrupt::MESSAGE));
        }
        std::fs::rename(&tmp, path)
    });
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

fn show(
    (doc, facts): (Document, FileFacts),
    target: &str,
    data: bool,
    max_bytes: usize,
) -> Result<serde_json::Value, Failure> {
    let mut body = serde_json::Map::new();
    if target.trim() == "trailer" {
        body.insert("object".into(), "trailer".into());
        body.insert(
            "value".into(),
            objects::encode(&nupdf::Object::Dict(doc.trailer().into())),
        );
        return Ok(report("nupdf.show/v1", &facts, &doc, body.into()));
    }
    let num = target
        .split_whitespace()
        .next()
        .and_then(|n| n.parse::<i32>().ok())
        .ok_or_else(|| Failure {
            exit: Exit::Usage,
            code: "usage.bad_target",
            message: format!("expected an object number, `N G R` or `trailer`, got {target:?}"),
        })?;
    if num <= 0 || num as usize >= doc.xref_len() {
        return Err(Failure {
            exit: Exit::Input,
            code: "input.no_such_object",
            message: format!("object {num} is outside the xref (1..{})", doc.xref_len()),
        });
    }
    body.insert("object".into(), format!("{num} 0 R").into());
    let value = doc.object(num);
    body.insert(
        "value".into(),
        value
            .as_ref()
            .map_or(serde_json::Value::Null, objects::encode),
    );
    if let Some(nupdf::Object::Stream(stream)) = &value {
        let mut info = serde_json::Map::new();
        match doc.raw_stream_data(stream) {
            Ok(raw) => info.insert("raw_len".into(), raw.len().into()),
            Err(e) => info.insert("raw_error".into(), e.into()),
        };
        match doc.stream_data(stream) {
            Ok(decoded) => {
                info.insert("decoded_len".into(), decoded.len().into());
                if data {
                    let shown = &decoded[..decoded.len().min(max_bytes)];
                    info.insert(
                        "data".into(),
                        String::from_utf8_lossy(shown).into_owned().into(),
                    );
                    info.insert("truncated".into(), (decoded.len() > max_bytes).into());
                }
            }
            Err(e) => {
                info.insert("decode_error".into(), e.to_string().into());
            }
        }
        body.insert("stream".into(), info.into());
    }
    Ok(report("nupdf.show/v1", &facts, &doc, body.into()))
}

/// MuPDF's permission names (`Document.hasPermission`).
const PERMISSIONS: [&str; 8] = [
    "print",
    "edit",
    "copy",
    "annotate",
    "form",
    "accessibility",
    "assemble",
    "print-hq",
];

fn info((doc, facts): (Document, FileFacts), authenticated: bool) -> Result<InfoReport, Failure> {
    let FileFacts {
        header,
        doc_id,
        file_size,
    } = facts;

    // Same order as oracle/js/info.js: a repair triggered by one call is
    // visible to the next.
    let pdf_version = doc.version().to_string();
    let encryption = doc.encryption();
    let needs_password = doc.needs_password();
    let (page_count, page_count_error) = if needs_password && !authenticated {
        (None, None)
    } else {
        match doc.page_count() {
            Ok(n) => (Some(n), None),
            Err(e) => (None, Some(e.0)),
        }
    };
    let object_count = doc.xref_len();
    let repaired = doc.repaired();
    let revisions = doc.revisions();
    let metadata = [
        "Title",
        "Author",
        "Subject",
        "Keywords",
        "Creator",
        "Producer",
        "CreationDate",
        "ModDate",
    ]
    .into_iter()
    .filter_map(|key| doc.metadata(key).map(|v| (key.to_string(), v.into())))
    .collect();

    let diagnostics = doc
        .diagnostics()
        .snapshot()
        .into_iter()
        .map(|d| Diagnostic {
            code: d.code,
            severity: "warning",
            message: d.message,
        })
        .collect();

    Ok(InfoReport {
        schema: "nupdf.info/v1",
        doc_id,
        file_size,
        kind: match header.map(|h| h.kind) {
            Some(Kind::Fdf) => "fdf",
            _ => "pdf",
        },
        pdf_version,
        header_version: doc.header_version().to_string(),
        header_offset: header.map(|h| h.offset),
        encryption,
        needs_password,
        permissions: PERMISSIONS
            .iter()
            .map(|p| (p.to_string(), doc.has_permission(p).into()))
            .collect(),
        page_count,
        page_count_error,
        object_count,
        revisions,
        revision_offsets: doc.revision_offsets(),
        linearized: doc.linearized(),
        repaired,
        encrypted: doc.is_encrypted(),
        metadata,
        diagnostics,
    })
}
