// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! The adversarial redaction corpus: one small PDF per way a redacted
//! secret leaks, each hiding the same secret where a careless redaction
//! leaves it. `cargo xtask fixtures` writes them to `corpus/redaction`;
//! `cargo xtask redaction` redacts each and judges the output with
//! `nupdf verify`, MuPDF and pdftotext.

use std::collections::BTreeMap;
use std::fmt::Write as _;

/// What every fixture hides.
pub const SECRET: &str = "SECRET-4711";

pub struct Fixture {
    pub name: &'static str,
    /// The leak class `nupdf verify` must report on the input.
    pub class: &'static str,
    pub about: &'static str,
    pub bytes: Vec<u8>,
}

/// A PDF from object bodies, numbered from 1, with an exact xref table.
struct Doc {
    objs: Vec<Vec<u8>>,
}

impl Doc {
    fn new() -> Doc {
        Doc { objs: Vec::new() }
    }

    /// Adds an object, returning its number.
    fn obj(&mut self, body: &str) -> usize {
        self.objs.push(body.as_bytes().to_vec());
        self.objs.len()
    }

    fn stream(&mut self, dict: &str, data: &[u8]) -> usize {
        let mut body = format!("<<{dict}/Length {}>>\nstream\n", data.len()).into_bytes();
        body.extend_from_slice(data);
        body.extend_from_slice(b"\nendstream");
        self.objs.push(body);
        self.objs.len()
    }

    fn finish(self, trailer: &str) -> Vec<u8> {
        let mut out = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n".to_vec();
        let mut offsets = Vec::new();
        for (i, body) in self.objs.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
            out.extend_from_slice(body);
            out.extend_from_slice(b"\nendobj\n");
        }
        let xref = out.len();
        out.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", self.objs.len() + 1).as_bytes(),
        );
        for o in offsets {
            out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!(
                "trailer\n<</Size {}{trailer}>>\nstartxref\n{xref}\n%%EOF\n",
                self.objs.len() + 1
            )
            .as_bytes(),
        );
        out
    }
}

const FONT: &str = "<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>";

/// Catalog (1), pages (2), one page (3) with `page_extra` in its
/// dictionary and `resources` in its resources, its contents (4) and font
/// (5). Returns the document, for more objects.
fn one_page(content: &str, page_extra: &str, resources: &str, catalog_extra: &str) -> Doc {
    let mut d = Doc::new();
    d.obj(&format!("<</Type/Catalog/Pages 2 0 R{catalog_extra}>>"));
    d.obj("<</Type/Pages/Kids[3 0 R]/Count 1>>");
    d.obj(&format!(
        "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources<</Font<</F1 5 0 R>>{resources}>>/Contents 4 0 R{page_extra}>>"
    ));
    d.stream("", content.as_bytes());
    d.obj(FONT);
    d
}

fn line(y: i32, text: &str) -> String {
    format!("BT /F1 12 Tf 72 {y} Td ({text}) Tj ET\n")
}

/// A 4x4 grey image: an image XObject, as a stream body.
fn image(d: &mut Doc) -> usize {
    d.stream(
        "/Type/XObject/Subtype/Image/Width 4/Height 4/BitsPerComponent 8/ColorSpace/DeviceGray",
        &[0x40; 16],
    )
}

pub fn all() -> Vec<Fixture> {
    let s = SECRET;
    let decoy = line(600, "Nothing to see here.");
    let mut out = Vec::new();
    let mut add = |name, class, about, bytes| {
        out.push(Fixture {
            name,
            class,
            about,
            bytes,
        })
    };

    add(
        "content-text.pdf",
        "content_text",
        "The secret in plain visible text",
        one_page(
            &format!("{}{decoy}", line(700, &format!("Account {s} closed"))),
            "",
            "",
            "",
        )
        .finish("/Root 1 0 R"),
    );
    add(
        "invisible-text.pdf",
        "content_text",
        "The secret in render mode 3 (invisible) text, as OCR layers are",
        one_page(
            &format!("BT 3 Tr /F1 12 Tf 72 700 Td ({s}) Tj ET\n{decoy}"),
            "",
            "",
            "",
        )
        .finish("/Root 1 0 R"),
    );
    add(
        "white-on-white.pdf",
        "content_text",
        "The secret in white text on the white page",
        one_page(&format!("1 1 1 rg {}0 g {decoy}", line(700, s)), "", "", "")
            .finish("/Root 1 0 R"),
    );
    add(
        "tiny-text.pdf",
        "content_text",
        "The secret at half a point",
        one_page(
            &format!("BT /F1 .5 Tf 72 700 Td ({s}) Tj ET\n{decoy}"),
            "",
            "",
            "",
        )
        .finish("/Root 1 0 R"),
    );
    add(
        "clipped-text.pdf",
        "content_text",
        "The secret outside its clip path, so nothing of it shows",
        one_page(
            &format!("q 0 0 10 10 re W n {}Q {decoy}", line(700, s)),
            "",
            "",
            "",
        )
        .finish("/Root 1 0 R"),
    );
    add(
        "split-text.pdf",
        "content_text",
        "The secret split across a TJ array with kerning, and across two Tj",
        one_page(
            "BT /F1 12 Tf 72 700 Td [(SEC) -20 (RET-) 30 (4711)] TJ ET\nBT /F1 12 Tf 72 680 Td (SECRET) Tj (-4711) Tj ET\n",
            "",
            "",
            "",
        )
        .finish("/Root 1 0 R"),
    );
    {
        let mut d = one_page(
            &format!(
                "{}q 1 0 0 1 0 -100 cm /Fm1 Do Q /Fm1 Do\n",
                line(500, "Form follows")
            ),
            "",
            "/XObject<</Fm1 6 0 R>>",
            "",
        );
        d.stream(
            "/Type/XObject/Subtype/Form/BBox[0 0 612 792]/Resources<</Font<</F1 5 0 R>>>>",
            line(700, s).as_bytes(),
        );
        add(
            "form-xobject.pdf",
            "content_text",
            "The secret in a form drawn twice",
            d.finish("/Root 1 0 R"),
        );
    }
    {
        let mut d = one_page(
            &format!("{}q 200 0 0 40 60 690 cm /Im1 Do Q\n", line(700, s)),
            "",
            "/XObject<</Im1 6 0 R>>",
            "",
        );
        image(&mut d);
        add(
            "under-image.pdf",
            "content_text",
            "The secret under an image drawn over it",
            d.finish("/Root 1 0 R"),
        );
    }
    {
        let mut d = one_page(
            &format!("q 300 0 0 100 50 650 cm /Im1 Do Q\n{}", line(700, s)),
            "",
            "/XObject<</Im1 6 0 R>>",
            "",
        );
        image(&mut d);
        add(
            "over-image.pdf",
            "images",
            "The secret drawn over an image, whose pixels under it must be blanked",
            d.finish("/Root 1 0 R"),
        );
    }
    add(
        "actualtext.pdf",
        "structure",
        "The secret as the ActualText of glyphs that show something else",
        one_page(
            &format!("/Span <</ActualText ({s})>> BDC BT /F1 12 Tf 72 700 Td (XXXXXXXXXXX) Tj ET EMC\n{decoy}"),
            "",
            "",
            "",
        )
        .finish("/Root 1 0 R"),
    );
    {
        let mut d = one_page(
            &format!(
                "/P <</MCID 0>> BDC {}EMC\n{decoy}",
                line(700, "Hidden in the tree")
            ),
            "/StructParents 0",
            "",
            "/StructTreeRoot 6 0 R/MarkInfo<</Marked true>>",
        );
        d.obj("<</Type/StructTreeRoot/K 7 0 R/ParentTree 8 0 R>>");
        d.obj(&format!(
            "<</Type/StructElem/S/Figure/P 6 0 R/Pg 3 0 R/K 0/Alt ({s})/E ({s})>>"
        ));
        d.obj("<</Nums[0[7 0 R]]>>");
        add(
            "structure-alt.pdf",
            "structure",
            "The secret in a structure element's /Alt and /E",
            d.finish("/Root 1 0 R"),
        );
    }
    {
        let mut d = one_page(&decoy, "/Annots[6 0 R 7 0 R]", "", "");
        d.obj(&format!(
            "<</Type/Annot/Subtype/Text/Rect[500 700 520 720]/Contents(Note: {s})/T(Reviewer)>>"
        ));
        d.obj("<</Type/Annot/Subtype/FreeText/Rect[300 300 500 330]/DA(/Helv 12 Tf 0 g)/Contents(see note)/AP<</N 8 0 R>>>>");
        d.stream(
            "/Type/XObject/Subtype/Form/BBox[300 300 500 330]/Resources<</Font<</Helv 5 0 R>>>>",
            format!("BT /Helv 12 Tf 305 310 Td ({s}) Tj ET").as_bytes(),
        );
        add(
            "annotations.pdf",
            "annotations",
            "The secret in a note's /Contents and in a FreeText appearance, away from the page text",
            d.finish("/Root 1 0 R"),
        );
    }
    {
        let mut d = one_page(
            &decoy,
            "/Annots[6 0 R]",
            "",
            "/AcroForm<</Fields[6 0 R]/DA(/Helv 0 Tf 0 g)>>",
        );
        d.obj(&format!(
            "<</Type/Annot/Subtype/Widget/FT/Tx/T(account)/V({s})/Rect[72 400 300 420]/P 3 0 R/AP<</N 7 0 R>>>>"
        ));
        d.stream(
            "/Type/XObject/Subtype/Form/BBox[0 0 228 20]/Resources<</Font<</Helv 5 0 R>>>>",
            format!("/Tx BMC BT /Helv 12 Tf 2 5 Td ({s}) Tj ET EMC").as_bytes(),
        );
        add(
            "form-field.pdf",
            "annotations",
            "The secret as a text field's value and appearance",
            d.finish("/Root 1 0 R"),
        );
    }
    {
        let mut d = one_page(&decoy, "", "", "/Metadata 6 0 R");
        d.stream(
            "/Type/Metadata/Subtype/XML",
            format!("<x:xmpmeta xmlns:x='adobe:ns:meta/'><dc:title>{s}</dc:title></x:xmpmeta>")
                .as_bytes(),
        );
        d.obj(&format!("<</Title({s} report)/Author(Ops)>>"));
        add(
            "metadata.pdf",
            "metadata",
            "The secret in /Info and in the XMP packet",
            d.finish("/Root 1 0 R/Info 7 0 R"),
        );
    }
    {
        let mut d = one_page(&decoy, "", "", "/Outlines 6 0 R/Names<</Dests 8 0 R>>");
        d.obj("<</Type/Outlines/First 7 0 R/Last 7 0 R/Count 1>>");
        d.obj(&format!(
            "<</Title({s} chapter)/Parent 6 0 R/Dest[3 0 R/Fit]>>"
        ));
        d.obj(&format!("<</Names[({s})[3 0 R/Fit]]>>"));
        add(
            "outline.pdf",
            "outline",
            "The secret in an outline title and a named destination",
            d.finish("/Root 1 0 R"),
        );
    }
    {
        let mut d = one_page(&decoy, "", "", "/Names<</EmbeddedFiles 6 0 R>>");
        d.obj(&format!("<</Names[({s}.txt) 7 0 R]>>"));
        d.obj(&format!(
            "<</Type/Filespec/F({s}.txt)/UF({s}.txt)/EF<</F 8 0 R>>/Desc(attachment)>>"
        ));
        d.stream(
            "/Type/EmbeddedFile",
            format!("the code is {s}\n").as_bytes(),
        );
        add(
            "embedded-file.pdf",
            "embedded_files",
            "The secret as an attachment's name and contents",
            d.finish("/Root 1 0 R"),
        );
    }
    {
        let mut d = one_page(
            &format!("/OC /oc1 BDC {}EMC\n{decoy}", line(700, s)),
            "",
            "/Properties<</oc1 6 0 R>>",
            "/OCProperties<</OCGs[6 0 R]/D<</OFF[6 0 R]>>>>",
        );
        d.obj("<</Type/OCG/Name(Draft notes)>>");
        add(
            "hidden-layer.pdf",
            "hidden_content",
            "The secret in a layer that is off",
            d.finish("/Root 1 0 R"),
        );
    }
    {
        let mut d = one_page(&decoy, "", "", "/OCProperties<</OCGs[6 0 R]/D<<>>>>");
        d.obj(&format!("<</Type/OCG/Name({s})>>"));
        add(
            "layer-name.pdf",
            "hidden_content",
            "The secret as a layer's name",
            d.finish("/Root 1 0 R"),
        );
    }
    {
        let mut d = one_page(
            &decoy,
            "",
            "",
            "/Names<</JavaScript 6 0 R>>/OpenAction 8 0 R",
        );
        d.obj("<</Names[(init) 7 0 R]>>");
        d.obj(&format!("<</S/JavaScript/JS(var code = '{s}';)>>"));
        d.obj(&format!("<</S/JavaScript/JS(app.alert('{s}');)>>"));
        add(
            "javascript.pdf",
            "javascript",
            "The secret in document JavaScript and the open action",
            d.finish("/Root 1 0 R"),
        );
    }
    {
        let d = one_page(
            &decoy,
            "",
            "",
            &format!("/PageLabels<</Nums[0<</S/D/P({s}-)>>]>>"),
        );
        add(
            "page-labels.pdf",
            "outline",
            "The secret as a page label prefix",
            d.finish("/Root 1 0 R"),
        );
    }
    {
        let mut d = one_page(&line(700, s), "/Annots[6 0 R 7 0 R]", "", "");
        d.obj(&format!(
            "<</Type/Annot/Subtype/Link/Rect[70 695 160 712]/A<</S/URI/URI(https://example.com/?q={s})>>>>"
        ));
        d.obj(&format!(
            "<</Type/Annot/Subtype/Link/Rect[300 300 400 320]/A<</S/URI/URI(https://example.com/{s})>>>>"
        ));
        add(
            "links.pdf",
            "annotations",
            "The secret in link URIs, one over the secret's text and one elsewhere",
            d.finish("/Root 1 0 R"),
        );
    }
    {
        let mut d = one_page(&line(700, s), "/Thumb 6 0 R", "", "");
        image(&mut d);
        add(
            "thumbnail.pdf",
            "images",
            "The secret on a page with a thumbnail image of it",
            d.finish("/Root 1 0 R"),
        );
    }
    out.push(history());
    out
}

/// Two revisions: the first shows the secret, the second replaces the
/// contents and leaves an object holding the secret that nothing reaches.
fn history() -> Fixture {
    let s = SECRET;
    let first = one_page(&line(700, s), "", "", "").finish("/Root 1 0 R");
    let mut out = first.clone();
    let content = line(700, "Revised.");
    let objs = [
        format!(
            "4 0 obj\n<</Length {}>>\nstream\n{content}\nendstream\nendobj\n",
            content.len()
        ),
        format!("6 0 obj\n({s})\nendobj\n"),
    ];
    let mut offsets = Vec::new();
    for o in &objs {
        offsets.push(out.len());
        out.extend_from_slice(o.as_bytes());
    }
    let xref = out.len();
    let prev = String::from_utf8_lossy(&first)
        .rsplit("startxref\n")
        .next()
        .and_then(|t| t.lines().next())
        .and_then(|n| n.parse::<usize>().ok())
        .unwrap_or(0);
    out.extend_from_slice(
        format!(
            "xref\n4 1\n{:010} 00000 n \n6 1\n{:010} 00000 n \ntrailer\n<</Size 7/Root 1 0 R/Prev {prev}>>\nstartxref\n{xref}\n%%EOF\n",
            offsets[0], offsets[1]
        )
        .as_bytes(),
    );
    Fixture {
        name: "history.pdf",
        class: "history",
        about: "The secret in an earlier revision and in an object nothing reaches",
        bytes: out,
    }
}

/// `corpus/redaction/README.md`.
pub fn readme(fixtures: &[Fixture]) -> String {
    let mut out = format!(
        "# Adversarial redaction corpus\n\n\
         Generated by `cargo xtask fixtures` (xtask/src/adversarial.rs). Do not edit by hand.\n\
         Each file hides `{SECRET}` where a careless redaction leaves it; \
         `cargo xtask redaction` redacts it and checks it is gone.\n\n\
         | File | Leak class | Hides |\n|---|---|---|\n"
    );
    for f in fixtures {
        let _ = writeln!(out, "| `{}` | {} | {} |", f.name, f.class, f.about);
    }
    out
}

/// The files to write, by name.
pub fn files() -> BTreeMap<String, Vec<u8>> {
    let fixtures = all();
    let mut out: BTreeMap<String, Vec<u8>> = fixtures
        .iter()
        .map(|f| (f.name.to_string(), f.bytes.clone()))
        .collect();
    out.insert("README.md".into(), readme(&fixtures).into_bytes());
    out
}
