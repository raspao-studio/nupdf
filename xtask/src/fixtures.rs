// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! The smoke corpus: small handcrafted PDFs, one syntax feature or failure
//! mode each. Generated deterministically so the bytes are reviewable and
//! license-free. `cargo xtask fixtures` writes them; `--check` verifies them.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result, bail};

pub struct Fixture {
    pub name: &'static str,
    pub about: &'static str,
    /// MuPDF is expected to warn and rebuild the xref for this file.
    pub needs_repair: bool,
    pub bytes: Vec<u8>,
}

pub fn all() -> Vec<Fixture> {
    vec![
        Fixture {
            name: "minimal.pdf",
            about: "One page, classic xref table, Helvetica text",
            needs_repair: false,
            bytes: hello(Pdf::new("1.7"), "Hello, nupdf", false).finish_with_xref(""),
        },
        Fixture {
            name: "version-2.0.pdf",
            about: "PDF 2.0 header",
            needs_repair: false,
            bytes: hello(Pdf::new("2.0"), "PDF 2.0", false).finish_with_xref(""),
        },
        Fixture {
            name: "catalog-version.pdf",
            about: "Header says 1.4, /Root/Version overrides it to 1.7",
            needs_repair: false,
            bytes: catalog_version(),
        },
        Fixture {
            name: "multipage-inherit.pdf",
            about: "Three pages inheriting MediaBox and Resources; one rotated, one with its own box",
            needs_repair: false,
            bytes: multipage_inherit(),
        },
        Fixture {
            name: "flate-content.pdf",
            about: "Content stream compressed with FlateDecode",
            needs_repair: false,
            bytes: hello(Pdf::new("1.5"), "Compressed", true).finish_with_xref(""),
        },
        Fixture {
            name: "xref-stream.pdf",
            about: "Catalog, page tree and page inside an object stream, indexed by an xref stream",
            needs_repair: false,
            bytes: xref_stream(),
        },
        Fixture {
            name: "incremental-update.pdf",
            about: "Two revisions: the update replaces the page content and adds /Info via /Prev",
            needs_repair: false,
            bytes: incremental_update(),
        },
        Fixture {
            name: "cr-line-endings.pdf",
            about: "Every line ends in a bare CR",
            needs_repair: false,
            bytes: hello(Pdf::with_eol("1.4", "\r"), "CR only", false).finish_with_xref(""),
        },
        Fixture {
            name: "crlf-line-endings.pdf",
            about: "Every line ends in CRLF",
            needs_repair: false,
            bytes: hello(Pdf::with_eol("1.4", "\r\n"), "CRLF", false).finish_with_xref(""),
        },
        Fixture {
            name: "garbage-prefix.pdf",
            about: "64 bytes of junk before %PDF-; offsets are relative to the header",
            needs_repair: false,
            bytes: garbage_prefix(),
        },
        Fixture {
            name: "broken-xref-offsets.pdf",
            about: "Every xref offset is 7 bytes too far",
            needs_repair: true,
            bytes: hello(Pdf::new("1.4"), "Skewed", false)
                .skew_offsets(7)
                .finish_with_xref(""),
        },
        Fixture {
            name: "no-xref.pdf",
            about: "Objects and trailer, but no xref table and no startxref",
            needs_repair: true,
            bytes: no_xref(),
        },
        Fixture {
            name: "truncated.pdf",
            about: "minimal.pdf cut off inside the page object",
            needs_repair: true,
            bytes: truncated(),
        },
    ]
    .into_iter()
    .chain(edge_cases())
    .chain(encrypted_fixtures())
    .chain(filter_fixtures())
    .chain(image_fixtures())
    .chain(writer_fixtures())
    .chain(garbage_fixtures())
    .chain(stream_fixtures())
    .chain(objstm_fixtures())
    .chain(rearrange_fixtures())
    .collect()
}

/// Cases for `clean -g` to `-gggg`.
fn garbage_fixtures() -> Vec<Fixture> {
    let fx = |name, about, bytes| Fixture {
        name,
        about,
        needs_repair: false,
        bytes,
    };
    let page = |contents: &str, resources: &str| {
        format!(
            "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources {resources}/Contents {contents}>>"
        )
    };

    // The page reaches a missing object, a free one, a null one, and one
    // whose value is a reference to a free object; the catalog reaches that
    // last one too, but after the page. The content stream's /Length is
    // indirect, and objects 6 to 8 are reached from nothing.
    let mut pdf = Pdf::new("1.4");
    pdf.obj(1, "<</Type/Catalog/Pages 2 0 R/Later 11 0 R>>");
    pdf.obj(2, "<</Type/Pages/Kids[3 0 R]/Count 1>>");
    pdf.obj(
        3,
        "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources<</Font<</F1 5 0 R>>>>\
         /Contents 4 0 R/Missing 99 0 R/Free 9 0 R/Null 10 0 R/Chain 11 0 R>>",
    );
    let content = text_content("Reachable");
    pdf.stream_as_is(4, "<</Length 13 0 R>>", &content);
    pdf.obj(5, FONT);
    pdf.obj(6, "<</Unused 7 0 R>>");
    pdf.stream(7, "", b"unreachable");
    pdf.obj(8, "<</Also 6 0 R>>");
    pdf.obj(10, "null");
    pdf.obj(11, "12 0 R");
    pdf.obj(13, &content.len().to_string());
    let reach = pdf.finish_with_xref("");

    // Two pages alike but for their resources and contents. The contents
    // have the same data (merged by -gggg only); the resources differ only
    // in their font, and the fonts are alike, so the resources merge once
    // the fonts have. Two dictionaries equal in another order are not
    // merged, and neither are the pages.
    let mut pdf = Pdf::new("1.4");
    pdf.obj(1, "<</Type/Catalog/Pages 2 0 R/Orders[11 0 R 12 0 R]>>");
    pdf.obj(2, "<</Type/Pages/Kids[3 0 R 4 0 R]/Count 2>>");
    pdf.obj(3, &page("5 0 R", "7 0 R"));
    pdf.obj(4, &page("6 0 R", "8 0 R"));
    let content = text_content("Same");
    pdf.stream(5, "", &content);
    pdf.stream(6, "", &content);
    pdf.obj(7, "<</Font<</F1 9 0 R>>>>");
    pdf.obj(8, "<</Font<</F1 10 0 R>>>>");
    pdf.obj(9, FONT);
    pdf.obj(10, FONT);
    pdf.obj(11, "<</A 1/B 2>>");
    pdf.obj(12, "<</B 2/A 1>>");
    let duplicates = pdf.finish_with_xref("");

    // Renumbering puts a changed reference back by key. Object 7 has 101
    // entries, so the put sorts it. Object 9 is the same with its reference
    // first: the sort moves it last, where it is renumbered a second time.
    // Object 8 is sorted, and its last key is a name MuPDF interns, so the
    // put appends a second /ca. Object 6 is unreachable, so everything
    // after it moves down.
    let mut pdf = hello(Pdf::new("1.4"), "Renumber", false);
    pdf.obj(
        1,
        "<</Type/Catalog/Pages 2 0 R/Unsorted 7 0 R/Sorted 8 0 R/First 9 0 R>>",
    );
    pdf.obj(6, "<</Unused true>>");
    let ints = |range: &mut dyn Iterator<Item = u32>| {
        range.map(|i| format!("/K{i:03} {i}")).collect::<String>()
    };
    pdf.obj(7, &format!("<<{}/A 10 0 R>>", ints(&mut (0..100).rev())));
    pdf.obj(8, &format!("<<{}/ca 11 0 R>>", ints(&mut (0..101))));
    pdf.obj(9, &format!("<</Z 12 0 R{}>>", ints(&mut (0..100))));
    pdf.obj(10, "(A target)");
    pdf.obj(11, "(ca target)");
    pdf.obj(12, "(Z target)");
    let renumber = pdf.finish_with_xref("");

    vec![
        fx(
            "gc-reach.pdf",
            "Garbage: unreachable objects, an indirect /Length, references to missing, free and null objects, and to a reference to nothing",
            reach,
        ),
        fx(
            "gc-duplicates.pdf",
            "Garbage: duplicate fonts, streams and pages, objects alike once others merge, and dictionaries alike in another order",
            duplicates,
        ),
        fx(
            "gc-renumber.pdf",
            "Garbage: references renumbered in a 101-entry dictionary and in a sorted one whose last key is interned",
            renumber,
        ),
        fx(
            "gc-encrypted.pdf",
            "Garbage: an RC4 document whose strings and streams are encrypted again under new numbers",
            encrypted_with_garbage(),
        ),
    ]
}

/// Cases for `clean --pages`.
fn rearrange_fixtures() -> Vec<Fixture> {
    // Four pages under two intermediate nodes that pass down MediaBox,
    // Rotate and Resources. Outlines: one item per page, the third with a
    // child, the fourth by name. Named destinations with a string key and a
    // name key. Page 1 links to each page: by GoTo with an array, by a
    // string destination, by a name /Dest. A text field has a widget on
    // pages 1 and 4. The catalog has generation 1 and keys the new one
    // drops.
    let mut pdf = Pdf::new("1.7");
    pdf.obj(
        1,
        "<</Type/Catalog/Pages 2 0 R/Outlines 20 0 R/Names<</Dests 30 0 R>>/AcroForm<</Fields[40 0 R]>>/Lang(en)/PageMode/UseOutlines>>",
    );
    pdf.obj(2, "<</Type/Pages/Kids[3 0 R 4 0 R]/Count 4>>");
    pdf.obj(
        3,
        "<</Type/Pages/Parent 2 0 R/Kids[5 0 R 6 0 R]/Count 2/MediaBox[0 0 612 792]/Resources<</Font<</F1 9 0 R>>>>>>",
    );
    pdf.obj(
        4,
        "<</Type/Pages/Parent 2 0 R/Kids[7 0 R 8 0 R]/Count 2/MediaBox[0 0 300 300]/Rotate 90/Resources<</Font<</F1 9 0 R>>>>>>",
    );
    let page = |parent: u32, content: u32, annots: &str| {
        format!("<</Type/Page/Parent {parent} 0 R/Contents {content} 0 R{annots}>>")
    };
    pdf.obj(
        5,
        &page(3, 10, "/Annots[50 0 R 51 0 R 52 0 R 53 0 R 41 0 R]"),
    );
    pdf.obj(6, &page(3, 11, ""));
    pdf.obj(7, &page(4, 12, ""));
    pdf.obj(8, &page(4, 13, "/Annots 54 0 R"));
    pdf.obj(9, FONT);
    for (num, text) in [(10, "One"), (11, "Two"), (12, "Three"), (13, "Four")] {
        pdf.stream(num, "", &text_content(text));
    }
    pdf.obj(20, "<</Type/Outlines/First 21 0 R/Last 24 0 R/Count 4>>");
    pdf.obj(
        21,
        "<</Title(One)/Parent 20 0 R/Next 22 0 R/Dest[5 0 R/Fit]>>",
    );
    pdf.obj(
        22,
        "<</Title(Two)/Parent 20 0 R/Prev 21 0 R/Next 23 0 R/Dest[6 0 R/Fit]>>",
    );
    pdf.obj(
        23,
        "<</Title(Three)/Parent 20 0 R/Prev 22 0 R/Next 24 0 R/A<</S/GoTo/D[7 0 R/Fit]>>/First 25 0 R/Last 25 0 R/Count 1>>",
    );
    pdf.obj(24, "<</Title(Four)/Parent 20 0 R/Prev 23 0 R/Dest(four)>>");
    pdf.obj(
        25,
        "<</Title(Three, again)/Parent 23 0 R/Dest[7 0 R/XYZ 0 0 0]>>",
    );
    pdf.obj(30, "<</Names[(four) 31 0 R /two 32 0 R]>>");
    pdf.obj(31, "<</D[8 0 R/Fit]>>");
    pdf.obj(32, "[6 0 R/Fit]");
    pdf.obj(40, "<</FT/Tx/T(field)/Kids[41 0 R 42 0 R]>>");
    pdf.obj(41, "<</Type/Annot/Subtype/Widget/Parent 40 0 R/P 5 0 R/Rect[0 0 10 10]/A<</S/GoTo/D[8 0 R/Fit]>>>>");
    pdf.obj(
        42,
        "<</Type/Annot/Subtype/Widget/Parent 40 0 R/P 8 0 R/Rect[0 0 10 10]>>",
    );
    let link = |dest: &str| format!("<</Type/Annot/Subtype/Link/Rect[0 0 10 10]{dest}>>");
    pdf.obj(50, &link("/A<</S/GoTo/D[6 0 R/Fit]>>"));
    pdf.obj(51, &link("/Dest(four)"));
    pdf.obj(52, &link("/Dest/two"));
    pdf.obj(53, &link("/Dest[7 0 R/Fit]"));
    pdf.obj(54, "[42 0 R]");
    let mut rearrange = pdf.finish_with_xref("");
    let header = b"1 0 obj";
    if let Some(i) = rearrange.windows(header.len()).position(|w| w == header) {
        rearrange[i + 2] = b'1';
        // The first xref entry after the free head is object 1's.
        let entry = format!("{:010} 00000 n", i);
        if let Some(j) = rearrange
            .windows(entry.len())
            .position(|w| w == entry.as_bytes())
        {
            rearrange[j + 15] = b'1';
        }
    }

    vec![Fixture {
        name: "pages-rearrange.pdf",
        about: "For --pages: a nested page tree passing attributes down, outlines, named destinations by string and name, links and a field across pages, a catalog of generation 1",
        needs_repair: false,
        bytes: rearrange,
    }]
}

/// Cases for `clean -Z`.
fn objstm_fixtures() -> Vec<Fixture> {
    let fx = |name, about, bytes| Fixture {
        name,
        about,
        needs_repair: false,
        bytes,
    };

    // What gathering leaves out: the catalog and /Info, a bare integer, a
    // bare reference, an object of generation 1, streams. The rest is
    // packed in number order with nothing between objects, so `true`,
    // `null`, a real and a name run together, each read back from its own
    // range. 300 small dictionaries fill a second object stream. The
    // catalog's /Version 1.4 becomes 1.5.
    let mut pdf = Pdf::new("1.4");
    pdf.obj(
        1,
        "<</Type/Catalog/Pages 2 0 R/Version/1.4/Extra 9 0 R/Many 315 0 R>>",
    );
    pdf.obj(2, "<</Type/Pages/Kids[3 0 R]/Count 1>>");
    pdf.obj(
        3,
        "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources<</Font<</F1 5 0 R>>>>/Contents 4 0 R>>",
    );
    pdf.stream(4, "", &text_content("Packed"));
    pdf.obj(5, FONT);
    pdf.obj(6, "<</Title(Packed)>>");
    pdf.obj(7, "42");
    pdf.obj(8, "5 0 R");
    pdf.obj(9, "[7 0 R 8 0 R 10 0 R 11 0 R 12 0 R 13 0 R 14 1 R]");
    pdf.obj(10, "true");
    pdf.obj(11, "null");
    pdf.obj(12, "0.5");
    pdf.obj(13, "/Name");
    let at = pdf.out.len();
    pdf.obj(14, "<</Generation 1>>");
    for i in 15..315 {
        pdf.obj(i, &format!("<</I {i}>>"));
    }
    let refs: String = (15..315).map(|i| format!("{i} 0 R ")).collect();
    pdf.obj(315, &format!("[{refs}]"));
    let mut pack = pdf.finish_with_xref("/Info 6 0 R");
    // Object 14 has generation 1, in its header and its xref entry.
    let header = b"14 0 obj";
    if let Some(i) = pack[at..].windows(header.len()).position(|w| w == header) {
        pack[at + i + 3] = b'1';
    }
    let entry = format!("{at:010} 00000 n");
    if let Some(i) = pack
        .windows(entry.len())
        .position(|w| w == entry.as_bytes())
    {
        pack[i + 15] = b'1';
    }

    // An object number the only xref section skips: gathering loads it,
    // which fails MuPDF's save.
    let mut pdf = hello(Pdf::new("1.5"), "Gap", false);
    pdf.obj(7, "<</After (the gap)>>");
    let start = pdf.xref(&[1, 2, 3, 4, 5, 7], true);
    pdf.trailer("/Root 1 0 R", start);
    let gap = pdf.out;

    // The same gap made by an update whose numbers start past the first
    // section's size: MuPDF's lookup then finds no entry, and the save
    // goes on.
    let mut pdf = hello(Pdf::new("1.5"), "Update", false);
    let nums: Vec<u32> = (1..pdf.size()).collect();
    let first = pdf.xref(&nums, true);
    pdf.trailer("/Root 1 0 R", first);
    pdf.obj(8, "<</Added (in the update)>>");
    let second = pdf.xref(&[8], true);
    pdf.trailer(&format!("/Root 1 0 R/Prev {first}"), second);
    let gap_update = pdf.out;

    // An empty stream, which MuPDF writes under AES with the /Length of an
    // IV and a block but no data.
    let mut pdf = Pdf::new("1.5");
    pdf.obj(1, "<</Type/Catalog/Pages 2 0 R>>");
    pdf.obj(2, "<</Type/Pages/Kids[3 0 R]/Count 1>>");
    pdf.obj(
        3,
        "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Contents[4 0 R 5 0 R]>>",
    );
    pdf.stream(4, "", b"0 0 1 rg 0 0 100 100 re f");
    pdf.stream(5, "", b"");
    let empty_stream = pdf.finish_with_xref("");

    vec![
        fx(
            "write-empty-stream.pdf",
            "Writer: an empty content stream, which MuPDF's AES encryption writes without data",
            empty_stream,
        ),
        fx(
            "objstm-pack.pdf",
            "For -Z: objects gathering leaves out, objects packed with nothing between them, two object streams' worth, and a catalog /Version below 1.5",
            pack,
        ),
        fx(
            "objstm-gap.pdf",
            "For -Z: an object number the only xref section skips, which fails MuPDF's save",
            gap,
        ),
        fx(
            "objstm-gap-update.pdf",
            "For -Z: undefined object numbers past the first section's size, which MuPDF's lookup skips",
            gap_update,
        ),
    ]
}

/// 1-bit rows packed MSB first, 0 for black as DeviceGray reads them; the
/// bits that pad a row to a byte are black, which a fax encoder drops.
fn pack_rows(rows: &[Vec<bool>]) -> Vec<u8> {
    let mut out = Vec::new();
    for row in rows {
        for chunk in row.chunks(8) {
            let mut byte = 0u8;
            for (i, &black) in chunk.iter().enumerate() {
                if !black {
                    byte |= 0x80 >> i;
                }
            }
            out.push(byte);
        }
    }
    out
}

/// Cases for `clean -d`, `-z`, `-f`, `-i` and `-a`.
fn stream_fixtures() -> Vec<Fixture> {
    use crate::encode;
    let fx = |name, about, bytes| Fixture {
        name,
        about,
        needs_repair: false,
        bytes,
    };
    let text = sample_data();

    // Streams with no filter, of every kind the options treat apart.
    let mut pdf = Pdf::new("1.6");
    pdf.obj(
        1,
        "<</Type/Catalog/Pages 2 0 R/Metadata 10 0 R/Extra[13 0 R 16 0 R]>>",
    );
    pdf.obj(2, "<</Type/Pages/Kids[3 0 R]/Count 1>>");
    pdf.obj(
        3,
        "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources<</Font<</F1 5 0 R>>\
         /XObject<</Im1 6 0 R/Im2 7 0 R/Im3 8 0 R/Im4 9 0 R>>>>/Contents 4 0 R>>",
    );
    pdf.stream(
        4,
        "",
        b"q 77 0 0 40 72 600 cm /Im1 Do Q q 300 0 0 3 72 500 cm /Im2 Do Q\n\
          q 61 0 0 37 72 400 cm /Im3 Do Q BT /F1 12 Tf 72 300 Td (Raw) Tj ET\n",
    );
    pdf.obj(
        5,
        "<</Type/Font/Subtype/Type1/BaseFont/Example/FontDescriptor 11 0 R>>",
    );
    let gray = |w: usize, h: usize, extra: &str| {
        format!(
            "/Type/XObject/Subtype/Image/Width {w}/Height {h}/ColorSpace/DeviceGray/BitsPerComponent 1{extra}"
        )
    };
    pdf.stream(6, &gray(77, 40, ""), &pack_rows(&bilevel(77, 40)));
    // A run longer than 2560 needs two make-up codes.
    let wide: Vec<Vec<bool>> = (0..3)
        .map(|y| (0..3000).map(|x| y == 1 || x > 2900).collect())
        .collect();
    pdf.stream(
        7,
        "/Type/XObject/Subtype/Image/Width 3000/Height 3/ImageMask true",
        &pack_rows(&wide),
    );
    let pixels: Vec<u8> = (0..37)
        .flat_map(|y| (0..61).map(move |x| image_sample(0, x, y)))
        .collect();
    pdf.stream(
        8,
        "/Type/XObject/Subtype/Image/Width 61/Height 37/ColorSpace/DeviceGray/BitsPerComponent 8",
        &pixels,
    );
    // One byte short of its rows: deflated, not fax encoded.
    let mut short = pack_rows(&bilevel(32, 8));
    short.pop();
    pdf.stream(9, &gray(32, 8, ""), &short);
    pdf.stream(
        10,
        "/Type/Metadata/Subtype/XML",
        b"<?xpacket begin='' id='W5M0MpCehiHzreSzNTczkc9d'?><x:xmpmeta xmlns:x='adobe:ns:meta/'/><?xpacket end='w'?>",
    );
    pdf.obj(
        11,
        "<</Type/FontDescriptor/FontName/Example/Flags 32/FontBBox[0 0 1000 1000]/ItalicAngle 0\
         /Ascent 800/Descent -200/CapHeight 700/StemV 80/FontFile 12 0 R>>",
    );
    let font: Vec<u8> = (0..300u32).map(|i| (i * 37 % 256) as u8).collect();
    pdf.stream(12, "/Length1 300/Length2 0/Length3 0", &font);
    // /Subtype by reference fails MuPDF's pointer comparison, so this is
    // deflated; /BitsPerComponent by reference still counts.
    pdf.stream(
        13,
        "/Type/XObject/Subtype 14 0 R/Width 16/Height 4/ColorSpace/DeviceGray/BitsPerComponent 1",
        &pack_rows(&bilevel(16, 4)),
    );
    pdf.obj(14, "/Image");
    pdf.obj(15, "1");
    pdf.stream(
        16,
        "/Type/XObject/Subtype/Image/Width 24/Height 4/ColorSpace/DeviceGray/BitsPerComponent 15 0 R",
        &pack_rows(&bilevel(24, 4)),
    );
    let raw = pdf.finish_with_xref("");

    // ASCIIHexDecode as `-z` takes it off, and filter arrays as `-a` and
    // `-z` edit them: in place, so a shared array changes for every stream
    // that uses it.
    let hex = encode::ascii_hex(&text);
    let hex_flate = encode::ascii_hex(&encode::flate(&text));
    let flate = encode::flate(&text);
    let mut pdf = Pdf::new("1.6");
    pdf.obj(1, "<</Type/Catalog/Pages 2 0 R/Extra[6 0 R 7 0 R 8 0 R 9 0 R 10 0 R 11 0 R 12 0 R 13 0 R 14 0 R 15 0 R 16 0 R]>>");
    pdf.obj(2, "<</Type/Pages/Kids[3 0 R]/Count 1>>");
    pdf.obj(
        3,
        "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources<</Font<</F1 5 0 R>>>>/Contents 4 0 R>>",
    );
    pdf.stream(4, "", &text_content("Hex"));
    pdf.obj(5, FONT);
    pdf.stream(6, "/Filter/ASCIIHexDecode", &hex);
    pdf.stream(
        7,
        "/Filter[/ASCIIHexDecode/FlateDecode]/DecodeParms[null<</Predictor 1>>]",
        &hex_flate,
    );
    // An array of one filter becomes the filter; /DecodeParms, not an
    // array, becomes null.
    pdf.stream(
        8,
        "/Filter[/FlateDecode]/DecodeParms<</Predictor 1>>",
        &flate,
    );
    pdf.stream(9, "/Filter 20 0 R/DecodeParms 21 0 R", &hex_flate);
    pdf.stream(10, "/Filter 20 0 R/DecodeParms 21 0 R", &hex_flate);
    pdf.stream(11, "/Filter 22 0 R", &flate);
    pdf.stream(12, "/Filter 22 0 R", &flate);
    // The abbreviation is not what MuPDF compares with.
    pdf.stream(13, "/Filter/AHx", &hex);
    let mut bad = encode::ascii_hex(b"Hello, world");
    bad[10] = b'z';
    pdf.stream(14, "/Filter/ASCIIHexDecode", &bad);
    pdf.stream(15, "/Filter[/ASCIIHexDecode]", &hex);
    let binary: Vec<u8> = (0..=255u8).collect();
    pdf.stream(
        16,
        "/Filter[/FlateDecode]/DecodeParms[<</Predictor 1>>]",
        &encode::flate(&binary),
    );
    pdf.obj(20, "[/ASCIIHexDecode/FlateDecode]");
    pdf.obj(21, "[null null]");
    pdf.obj(22, "[/FlateDecode]");
    let hexed = pdf.finish_with_xref("");

    // Taking ASCIIHexDecode off an array whose /DecodeParms is empty fails
    // MuPDF's whole save.
    let mut pdf = hello(Pdf::new("1.6"), "Empty", false);
    pdf.stream(
        6,
        "/Filter[/ASCIIHexDecode/FlateDecode]/DecodeParms[]",
        &hex_flate,
    );
    let empty_parms = pdf.finish_with_xref("");

    // 101 keys and /Filter, with no /Length: parsing sorts the dictionary,
    // so /Filter is its last key, and MuPDF's lookup of an interned name
    // misses it. The stream reads as unfiltered, and -z appends a second
    // /Filter.
    let keys: String = (0..101).map(|i| format!("/A{i:03} {i}")).collect();
    let mut pdf = hello(Pdf::new("1.6"), "Sorted", false);
    pdf.stream_as_is(6, &format!("<<{keys}/Filter/FlateDecode>>"), &flate);
    let sorted = pdf.finish_with_xref("");

    vec![
        fx(
            "streams-raw.pdf",
            "Streams with no filter: 1-bit images and a mask (fax encoded), one a byte short, one with /Subtype by reference, an 8-bit image, a font file, XML metadata",
            raw,
        ),
        fx(
            "streams-hex.pdf",
            "ASCIIHexDecode alone, first in an array, abbreviated and damaged; filter arrays of one; filter arrays shared by reference, which -a and -z edit in place",
            hexed,
        ),
        fx(
            "streams-hex-empty-parms.pdf",
            "ASCIIHexDecode first of two filters with an empty /DecodeParms array, which fails MuPDF's save with -z",
            empty_parms,
        ),
        fx(
            "streams-sorted-filter.pdf",
            "A stream dictionary of 102 keys, no /Length, and /Filter last once sorted, which MuPDF's lookup misses",
            sorted,
        ),
    ]
}

/// `encrypted`'s document, RC4 128-bit, with an unreachable string at
/// object 2, so renumbering moves every encrypted object.
fn encrypted_with_garbage() -> Vec<u8> {
    use crate::encrypt::{Handler, Method, Spec};
    let handler = Handler::new(
        Spec {
            v: 2,
            r: 3,
            bits: 128,
            method: Method::Rc4,
            crypt_filter: false,
            user: "",
            owner: "owner",
            encrypt_metadata: true,
            permissions: -4,
        },
        "gc",
    );
    let string = |num: u32, text: &str| {
        format!(
            "<{}>",
            crate::encrypt::hex(&handler.encrypt(num, text.as_bytes()))
        )
    };
    let mut pdf = Pdf::new("1.7");
    pdf.obj(1, "<</Type/Catalog/Pages 3 0 R>>");
    pdf.obj(2, &string(2, "unreachable"));
    pdf.obj(3, "<</Type/Pages/Kids[4 0 R]/Count 1>>");
    pdf.obj(
        4,
        "<</Type/Page/Parent 3 0 R/MediaBox[0 0 612 792]/Resources<</Font<</F1 6 0 R>>>>/Contents 5 0 R>>",
    );
    let content = miniz_oxide::deflate::compress_to_vec_zlib(&text_content("Encrypted"), 6);
    pdf.stream(5, "/Filter/FlateDecode", &handler.encrypt(5, &content));
    pdf.obj(6, FONT);
    pdf.obj(7, &format!("<</Title {}>>", string(7, "Renumbered")));
    pdf.obj(8, &handler.dict());
    let id = crate::encrypt::hex(&handler.id);
    pdf.finish_with_xref(&format!("/Info 7 0 R/Encrypt 8 0 R/ID[<{id}><{id}>]"))
}

/// Sample data for the filter fixtures: text with runs, repeats and enough
/// length for LZW to widen its codes and Flate to fill several buffers.
fn sample_data() -> Vec<u8> {
    let mut out = Vec::new();
    for i in 0..400u32 {
        out.extend_from_slice(
            format!("line {i:03}: the quick brown fox {} ", i * 7919 % 1000).as_bytes(),
        );
        out.extend(std::iter::repeat_n(b'.', (i % 9) as usize));
        out.push(b'\n');
    }
    out
}

/// The minimal document plus stream 6, `data` encoded with the filters
/// in `dict` (the stream's dictionary entries, without /Length).
fn with_filtered_stream(dict: &str, encoded: &[u8]) -> Vec<u8> {
    let mut pdf = hello(Pdf::new("1.7"), "Filters", false);
    pdf.stream(6, dict, encoded);
    pdf.finish_with_xref("")
}

/// One fixture per decoder, a chain, a predictor, and damaged data whose
/// recovery depends on MuPDF's buffer sizes.
fn filter_fixtures() -> Vec<Fixture> {
    use crate::encode;
    let data = sample_data();
    let fx = |name, about, bytes| Fixture {
        name,
        about,
        needs_repair: false,
        bytes,
    };
    let damaged = encode::flate_broken_after(&data[..14000], 3500);
    let mut truncated_lzw = encode::lzw(&data, true);
    truncated_lzw.truncate(truncated_lzw.len() * 2 / 3);
    let mut bad_hex = encode::ascii_hex(&data);
    let at = bad_hex.len() * 3 / 4;
    bad_hex[at] = b'G';
    let rows: Vec<u8> = (0..64u32)
        .flat_map(|i| [(i * 3) as u8, (i * 5) as u8, 200, (i & 7) as u8])
        .collect();
    vec![
        fx(
            "filter-lzw.pdf",
            "LZWDecode with the default EarlyChange 1",
            with_filtered_stream("/Filter/LZWDecode", &encode::lzw(&data, true)),
        ),
        fx(
            "filter-lzw-early-change-0.pdf",
            "LZWDecode with EarlyChange 0",
            with_filtered_stream(
                "/Filter/LZWDecode/DecodeParms<</EarlyChange 0>>",
                &encode::lzw(&data, false),
            ),
        ),
        fx(
            "filter-lzw-truncated.pdf",
            "LZWDecode cut off two thirds in",
            with_filtered_stream("/Filter/LZW", &truncated_lzw),
        ),
        fx(
            "filter-runlength.pdf",
            "RunLengthDecode",
            with_filtered_stream("/Filter/RunLengthDecode", &encode::run_length(&data)),
        ),
        fx(
            "filter-asciihex.pdf",
            "ASCIIHexDecode",
            with_filtered_stream("/Filter/ASCIIHexDecode", &encode::ascii_hex(&data)),
        ),
        fx(
            "filter-asciihex-bad-byte.pdf",
            "ASCIIHexDecode with a bad byte three quarters in",
            with_filtered_stream("/Filter/AHx", &bad_hex),
        ),
        fx(
            "filter-ascii85.pdf",
            "ASCII85Decode",
            with_filtered_stream("/Filter/ASCII85Decode", &encode::ascii85(&data)),
        ),
        fx(
            "filter-brotli.pdf",
            "BrotliDecode",
            with_filtered_stream("/Filter/BrotliDecode", &encode::brotli(&data)),
        ),
        fx(
            "filter-chain.pdf",
            "ASCII85Decode then FlateDecode",
            with_filtered_stream(
                "/Filter[/A85/FlateDecode]",
                &encode::ascii85(&encode::flate(&data)),
            ),
        ),
        fx(
            "filter-png-predictor.pdf",
            "FlateDecode with PNG Paeth prediction, 4 columns",
            with_filtered_stream(
                "/Filter/FlateDecode/DecodeParms<</Predictor 14/Columns 4>>",
                &encode::flate(&encode::png_predict(&rows, 4, 4)),
            ),
        ),
        fx(
            "filter-flate-damaged.pdf",
            "FlateDecode with a corrupt byte near the end",
            with_filtered_stream("/Filter/FlateDecode", &damaged),
        ),
    ]
}

/// The minimal document plus image XObject 6, `data` encoded with the
/// filters in `dict`.
fn with_image(width: usize, height: usize, cs: &str, bpc: u8, dict: &str, data: &[u8]) -> Vec<u8> {
    with_filtered_stream(
        &format!(
            "/Type/XObject/Subtype/Image/Width {width}/Height {height}/ColorSpace/{cs}\
             /BitsPerComponent {bpc}{dict}"
        ),
        data,
    )
}

/// Sample values with gradients, an edge and a disk, different per component.
fn image_sample(c: usize, x: usize, y: usize) -> u8 {
    let (dx, dy) = (x as i64 - 30, y as i64 - 20);
    if dx * dx + dy * dy < 180 {
        return (230 - c * 45) as u8;
    }
    let edge = if (x + 2 * y) % 23 < 4 { 90 } else { 0 };
    ((x * 3 + y * 5 + c * 70 + edge) % 256) as u8
}

/// A bilevel pattern (true is black): stripes, a checker corner and a disk.
fn bilevel(width: usize, height: usize) -> Vec<Vec<bool>> {
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| {
                    let (dx, dy) = (x as i64 - 40, y as i64 - 30);
                    (x / 7 + y / 5) % 3 == 0
                        || dx * dx + dy * dy < 150
                        || (x < 16 && y < 16 && (x / 4 + y / 4) % 2 == 0)
                })
                .collect()
        })
        .collect()
}

/// One fixture per image filter and the decoder paths that differ between
/// decoders: every chroma subsampling libjpeg scales in the IDCT, color
/// transforms, restarts, progressive scans (complete and partial), damaged
/// data; CCITT Group 4; JBIG2 MMR pages, globals and what jbig2dec rejects.
fn image_fixtures() -> Vec<Fixture> {
    use crate::encode;
    use crate::jpeg::{self, Component, Scan, Spec};
    let fx = |name, about, bytes| Fixture {
        name,
        about,
        needs_repair: false,
        bytes,
    };
    let comps = |f: &[(u8, usize, usize)]| -> Vec<Component> {
        f.iter().map(|&(id, h, v)| Component { id, h, v }).collect()
    };
    let spec = |width, height, components, jfif, adobe| Spec {
        width,
        height,
        components,
        sample: image_sample,
        jfif,
        adobe,
        restart_interval: 0,
        progressive: None,
    };
    let ycc = |h: usize, v: usize| comps(&[(1, h, v), (2, 1, 1), (3, 1, 1)]);
    let (w, h) = (75, 51);
    let dct = |cs: &str, s: &Spec, extra: &str| {
        with_image(
            s.width,
            s.height,
            cs,
            8,
            &format!("/Filter/DCTDecode{extra}"),
            &jpeg::encode(s),
        )
    };
    let scan = |comps: &[usize], ss, se, ah, al| Scan {
        comps: comps.to_vec(),
        ss,
        se,
        ah,
        al,
    };
    let progressive = vec![
        scan(&[0, 1, 2], 0, 0, 0, 1),
        scan(&[0], 1, 5, 0, 0),
        scan(&[0], 6, 63, 0, 0),
        scan(&[1], 1, 63, 0, 0),
        scan(&[2], 1, 63, 0, 0),
        scan(&[0, 1, 2], 0, 0, 1, 0),
    ];
    let mut truncated = jpeg::encode(&spec(w, h, ycc(2, 2), true, None));
    truncated.truncate(truncated.len() * 3 / 5);
    let mut whitespace = b"\r\n \n".to_vec();
    whitespace.extend(jpeg::encode(&spec(61, 37, comps(&[(1, 1, 1)]), true, None)));

    let (bw, bh) = (97, 60);
    let rows = bilevel(bw, bh);
    let g4 = encode::g4(bw, &rows);
    let ccitt = |dict: &str, data: &[u8]| {
        with_image(
            bw,
            bh,
            "DeviceGray",
            1,
            &format!("/Filter/CCITTFaxDecode{dict}"),
            data,
        )
    };
    let jbig2 = |dict: &str, data: &[u8]| {
        with_image(
            bw,
            bh,
            "DeviceGray",
            1,
            &format!("/Filter/JBIG2Decode{dict}"),
            data,
        )
    };
    let mut jbig2_file = vec![0x97, 0x4a, 0x42, 0x32, 0x0d, 0x0a, 0x1a, 0x0a, 0x01];
    jbig2_file.extend(1u32.to_be_bytes());
    jbig2_file.extend(encode::jbig2_mmr_page(bw, &rows, 38));
    let jbig2_globals = {
        let mut pdf = hello(Pdf::new("1.7"), "Filters", false);
        pdf.stream(
            6,
            &format!(
                "/Type/XObject/Subtype/Image/Width {bw}/Height {bh}/ColorSpace/DeviceGray\
                 /BitsPerComponent 1/Filter/JBIG2Decode/DecodeParms<</JBIG2Globals 7 0 R>>"
            ),
            &encode::jbig2_mmr_page(bw, &rows, 38),
        );
        pdf.stream(7, "", &encode::jbig2_pattern_dict(3, 4, 2));
        pdf.finish_with_xref("")
    };

    vec![
        fx(
            "filter-dct-gray.pdf",
            "DCTDecode: baseline grayscale, odd size",
            dct(
                "DeviceGray",
                &spec(61, 37, comps(&[(1, 1, 1)]), true, None),
                "",
            ),
        ),
        fx(
            "filter-dct-420.pdf",
            "DCTDecode: YCbCr 4:2:0 (16x16 chroma IDCT)",
            dct("DeviceRGB", &spec(w, h, ycc(2, 2), true, None), ""),
        ),
        fx(
            "filter-dct-422.pdf",
            "DCTDecode: YCbCr 4:2:2 (16x8 chroma IDCT)",
            dct("DeviceRGB", &spec(w, h, ycc(2, 1), true, None), ""),
        ),
        fx(
            "filter-dct-440.pdf",
            "DCTDecode: YCbCr 4:4:0 (8x16 chroma IDCT)",
            dct("DeviceRGB", &spec(w, h, ycc(1, 2), true, None), ""),
        ),
        fx(
            "filter-dct-411.pdf",
            "DCTDecode: YCbCr 4:1:1 (16x8 chroma IDCT, then upsampled)",
            dct("DeviceRGB", &spec(w, h, ycc(4, 1), true, None), ""),
        ),
        fx(
            "filter-dct-rgb.pdf",
            "DCTDecode: three components, Adobe marker with transform 0",
            dct("DeviceRGB", &spec(w, h, ycc(1, 1), false, Some(0)), ""),
        ),
        fx(
            "filter-dct-colortransform-0.pdf",
            "DCTDecode: no markers, /ColorTransform 0 (data left untransformed)",
            dct(
                "DeviceRGB",
                &spec(w, h, ycc(1, 1), false, None),
                "/DecodeParms<</ColorTransform 0>>",
            ),
        ),
        fx(
            "filter-dct-cmyk.pdf",
            "DCTDecode: CMYK, Adobe transform 0",
            dct(
                "DeviceCMYK",
                &spec(
                    w,
                    h,
                    comps(&[(1, 1, 1), (2, 1, 1), (3, 1, 1), (4, 1, 1)]),
                    false,
                    Some(0),
                ),
                "",
            ),
        ),
        fx(
            "filter-dct-ycck.pdf",
            "DCTDecode: YCCK, Adobe transform 2, 4:2:0 with full-size K",
            dct(
                "DeviceCMYK",
                &spec(
                    w,
                    h,
                    comps(&[(1, 2, 2), (2, 1, 1), (3, 1, 1), (4, 2, 2)]),
                    false,
                    Some(2),
                ),
                "",
            ),
        ),
        fx(
            "filter-dct-restart.pdf",
            "DCTDecode: 4:2:0 with a restart marker every two MCUs",
            dct(
                "DeviceRGB",
                &Spec {
                    restart_interval: 2,
                    ..spec(w, h, ycc(2, 2), true, None)
                },
                "",
            ),
        ),
        fx(
            "filter-dct-progressive.pdf",
            "DCTDecode: progressive 4:2:0, spectral selection and DC refinement",
            dct(
                "DeviceRGB",
                &Spec {
                    progressive: Some(progressive.clone()),
                    ..spec(w, h, ycc(2, 2), true, None)
                },
                "",
            ),
        ),
        fx(
            "filter-dct-progressive-partial.pdf",
            "DCTDecode: progressive with only the first two scans (block smoothing)",
            dct(
                "DeviceRGB",
                &Spec {
                    progressive: Some(progressive[..2].to_vec()),
                    ..spec(w, h, ycc(2, 2), true, None)
                },
                "",
            ),
        ),
        fx(
            "filter-dct-truncated.pdf",
            "DCTDecode: baseline 4:2:0 cut off three fifths in",
            with_image(w, h, "DeviceRGB", 8, "/Filter/DCTDecode", &truncated),
        ),
        fx(
            "filter-dct-whitespace.pdf",
            "DCTDecode: whitespace before the SOI marker",
            with_image(61, 37, "DeviceGray", 8, "/Filter/DCTDecode", &whitespace),
        ),
        fx(
            "filter-dct-flate.pdf",
            "FlateDecode then DCTDecode",
            with_image(
                w,
                h,
                "DeviceRGB",
                8,
                "/Filter[/FlateDecode/DCTDecode]",
                &encode::flate(&jpeg::encode(&spec(w, h, ycc(2, 2), true, None))),
            ),
        ),
        fx(
            "filter-ccitt-g4.pdf",
            "CCITTFaxDecode: Group 4",
            ccitt(
                &format!("/DecodeParms<</K -1/Columns {bw}/Rows {bh}>>"),
                &g4,
            ),
        ),
        fx(
            "filter-ccitt-g4-blackis1.pdf",
            "CCITTFaxDecode: Group 4 with /BlackIs1",
            ccitt(
                &format!("/DecodeParms<</K -1/Columns {bw}/Rows {bh}/BlackIs1 true>>"),
                &g4,
            ),
        ),
        fx(
            "filter-ccitt-g4-truncated.pdf",
            "CCITTFaxDecode: Group 4 cut off halfway",
            ccitt(
                &format!("/DecodeParms<</K -1/Columns {bw}/Rows {bh}>>"),
                &g4[..g4.len() / 2],
            ),
        ),
        fx(
            "filter-jbig2-mmr.pdf",
            "JBIG2Decode: one page, an MMR-coded generic region",
            jbig2("", &encode::jbig2_mmr_page(bw, &rows, 38)),
        ),
        fx(
            "filter-jbig2-globals.pdf",
            "JBIG2Decode with /JBIG2Globals (a pattern dictionary)",
            jbig2_globals,
        ),
        fx(
            "filter-jbig2-globals-not-stream.pdf",
            "JBIG2Decode whose /JBIG2Globals is not a stream",
            jbig2(
                "/DecodeParms<</JBIG2Globals<<>>>>",
                &encode::jbig2_mmr_page(bw, &rows, 38),
            ),
        ),
        fx(
            "filter-jbig2-intermediate.pdf",
            "JBIG2Decode: an intermediate generic region, which jbig2dec rejects",
            jbig2("", &encode::jbig2_mmr_page(bw, &rows, 36)),
        ),
        fx(
            "filter-jbig2-file-header.pdf",
            "JBIG2Decode: a JBIG2 file header where none belongs",
            jbig2("", &jbig2_file),
        ),
        fx(
            "filter-jpx.pdf",
            "JPXDecode: passed through undecoded",
            with_image(
                8,
                8,
                "DeviceGray",
                8,
                "/Filter/JPXDecode",
                &sample_data()[..256],
            ),
        ),
    ]
}

/// Inputs whose rewrite depends on MuPDF's writing rules: dictionary
/// order, literal or hex strings, real formatting, null values, object
/// stream members and free entry generations.
fn writer_fixtures() -> Vec<Fixture> {
    let fx = |name, about, bytes| Fixture {
        name,
        about,
        needs_repair: false,
        bytes,
    };

    // 120 keys inserted in reverse order: MuPDF sorts a dictionary once it
    // grows past 100 entries.
    let mut big = String::from("<<");
    for i in (0..120).rev() {
        let _ = write!(big, "/K{i:03} {i}");
    }
    big.push_str("/Type/Example/Subtype/Big>>");

    let strings = "[(plain) (with \\(parens\\) and \\\\) (tab\\tnew\\nline) <00010203> \
        <FEFF00410042> (caf\\351) (\\000\\001) () <> (\\(\\(\\)\\))]";
    let reals = "[0.5 -0.25 .001 0.0000001 1.0 -0.0 3.4 123456789.0 99999.5 \
        -2147483648.0 3000000000.0 1e5 0.30000001 612.123]";
    let names = "[/A#20B /Hash#23 /Paren#28 /Slash#2F /caf#E9 /NUL#00x /x]";
    let mut doc = hello(Pdf::new("1.6"), "Writer", false);
    doc.obj(6, &big);
    doc.obj(7, strings);
    doc.obj(8, reals);
    doc.obj(9, names);
    doc.obj(
        10,
        "<</Type/Sig/Filter/Adobe.PPKLite/ByteRange[0 0 0 0]/Contents<00FF>>>",
    );
    doc.obj(
        11,
        "<</Type null/Subtype/X/Empty null/List[null true false]>>",
    );
    let syntax = doc.finish_with_xref("");

    // /ID and /Info null in the trailer, which MuPDF leaves out.
    let null_trailer =
        hello(Pdf::new("1.4"), "Null trailer", false).finish_with_xref("/ID null/Info null");

    // Free entries with generations 7, 65535 and 65536 (an unsigned short
    // in MuPDF, so 0).
    let mut pdf = hello(Pdf::new("1.4"), "Free entries", false);
    let start = pdf.out.len();
    pdf.line("xref");
    pdf.line("0 9");
    pdf.out.extend_from_slice(b"0000000000 65535 f \n");
    for num in 1..6 {
        let entry = format!("{:010} 00000 n \n", pdf.offsets[&num]);
        pdf.out.extend_from_slice(entry.as_bytes());
    }
    for generation in ["00007", "65535", "65536"] {
        pdf.out
            .extend_from_slice(format!("0000000000 {generation} f \n").as_bytes());
    }
    pdf.line("trailer");
    pdf.line("<</Size 9/Root 1 0 R>>");
    pdf.line("startxref");
    pdf.line(&start.to_string());
    pdf.line("%%EOF");
    let free = pdf.out;

    // An empty name as a key, before a number: MuPDF writes `/7 0 R`
    // without a space, which reads back as the key /7. `clean`'s
    // verification fails on it.
    let mut doc = hello(Pdf::new("1.4"), "Lossy", false);
    doc.obj(6, "<</ 7 0 R/Next 7 0 R>>");
    doc.obj(7, "(target)");
    let lossy = doc.finish_with_xref("");

    vec![
        fx(
            "write-syntax.pdf",
            "Writer: a 120-key dictionary, strings, reals, names, a signature, nulls",
            syntax,
        ),
        fx(
            "write-lossy-name.pdf",
            "Writer: an empty key name MuPDF writes so that it reads back as another",
            lossy,
        ),
        fx(
            "write-null-trailer.pdf",
            "Writer: /ID and /Info null in the trailer",
            null_trailer,
        ),
        fx(
            "write-free-generations.pdf",
            "Writer: free entries with generations 7, 65535 and 65536",
            free,
        ),
        fx(
            "write-objstm-null.pdf",
            "Writer: an object stream member that is null",
            objstm_with_null(),
        ),
    ]
}

/// `xref_stream`'s document with a fourth object stream member, `null`.
fn objstm_with_null() -> Vec<u8> {
    let mut pdf = Pdf::new("1.5");
    let content = text_content("Null member");
    pdf.stream(4, "", &content);
    pdf.obj(5, FONT);
    let objects = [
        (1, "<</Type/Catalog/Pages 2 0 R>>"),
        (2, "<</Type/Pages/Kids[3 0 R]/Count 1>>"),
        (
            3,
            "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources<</Font<</F1 5 0 R>>>>/Contents 4 0 R>>",
        ),
        (8, "null"),
    ];
    let mut index = String::new();
    let mut body = String::new();
    for (num, text) in objects {
        let _ = write!(index, "{num} {} ", body.len());
        body.push_str(text);
        body.push('\n');
    }
    let data = format!("{index}{body}");
    pdf.stream(
        6,
        &format!("/Type/ObjStm/N {}/First {}", objects.len(), index.len()),
        data.as_bytes(),
    );
    let xref_offset = pdf.out.len();
    let mut rows = Vec::new();
    let mut row = |kind: u8, field2: u32, field3: u16| {
        rows.push(kind);
        rows.extend_from_slice(&field2.to_be_bytes());
        rows.extend_from_slice(&field3.to_be_bytes());
    };
    row(0, 0, 65535);
    for i in 0..3u16 {
        row(2, 6, i);
    }
    for num in [4, 5, 6] {
        row(1, u32::try_from(pdf.offsets[&num]).unwrap_or(0), 0);
    }
    row(1, u32::try_from(xref_offset).unwrap_or(0), 0);
    row(2, 6, 3);
    pdf.stream(7, "/Type/XRef/Size 9/W[1 4 2]/Root 1 0 R", &rows);
    pdf.line("startxref");
    pdf.line(&xref_offset.to_string());
    pdf.line("%%EOF");
    pdf.out
}

/// One file per security handler revision and method, plus files that need
/// a user password. Each has an encrypted Info string and content stream.
fn encrypted_fixtures() -> Vec<Fixture> {
    use crate::encrypt::{Method, Spec};
    let spec = |v, r, bits, method, crypt_filter| Spec {
        v,
        r,
        bits,
        method,
        crypt_filter,
        user: "",
        owner: "owner",
        encrypt_metadata: true,
        permissions: -4,
    };
    let cases: [(&'static str, &'static str, Spec); 8] = [
        (
            "encrypted-rc4-40.pdf",
            "Standard handler V1 R2, RC4 40-bit, empty user password",
            spec(1, 2, 40, Method::Rc4, false),
        ),
        (
            "encrypted-rc4-128.pdf",
            "Standard handler V2 R3, RC4 128-bit, empty user password",
            spec(2, 3, 128, Method::Rc4, false),
        ),
        (
            "encrypted-rc4-crypt-filter.pdf",
            "Standard handler V4 R4 with an RC4 (V2) crypt filter",
            spec(4, 4, 128, Method::Rc4, true),
        ),
        (
            "encrypted-aes-128.pdf",
            "Standard handler V4 R4 with an AESV2 crypt filter",
            spec(4, 4, 128, Method::AesV2, true),
        ),
        (
            "encrypted-aes-128-clear-metadata.pdf",
            "AESV2 with /EncryptMetadata false, which changes the file key",
            Spec {
                encrypt_metadata: false,
                ..spec(4, 4, 128, Method::AesV2, true)
            },
        ),
        (
            "encrypted-aes-256-r5.pdf",
            "Standard handler V5 R5 (Adobe extension level 3), AES-256",
            spec(5, 5, 256, Method::AesV3, true),
        ),
        (
            "encrypted-aes-256.pdf",
            "Standard handler V5 R6 (ISO 32000-2), AES-256",
            spec(5, 6, 256, Method::AesV3, true),
        ),
        (
            "encrypted-user-password.pdf",
            "AES-256 R6 with user password \"user\": opening needs a password",
            Spec {
                user: "user",
                ..spec(5, 6, 256, Method::AesV3, true)
            },
        ),
    ];
    let mut out: Vec<Fixture> = cases
        .into_iter()
        .map(|(name, about, spec)| Fixture {
            name,
            about,
            needs_repair: false,
            bytes: encrypted(spec, name),
        })
        .collect();
    out.push(Fixture {
        name: "encrypted-restricted.pdf",
        about: "AES-128 opened with the empty user password, allowing printing and accessibility only",
        needs_repair: false,
        bytes: encrypted(
            Spec {
                // Every bit set except 1-2, edit, copy, annotate, form,
                // assemble and high-quality print.
                permissions: 0xffff_f2c4_u32 as i32,
                ..spec(4, 4, 128, Method::AesV2, true)
            },
            "encrypted-restricted.pdf",
        ),
    });
    out.push(Fixture {
        name: "encrypted-rc4-user-password.pdf",
        about: "RC4 128-bit with user password \"user\": decrypted with the wrong key",
        needs_repair: false,
        bytes: encrypted(
            Spec {
                user: "user",
                ..spec(2, 3, 128, Method::Rc4, false)
            },
            "encrypted-rc4-user-password.pdf",
        ),
    });
    out
}

/// The minimal document, encrypted: objects 1-5 as in `hello`, the Info
/// dictionary (6) and the /Encrypt dictionary (7, not encrypted itself).
fn encrypted(spec: crate::encrypt::Spec, seed: &str) -> Vec<u8> {
    let handler = crate::encrypt::Handler::new(spec, seed);
    let string = |num: u32, text: &str| {
        format!(
            "<{}>",
            crate::encrypt::hex(&handler.encrypt(num, text.as_bytes()))
        )
    };
    let mut pdf = Pdf::new("1.7");
    pdf.obj(1, "<</Type/Catalog/Pages 2 0 R>>");
    pdf.obj(2, "<</Type/Pages/Kids[3 0 R]/Count 1>>");
    pdf.obj(
        3,
        "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources<</Font<</F1 5 0 R>>>>/Contents 4 0 R>>",
    );
    let content = miniz_oxide::deflate::compress_to_vec_zlib(&text_content("Encrypted"), 6);
    pdf.stream(4, "/Filter/FlateDecode", &handler.encrypt(4, &content));
    pdf.obj(5, FONT);
    pdf.obj(
        6,
        &format!(
            "<</Title {}/Producer {}>>",
            string(6, &format!("Encrypted: {seed}")),
            string(6, "nupdf fixtures")
        ),
    );
    pdf.obj(7, &handler.dict());
    let id = crate::encrypt::hex(&handler.id);
    pdf.finish_with_xref(&format!("/Info 6 0 R/Encrypt 7 0 R/ID[<{id}><{id}>]"))
}

pub fn run(root: &Path, check: bool) -> Result<()> {
    let dir = root.join("corpus/smoke");
    let fixtures = all();
    let mut expected: BTreeMap<String, Vec<u8>> = fixtures
        .iter()
        .map(|f| (f.name.to_string(), f.bytes.clone()))
        .collect();
    expected.insert("README.md".into(), readme(&fixtures).into_bytes());

    if check {
        let mut problems = Vec::new();
        for (name, bytes) in &expected {
            match std::fs::read(dir.join(name)) {
                Ok(found) if &found == bytes => {}
                Ok(_) => problems.push(format!("{name} differs from its generator")),
                Err(_) => problems.push(format!("{name} is missing")),
            }
        }
        for entry in std::fs::read_dir(&dir).context("reading corpus/smoke")? {
            let name = entry?.file_name().to_string_lossy().to_string();
            if !expected.contains_key(&name) {
                problems.push(format!("{name} has no generator"));
            }
        }
        if !problems.is_empty() {
            bail!(
                "corpus/smoke is stale; run `cargo xtask fixtures`:\n  {}",
                problems.join("\n  ")
            );
        }
        println!("corpus/smoke is up to date ({} fixtures)", fixtures.len());
        return Ok(());
    }

    std::fs::create_dir_all(&dir)?;
    for (name, bytes) in &expected {
        std::fs::write(dir.join(name), bytes)?;
    }
    println!("wrote {} fixtures to {}", fixtures.len(), dir.display());
    Ok(())
}

fn readme(fixtures: &[Fixture]) -> String {
    let mut out = String::from(
        "# Smoke corpus\n\n\
         Generated by `cargo xtask fixtures` (xtask/src/fixtures.rs). Do not edit by hand.\n\n\
         | File | Exercises | MuPDF repairs it |\n|---|---|---|\n",
    );
    for f in fixtures {
        let repair = if f.needs_repair { "yes" } else { "no" };
        let _ = writeln!(out, "| `{}` | {} | {repair} |", f.name, f.about);
    }
    out
}

/// Minimal PDF writer that tracks object offsets so xref tables are exact.
struct Pdf {
    out: Vec<u8>,
    eol: &'static str,
    offsets: BTreeMap<u32, usize>,
    skew: usize,
}

impl Pdf {
    fn new(version: &str) -> Self {
        Self::with_eol(version, "\n")
    }

    fn with_eol(version: &str, eol: &'static str) -> Self {
        let mut pdf = Pdf {
            out: Vec::new(),
            eol,
            offsets: BTreeMap::new(),
            skew: 0,
        };
        pdf.line(&format!("%PDF-{version}"));
        // Binary comment so tools treat the file as binary, as real writers do.
        pdf.out.extend_from_slice(b"%\xe2\xe3\xcf\xd3");
        pdf.out.extend_from_slice(eol.as_bytes());
        pdf
    }

    fn line(&mut self, text: &str) {
        self.out.extend_from_slice(text.as_bytes());
        self.out.extend_from_slice(self.eol.as_bytes());
    }

    fn obj(&mut self, num: u32, body: &str) {
        self.offsets.insert(num, self.out.len());
        self.line(&format!("{num} 0 obj"));
        self.line(body);
        self.line("endobj");
    }

    /// An object whose body is arbitrary bytes (for lexer edge cases).
    fn obj_bytes(&mut self, num: u32, body: &[u8]) {
        self.offsets.insert(num, self.out.len());
        self.line(&format!("{num} 0 obj"));
        self.out.extend_from_slice(body);
        self.out.extend_from_slice(self.eol.as_bytes());
        self.line("endobj");
    }

    /// A stream whose dictionary is written as given, /Length included.
    fn stream_as_is(&mut self, num: u32, dict: &str, data: &[u8]) {
        self.offsets.insert(num, self.out.len());
        self.line(&format!("{num} 0 obj"));
        self.line(dict);
        self.line("stream");
        self.out.extend_from_slice(data);
        self.out.extend_from_slice(self.eol.as_bytes());
        self.line("endstream");
        self.line("endobj");
    }

    fn stream(&mut self, num: u32, dict: &str, data: &[u8]) {
        self.offsets.insert(num, self.out.len());
        self.line(&format!("{num} 0 obj"));
        self.line(&format!("<<{dict}/Length {}>>", data.len()));
        self.line("stream");
        self.out.extend_from_slice(data);
        self.out.extend_from_slice(self.eol.as_bytes());
        self.line("endstream");
        self.line("endobj");
    }

    fn skew_offsets(mut self, by: usize) -> Self {
        self.skew = by;
        self
    }

    /// Writes an xref section covering `nums` (plus entry 0 when `with_free_head`).
    fn xref(&mut self, nums: &[u32], with_free_head: bool) -> usize {
        let start = self.out.len();
        self.line("xref");
        let mut entries: Vec<u32> = nums.to_vec();
        if with_free_head {
            entries.insert(0, 0);
        }
        entries.sort_unstable();
        // Group into contiguous subsections.
        let mut i = 0;
        while i < entries.len() {
            let mut j = i + 1;
            while j < entries.len() && entries[j] == entries[j - 1] + 1 {
                j += 1;
            }
            self.line(&format!("{} {}", entries[i], j - i));
            for &num in &entries[i..j] {
                // Entries are exactly 20 bytes: the EOL is padded to 2 bytes.
                let eol = if self.eol.len() == 2 {
                    self.eol.to_string()
                } else {
                    format!(" {}", self.eol)
                };
                let entry = match self.offsets.get(&num) {
                    Some(&offset) if num != 0 => format!("{:010} 00000 n{eol}", offset + self.skew),
                    _ => format!("0000000000 65535 f{eol}"),
                };
                self.out.extend_from_slice(entry.as_bytes());
            }
            i = j;
        }
        start
    }

    fn size(&self) -> u32 {
        self.offsets.keys().max().map_or(1, |n| n + 1)
    }

    /// Like `finish_with_xref`, but with a deliberately wrong /Size.
    fn finish_with_size(mut self, size: u32) -> Vec<u8> {
        let nums: Vec<u32> = (1..self.size()).collect();
        let start = self.xref(&nums, true);
        self.line("trailer");
        self.line(&format!("<</Root 1 0 R/Size {size}>>"));
        self.line("startxref");
        self.line(&start.to_string());
        self.line("%%EOF");
        self.out
    }

    fn finish_with_xref(mut self, extra_trailer: &str) -> Vec<u8> {
        let nums: Vec<u32> = (1..self.size()).collect();
        let start = self.xref(&nums, true);
        self.trailer(&format!("/Root 1 0 R{extra_trailer}"), start);
        self.out
    }

    fn trailer(&mut self, entries: &str, xref_start: usize) {
        let size = self.size();
        self.line("trailer");
        self.line(&format!("<<{entries}/Size {size}>>"));
        self.line("startxref");
        self.line(&xref_start.to_string());
        self.line("%%EOF");
    }
}

const FONT: &str = "<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>";

fn text_content(text: &str) -> Vec<u8> {
    format!("BT /F1 24 Tf 72 720 Td ({text}) Tj ET").into_bytes()
}

/// Objects 1-5: catalog, pages, one page, its content, its font.
fn hello(mut pdf: Pdf, text: &str, compress: bool) -> Pdf {
    pdf.obj(1, "<</Type/Catalog/Pages 2 0 R>>");
    pdf.obj(2, "<</Type/Pages/Kids[3 0 R]/Count 1>>");
    pdf.obj(
        3,
        "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources<</Font<</F1 5 0 R>>>>/Contents 4 0 R>>",
    );
    let content = text_content(text);
    if compress {
        let packed = miniz_oxide::deflate::compress_to_vec_zlib(&content, 6);
        pdf.stream(4, "/Filter/FlateDecode", &packed);
    } else {
        pdf.stream(4, "", &content);
    }
    pdf.obj(5, FONT);
    pdf
}

fn catalog_version() -> Vec<u8> {
    let mut pdf = Pdf::new("1.4");
    pdf.obj(1, "<</Type/Catalog/Pages 2 0 R/Version/1.7>>");
    pdf.obj(2, "<</Type/Pages/Kids[3 0 R]/Count 1>>");
    pdf.obj(
        3,
        "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources<</Font<</F1 5 0 R>>>>/Contents 4 0 R>>",
    );
    pdf.stream(4, "", &text_content("Catalog says 1.7"));
    pdf.obj(5, FONT);
    pdf.finish_with_xref("")
}

fn multipage_inherit() -> Vec<u8> {
    let mut pdf = Pdf::new("1.4");
    pdf.obj(1, "<</Type/Catalog/Pages 2 0 R>>");
    pdf.obj(
        2,
        "<</Type/Pages/Kids[3 0 R 4 0 R 5 0 R]/Count 3/MediaBox[0 0 595 842]/Resources<</Font<</F1 7 0 R>>>>>>",
    );
    pdf.obj(3, "<</Type/Page/Parent 2 0 R/Contents 6 0 R>>");
    pdf.obj(4, "<</Type/Page/Parent 2 0 R/Contents 6 0 R/Rotate 90>>");
    pdf.obj(
        5,
        "<</Type/Page/Parent 2 0 R/Contents 6 0 R/MediaBox[0 0 300 300]>>",
    );
    pdf.stream(6, "", &text_content("Inherited"));
    pdf.obj(7, FONT);
    pdf.finish_with_xref("")
}

fn xref_stream() -> Vec<u8> {
    let mut pdf = Pdf::new("1.5");
    let content = text_content("Object streams");
    pdf.stream(4, "", &content);
    pdf.obj(5, FONT);

    // Object stream 6 holds objects 1-3.
    let objects = [
        (1, "<</Type/Catalog/Pages 2 0 R>>"),
        (2, "<</Type/Pages/Kids[3 0 R]/Count 1>>"),
        (
            3,
            "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources<</Font<</F1 5 0 R>>>>/Contents 4 0 R>>",
        ),
    ];
    let mut index = String::new();
    let mut body = String::new();
    for (num, text) in objects {
        let _ = write!(index, "{num} {} ", body.len());
        body.push_str(text);
        body.push('\n');
    }
    let data = format!("{index}{body}");
    pdf.stream(
        6,
        &format!("/Type/ObjStm/N {}/First {}", objects.len(), index.len()),
        data.as_bytes(),
    );

    // Xref stream 7, uncompressed, W [1 4 2]: type, offset or objstm number, gen or index.
    let xref_offset = pdf.out.len();
    let mut rows = Vec::new();
    let mut row = |kind: u8, field2: u32, field3: u16| {
        rows.push(kind);
        rows.extend_from_slice(&field2.to_be_bytes());
        rows.extend_from_slice(&field3.to_be_bytes());
    };
    row(0, 0, 65535);
    for (i, _) in objects.iter().enumerate() {
        row(2, 6, u16::try_from(i).unwrap_or(0));
    }
    for num in [4, 5, 6] {
        row(1, u32::try_from(pdf.offsets[&num]).unwrap_or(0), 0);
    }
    row(1, u32::try_from(xref_offset).unwrap_or(0), 0);
    pdf.stream(7, "/Type/XRef/Size 8/W[1 4 2]/Root 1 0 R", &rows);
    pdf.line("startxref");
    pdf.line(&xref_offset.to_string());
    pdf.line("%%EOF");
    pdf.out
}

fn incremental_update() -> Vec<u8> {
    let mut pdf = hello(Pdf::new("1.4"), "Revision 1", false);
    let nums: Vec<u32> = (1..pdf.size()).collect();
    let first = pdf.xref(&nums, true);
    pdf.trailer("/Root 1 0 R", first);

    pdf.stream(4, "", &text_content("Revision 2"));
    pdf.obj(6, "<</Title(Revision 2)/Producer(nupdf fixtures)>>");
    let second = pdf.xref(&[4, 6], true);
    pdf.trailer(&format!("/Root 1 0 R/Info 6 0 R/Prev {first}"), second);
    pdf.out
}

fn garbage_prefix() -> Vec<u8> {
    let body = hello(Pdf::new("1.6"), "Junk first", false).finish_with_xref("");
    let mut out = b"Content-Type: application/pdf\r\nX-Junk: ".to_vec();
    out.resize(64, b'x');
    out.extend_from_slice(&body);
    out
}

fn no_xref() -> Vec<u8> {
    let mut pdf = hello(Pdf::new("1.4"), "No xref", false);
    pdf.line("trailer");
    pdf.line("<</Root 1 0 R/Size 6>>");
    pdf.line("%%EOF");
    pdf.out
}

fn truncated() -> Vec<u8> {
    let pdf = hello(Pdf::new("1.7"), "Truncated", false);
    let cut = pdf.offsets[&3] + 20;
    pdf.out[..cut].to_vec()
}

/// One fixture per lexer leniency, xref repair path and page tree quirk.
fn edge_cases() -> Vec<Fixture> {
    let fx = |name, about, needs_repair, bytes| Fixture {
        name,
        about,
        needs_repair,
        bytes,
    };
    vec![
        fx(
            "lexer-numbers.pdf",
            "Number leniency: --5, 0.0-5684342, 1.2.3, .5, 4., long integer parts, overflow",
            false,
            with_object(
                "<</A --5 /B 0.000000000000-5684342 /C 1.2.3 /D .5 /E 4. /F 12345678901.5 \
                 /G 18446744073709551617 /H 1e5 /I -.25 /J 3.40282347e38 /K 0.1 /L 2.000000001>>",
            ),
        ),
        fx(
            "lexer-names.pdf",
            "Name escapes: #20, #2f, #00, malformed escapes, non-ASCII bytes, an over-long name",
            false,
            with_object_bytes(&names_body()),
        ),
        fx(
            "lexer-strings.pdf",
            "Literal and hex strings: escapes, octal, line continuations, raw EOLs, odd and invalid hex",
            false,
            with_object_bytes(
                b"[(a\\(b\\)c) (\\n\\r\\t\\b\\f\\(\\)\\\\) (\\101\\7\\0123) (x\\\nY\\\r\nZ) \
                  (cr\rcrlf\r\nlf\n) (nest(ed)) <48656c6c6f> <4> <4z1> <>]",
            ),
        ),
        fx(
            "dict-quirks.pdf",
            "Duplicate keys, a broken reference that swallows a token, references in arrays",
            false,
            with_object("<</A 1 /A 2 /B 3 4 (x) /C [1 2 0 R 3] /D null /E <</F 5 0 R>>>>"),
        ),
        fx(
            "xref-19-byte-entries.pdf",
            "Classic xref whose entries are 19 bytes (no space before the EOL)",
            false,
            nineteen_byte_xref(),
        ),
        fx(
            "xref-subsection-same-line.pdf",
            "The first xref entry sits on the subsection header line",
            false,
            subsection_same_line(),
        ),
        fx(
            "xref-size-off-by-one.pdf",
            "Trailer /Size one smaller than the table",
            false,
            hello(Pdf::new("1.4"), "Size", false).finish_with_size(5),
        ),
        fx(
            "xref-size-too-small.pdf",
            "Trailer /Size far smaller than the table, which sends MuPDF to repair",
            true,
            hello(Pdf::new("1.4"), "Size", false).finish_with_size(2),
        ),
        fx(
            "xref-prev-loop.pdf",
            "A trailer whose /Prev points at its own xref section",
            false,
            prev_loop(),
        ),
        fx(
            "stream-length-short.pdf",
            "Stream /Length shorter than the data",
            false,
            with_stream("<</Length 5>>", b"0 0 m 100 100 l S"),
        ),
        fx(
            "stream-length-long.pdf",
            "Stream /Length running past endstream",
            false,
            with_stream("<</Length 60>>", b"0 0 m 100 100 l S"),
        ),
        fx(
            "stream-length-indirect.pdf",
            "Stream /Length given as an indirect reference",
            false,
            indirect_length(),
        ),
        fx(
            "missing-endobj.pdf",
            "An object without endobj, with a correct xref",
            false,
            missing_endobj(),
        ),
        fx(
            "no-header.pdf",
            "No %PDF- header: MuPDF repairs and ignores every stream /Length",
            true,
            no_header(),
        ),
        fx(
            "objstm-first-too-small.pdf",
            "Object stream whose /First points inside its header",
            false,
            objstm_first_too_small(),
        ),
        fx(
            "wrong-object-at-offset.pdf",
            "An xref offset that points at a different object, repaired on first use",
            true,
            wrong_object_at_offset(),
        ),
        fx(
            "pages-count-too-high.pdf",
            "/Count claims three pages, the tree has one; MuPDF rewrites /Count",
            false,
            page_tree(
                "<</Type/Pages/Kids[3 0 R]/Count 3/MediaBox[0 0 200 200]>>",
                &[],
                "",
            ),
        ),
        fx(
            "pages-untyped-node.pdf",
            "An intermediate page tree node without /Type, found by the slow lookup",
            false,
            page_tree(
                "<</Type/Pages/Kids[4 0 R]/Count 1>>",
                &[(4, "<</Kids[3 0 R]/Count 1/Parent 2 0 R>>")],
                "/Parent 4 0 R",
            ),
        ),
        fx(
            "pages-parent-cycle.pdf",
            "The page tree root lists itself as its /Parent",
            false,
            page_tree(
                "<</Type/Pages/Kids[3 0 R]/Count 1/Parent 2 0 R/MediaBox[0 0 200 200]>>",
                &[],
                "",
            ),
        ),
    ]
}

/// A minimal one-page file with object 6 set to `body`.
fn with_object(body: &str) -> Vec<u8> {
    with_object_bytes(body.as_bytes())
}

fn with_object_bytes(body: &[u8]) -> Vec<u8> {
    let mut pdf = hello(Pdf::new("1.4"), "Edge case", false);
    pdf.obj_bytes(6, body);
    pdf.finish_with_xref("")
}

fn names_body() -> Vec<u8> {
    let mut body = b"[/A#20B /#2f /a#00b /a#zz /a#4g /caf\xe9 /Tab#09 /".to_vec();
    body.extend(std::iter::repeat_n(b'x', 4100));
    body.extend_from_slice(b"]");
    body
}

/// Objects 1-5 plus object 6, a stream with the given dictionary.
fn with_stream(dict: &str, data: &[u8]) -> Vec<u8> {
    let mut pdf = hello(Pdf::new("1.4"), "Stream", false);
    pdf.stream_as_is(6, dict, data);
    pdf.finish_with_xref("")
}

fn indirect_length() -> Vec<u8> {
    let data = b"0 0 m 100 100 l S";
    let mut pdf = hello(Pdf::new("1.4"), "Stream", false);
    pdf.stream_as_is(6, "<</Length 7 0 R>>", data);
    pdf.obj(7, &data.len().to_string());
    pdf.finish_with_xref("")
}

fn nineteen_byte_xref() -> Vec<u8> {
    let mut pdf = hello(Pdf::new("1.4"), "19-byte entries", false);
    let start = pdf.out.len();
    pdf.line("xref");
    pdf.line("0 6");
    pdf.out.extend_from_slice(b"0000000000 65535 f\n");
    for num in 1..6u32 {
        let entry = format!("{:010} 00000 n\n", pdf.offsets[&num]);
        pdf.out.extend_from_slice(entry.as_bytes());
    }
    pdf.trailer("/Root 1 0 R", start);
    pdf.out
}

fn subsection_same_line() -> Vec<u8> {
    let mut pdf = hello(Pdf::new("1.4"), "Same line", false);
    let start = pdf.out.len();
    pdf.line("xref");
    pdf.out.extend_from_slice(b"0 6 0000000000 65535 f\r\n");
    for num in 1..6u32 {
        let entry = format!("{:010} 00000 n\r\n", pdf.offsets[&num]);
        pdf.out.extend_from_slice(entry.as_bytes());
    }
    pdf.trailer("/Root 1 0 R", start);
    pdf.out
}

fn prev_loop() -> Vec<u8> {
    let mut pdf = hello(Pdf::new("1.4"), "Loop", false);
    let nums: Vec<u32> = (1..pdf.size()).collect();
    let start = pdf.xref(&nums, true);
    pdf.trailer(&format!("/Root 1 0 R/Prev {start}"), start);
    pdf.out
}

fn missing_endobj() -> Vec<u8> {
    let mut pdf = hello(Pdf::new("1.4"), "No endobj", false);
    pdf.offsets.insert(6, pdf.out.len());
    pdf.line("6 0 obj");
    pdf.line("<</Note(no endobj follows)>>");
    pdf.obj(7, "<</Next true>>");
    pdf.finish_with_xref("")
}

fn no_header() -> Vec<u8> {
    let body = with_stream("<</Length 5>>", b"0 0 m 100 100 l S");
    let at = body.iter().position(|&b| b == b'\n').map_or(0, |i| i + 1);
    body[at..].to_vec()
}

fn objstm_first_too_small() -> Vec<u8> {
    let mut pdf = Pdf::new("1.5");
    pdf.stream(4, "", &text_content("Object stream"));
    pdf.obj(5, FONT);
    let objects = [
        (1, "<</Type/Catalog/Pages 2 0 R>>"),
        (2, "<</Type/Pages/Kids[3 0 R]/Count 1>>"),
        (
            3,
            "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources<</Font<</F1 5 0 R>>>>/Contents 4 0 R>>",
        ),
    ];
    let mut index = String::new();
    let mut body = String::new();
    for (num, text) in objects {
        let _ = write!(index, "{num} {} ", body.len());
        body.push_str(text);
        body.push('\n');
    }
    let data = format!("{index}{body}");
    pdf.stream(6, "/Type/ObjStm/N 3/First 4", data.as_bytes());
    let xref_offset = pdf.out.len();
    let mut rows = Vec::new();
    let mut row = |kind: u8, field2: u32, field3: u16| {
        rows.push(kind);
        rows.extend_from_slice(&field2.to_be_bytes());
        rows.extend_from_slice(&field3.to_be_bytes());
    };
    row(0, 0, 65535);
    for i in 0..3u16 {
        row(2, 6, i);
    }
    for num in [4, 5, 6] {
        row(1, u32::try_from(pdf.offsets[&num]).unwrap_or(0), 0);
    }
    row(1, u32::try_from(xref_offset).unwrap_or(0), 0);
    pdf.stream(7, "/Type/XRef/Size 8/W[1 4 2]/Root 1 0 R", &rows);
    pdf.line("startxref");
    pdf.line(&xref_offset.to_string());
    pdf.line("%%EOF");
    pdf.out
}

fn wrong_object_at_offset() -> Vec<u8> {
    let mut pdf = hello(Pdf::new("1.4"), "Swapped", false);
    // Point object 5's entry at object 4.
    let four = pdf.offsets[&4];
    pdf.offsets.insert(5, four);
    pdf.finish_with_xref("")
}

/// Catalog 1, page tree root 2 (`root`), page 3 (with `page_extra`), plus
/// `extra` objects.
fn page_tree(root: &str, extra: &[(u32, &str)], page_extra: &str) -> Vec<u8> {
    let mut pdf = Pdf::new("1.4");
    pdf.obj(1, "<</Type/Catalog/Pages 2 0 R>>");
    pdf.obj(2, root);
    let parent = if page_extra.is_empty() {
        "/Parent 2 0 R"
    } else {
        page_extra
    };
    pdf.obj(3, &format!("<</Type/Page{parent}>>"));
    for (num, body) in extra {
        pdf.obj(*num, body);
    }
    pdf.finish_with_xref("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn committed_fixtures_match_generators() {
        let root = crate::repo_root();
        run(&root, true).unwrap();
    }

    #[test]
    #[ignore = "needs the oracle: cargo xtask oracle build"]
    fn repair_expectations_match_mupdf() {
        let root = crate::repo_root();
        let mutool = crate::oracle::mutool(&root).unwrap();
        for f in all() {
            let path = root.join("corpus/smoke").join(f.name);
            let run = crate::proc::run(
                &mutool,
                &[std::ffi::OsStr::new("info"), path.as_os_str()],
                std::time::Duration::from_secs(30),
            )
            .unwrap();
            let stderr = String::from_utf8_lossy(&run.stderr);
            let repaired = stderr.contains("repairing PDF document");
            assert_eq!(repaired, f.needs_repair, "{}: {stderr}", f.name);
        }
    }

    #[test]
    fn names_are_unique_pdfs() {
        let fixtures = all();
        let mut names: Vec<_> = fixtures.iter().map(|f| f.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), fixtures.len());
        assert!(names.iter().all(|n| n.ends_with(".pdf")));
    }

    #[test]
    fn recorded_offsets_point_at_objects() {
        let pdf = hello(Pdf::with_eol("1.4", "\r\n"), "x", true);
        for (num, &offset) in &pdf.offsets {
            assert!(pdf.out[offset..].starts_with(format!("{num} 0 obj").as_bytes()));
        }
    }

    #[test]
    fn xref_entries_are_twenty_bytes_for_every_eol() {
        for eol in ["\n", "\r", "\r\n"] {
            let mut pdf = hello(Pdf::with_eol("1.4", eol), "x", false);
            let start = pdf.xref(&[1, 2, 3, 4, 5], true);
            let table = &pdf.out[start..];
            let header_len = format!("xref{eol}0 6{eol}").len();
            assert_eq!(table.len(), header_len + 6 * 20, "eol {eol:?}");
        }
    }

    #[test]
    fn xref_stream_rows_resolve_to_objects() {
        let bytes = xref_stream();
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("/Type/XRef"));
        assert!(text.contains("/Type/ObjStm/N 3"));
    }

    #[test]
    fn incremental_update_links_revisions() {
        let text = String::from_utf8_lossy(&incremental_update()).to_string();
        assert_eq!(text.matches("%%EOF").count(), 2);
        assert!(text.contains("/Prev "));
    }
}
