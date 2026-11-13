// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! Small PDFs built in tests and rendered as `mutool draw` would, so the
//! rasterizer is tested through real pages.
//!
//! Expected pixels come from the oracle. With `NUPDF_DRAW_DUMP=<dir>`,
//! every page a test renders is written to `<dir>/<name>.pdf` with
//! nupdf's MD5 of its pixels beside it (`<name>.md5`), so
//! `mutool draw -c rgb -s5 -o /dev/null -F pnm <dir>/<name>.pdf` (`-N` for the
//! names ending `-noicc`) gives MuPDF's MD5 to compare.

use crate::content::Store;
use crate::document::Document;
use crate::geometry::Matrix;

use super::Pix;

/// A PDF of `objects`, numbered from 1, the first the catalog.
pub fn pdf_of(objects: &[&str]) -> Vec<u8> {
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (i, o) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
    }
    let xref = out.len();
    let n = objects.len() + 1;
    out.extend_from_slice(format!("xref\n0 {n}\n0000000000 65535 f \n").as_bytes());
    for o in offsets {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!("trailer\n<</Size {n}/Root 1 0 R>>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
    );
    out
}

/// A stream object with `dict` (without /Length) around `data`.
pub fn stream(dict: &str, data: &str) -> String {
    format!(
        "<<{dict}/Length {}>>stream\n{data}\nendstream",
        data.len() + 1
    )
}

/// A stream object holding `data` hex encoded.
pub fn hex_stream(dict: &str, data: &[u8]) -> String {
    let mut hex: String = data.iter().map(|b| format!("{b:02x}")).collect();
    hex.push('>');
    stream(&format!("{dict}/Filter/ASCIIHexDecode"), &hex)
}

/// A one-page PDF: the page `w` by `h` points with `resources` and
/// `content`; `extra` are objects 5 onwards.
pub fn page(w: u32, h: u32, resources: &str, content: &str, extra: &[&str]) -> Vec<u8> {
    let page = format!(
        "<</Type/Page/Parent 2 0 R/MediaBox[0 0 {w} {h}]/Resources<<{resources}>>/Contents 4 0 R>>"
    );
    let contents = stream("", content);
    let mut objs = vec![
        "<</Type/Catalog/Pages 2 0 R>>",
        "<</Type/Pages/Kids[3 0 R]/Count 1>>",
        page.as_str(),
        contents.as_str(),
    ];
    objs.extend_from_slice(extra);
    pdf_of(&objs)
}

/// `pdf`'s first page rendered at `zoom` (1 is 72 dpi), with ICC color
/// management as `mutool draw` has it by default, or without (`-N`).
pub fn render(name: &str, pdf: &[u8], zoom: f32, icc: bool) -> Pix {
    let doc = Document::open(pdf.to_vec()).unwrap_or_else(|e| panic!("{name}: {e:?}"));
    let page = doc.page(0).unwrap_or_else(|e| panic!("{name}: {e:?}"));
    let ctm = Matrix::scale(zoom, zoom);
    let (out, _) = super::render_page_with(
        &doc,
        &page,
        ctm,
        1 << 26,
        None,
        Store::default(),
        icc,
        super::Parts::CONTENTS,
    );
    let out = out.unwrap_or_else(|e| panic!("{name}: {e}"));
    if let Some(e) = &out.error {
        panic!("{name}: {e}");
    }
    if let Some(dir) = std::env::var_os("NUPDF_DRAW_DUMP") {
        let dir = std::path::PathBuf::from(dir);
        let name = if icc {
            name.to_string()
        } else {
            format!("{name}-noicc")
        };
        let _ = std::fs::write(dir.join(format!("{name}.pdf")), pdf);
        let _ = std::fs::write(dir.join(format!("{name}.md5")), md5(&out.pix));
        let _ = std::fs::write(dir.join(format!("{name}.raw")), &out.pix.samples);
    }
    out.pix
}

/// `fz_md5_pixmap` as `mutool draw -s5` prints it.
pub fn md5(pix: &Pix) -> String {
    use md5::{Digest, Md5};
    Md5::digest(&pix.samples)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The pixel at (`x`, `y`) from the pixmap's origin.
pub fn px(p: &Pix, x: i32, y: i32) -> Vec<u8> {
    let i = (y as usize * p.w as usize + x as usize) * p.n;
    p.samples[i..i + p.n].to_vec()
}

/// `(value, bits)` fields packed most significant bit first, the last
/// byte padded with zeros: the data of a mesh shading.
pub fn bits(fields: &[(u32, u32)]) -> Vec<u8> {
    let mut out = Vec::new();
    let (mut acc, mut n) = (0u64, 0u32);
    for &(v, w) in fields {
        acc = (acc << w) | (u64::from(v) & ((1u64 << w) - 1));
        n += w;
        while n >= 8 {
            n -= 8;
            out.push((acc >> n) as u8);
        }
    }
    if n > 0 {
        out.push((acc << (8 - n)) as u8);
    }
    out
}

/// Renders `pdf` and checks its pixels against MuPDF's MD5 (from
/// `mutool draw -s5`, with `-N` when not `icc`).
pub fn check(name: &str, pdf: &[u8], icc: bool, want: &str) -> Pix {
    let pix = render(name, pdf, 1.0, icc);
    // Dumping is for finding MuPDF's MD5s, which the dump is compared to.
    if std::env::var_os("NUPDF_DRAW_DUMP").is_none() {
        assert_eq!(md5(&pix), want, "{name}: pixels differ from mutool draw's");
    }
    pix
}

/// A 128-point page with something of everything a page can draw:
/// paths, text in every render mode (stroked in a Type 3 font, whose
/// glyphs MuPDF does not stroke with FreeType), a
/// shading, an image, stencils (in a color and in a pattern), tiling
/// patterns (one used twice), a soft mask, a knockout group, a blend
/// mode, a layer, ActualText and clips.
pub fn sink() -> Vec<u8> {
    sink_with("")
}

/// [`sink`] with `more` drawn last.
pub fn sink_with(more: &str) -> Vec<u8> {
    let image = hex_stream(
        "/Type/XObject/Subtype/Image/Width 4/Height 4/ColorSpace/DeviceRGB/BitsPerComponent 8",
        &(0..48u32).map(|i| (i * 37 % 256) as u8).collect::<Vec<_>>(),
    );
    let stencil = hex_stream(
        "/Type/XObject/Subtype/Image/Width 8/Height 8/ImageMask true/BitsPerComponent 1",
        &[0x18, 0x3C, 0x7E, 0xFF, 0xFF, 0x7E, 0x3C, 0x18],
    );
    let objs = [
        "<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>".to_string(),
        "<</Type/Font/Subtype/Type3/FontBBox[0 0 1000 1000]/FontMatrix[0.001 0 0 0.001 0 0]/CharProcs<</a 7 0 R/b 8 0 R>>/Encoding<</Differences[97/a/b]>>/FirstChar 97/LastChar 98/Widths[1000 1000]/Resources<<>>>>".to_string(),
        stream("", "1000 0 0 0 1000 1000 d1 0 0 1000 1000 re f"),
        stream(
            "",
            "1000 0 d0 1 0 0 rg 100 100 800 800 re f 0 0 1 RG 50 w 0 0 m 1000 1000 l S",
        ),
        image,
        stencil,
        "<</ShadingType 2/ColorSpace/DeviceRGB/Coords[0 0 128 0]/Extend[true true]/Function<</FunctionType 2/Domain[0 1]/C0[1 1 0]/C1[0 0.5 1]/N 1>>>>".to_string(),
        stream(
            "/PatternType 1/PaintType 1/TilingType 1/BBox[0 0 8 8]/XStep 8/YStep 8/Resources<<>>",
            "1 0 0 rg 0 0 4 4 re f 0 0 1 rg 4 4 4 4 re f",
        ),
        stream(
            "/PatternType 1/PaintType 2/TilingType 2/BBox[0 0 6 6]/XStep 6/YStep 7/Matrix[1 0.3 0 1 0 0]/Resources<<>>",
            "0 0 3 3 re f",
        ),
        "<</SMask<</S/Luminosity/G 15 0 R>>>>".to_string(),
        stream(
            "/Type/XObject/Subtype/Form/BBox[0 0 128 128]/Group<</S/Transparency/CS/DeviceGray>>",
            "1 g 40 76 20 20 re f 0.5 g 60 76 20 20 re f",
        ),
        stream(
            "/Type/XObject/Subtype/Form/BBox[0 0 128 128]/Group<</S/Transparency/K true>>/Resources<<>>",
            "1 0 0 rg 4 100 30 10 re f 0 1 0 rg 20 104 30 10 re f",
        ),
        "<</Type/OCG/Name(L)>>".to_string(),
    ];
    let res = "/Font<</F1 5 0 R/F3 6 0 R>>/XObject<</Im 9 0 R/St 10 0 R/Fm 16 0 R>>/Shading<</S 11 0 R>>/Pattern<</Sh<</PatternType 2/Shading 11 0 R>>/T1 12 0 R/T2 13 0 R>>/ExtGState<</M 14 0 R/B<</BM/Multiply/ca 0.5>>/Hs<</CA 0.5>>>>/ColorSpace<</Cs1[/Pattern/DeviceRGB]>>/Properties<</oc1 17 0 R>>";
    let content = "q 1 0 0 rg 4 4 30 20 re f 0 0 1 RG 2 w 4 4 30 20 re S Q
        BT /F1 14 Tf 4 30 Td (Hello) Tj 3 Tr (inv) Tj /F3 1 Tf 8 0 0 8 50 30 Tm 1 Tr 0 1 0 RG (a) Tj 2 Tr (b) Tj ET
        q BT /F1 20 Tf 7 Tr 40 4 Td (Clip) Tj ET /S sh Q
        q BT /F1 20 Tf 4 Tr 40 26 Td (y) Tj /F3 1 Tf 12 0 0 12 60 26 Tm 5 Tr (a) Tj 6 Tr (b) Tj ET 0 0 1 rg 0 0 128 128 re f Q
        q BT /F3 1 Tf 12 0 0 12 90 4 Tm (abab) Tj 1 Tr (a) Tj ET Q
        q 20 0 0 20 4 50 cm /Im Do Q
        q 0.2 0.6 0.2 rg 20 0 0 20 28 50 cm /St Do Q
        q /Pattern cs /Sh scn 20 0 0 20 52 50 cm /St Do Q
        q /Pattern cs /T1 scn 76 50 20 20 re f /T1 scn 100 50 20 20 re f Q
        q /Cs1 cs 1 0 0 /T2 scn 4 76 30 20 re f Q
        q /M gs 0 0 1 rg 40 76 40 20 re f Q
        q /B gs /Fm Do Q
        /OC /oc1 BDC 0.5 g 84 76 20 20 re f EMC
        /Span <</ActualText (X)>> BDC 0.3 g 108 76 16 20 re f EMC
        q /Pattern CS /Sh SCN 3 w 100 30 m 124 44 l S BT /F3 1 Tf 14 0 0 14 100 30 Tm 1 Tr (a) Tj ET Q
        q /Pattern CS /T1 SCN /Hs gs 4 w 100 4 m 124 20 l S Q
        q 60 100 64 24 re W n 0.9 0.5 0.1 rg 0 0 128 128 re f Q";
    let extra: Vec<&str> = objs.iter().map(String::as_str).collect();
    page(128, 128, res, &format!("{content}\n{more}"), &extra)
}
