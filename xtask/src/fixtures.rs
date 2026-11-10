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
    .collect()
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
