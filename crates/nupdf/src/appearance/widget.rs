// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-appearance.c
// (pdf_write_widget_appearance, pdf_write_tx_widget_appearance,
// pdf_write_ch_widget_appearance, pdf_write_sig_widget_appearance,
// pdf_update_button_appearance, draw_push_button, draw_check_button,
// draw_radio_button), source/pdf/pdf-annot.c (pdf_annot_MK_BG,
// pdf_annot_MK_BC and their RGB forms) and source/pdf/pdf-form.c
// (pdf_button_field_on_state), Copyright (C) 2004-2026 Artifex Software, Inc.

//! Form widgets' appearances: text fields, combo and list boxes and
//! signature fields as one stream, written with the free text layout;
//! push buttons, check boxes and radio buttons as a stream per state.

use super::text::fonts::{self, LANG_UNSET};
use super::text::layout::{self, TextBox};
use super::text::{rotate, text_needs_rich_layout};
use super::{Writer, len};
use crate::document::Document;
use crate::fma::Madd;
use crate::form;
use crate::geometry::{Matrix, Rect};
use crate::object::{Dict, Object};

pub const TX_MULTILINE: i32 = 1 << 12;
pub const TX_PASSWORD: i32 = 1 << 13;
pub const TX_COMB: i32 = 1 << 24;

const CIRCLE_MAGIC: f32 = 0.551_915;

/// `pdf_annot_color_imp`: none, gray, RGB or CMYK by the array's length.
fn color_of(doc: &Document, obj: &Object) -> Vec<f32> {
    let a = doc.resolve(obj);
    let a = a.as_array().unwrap_or(&[]);
    let n = match a.len() {
        0 => 0,
        1 | 2 => 1,
        3 => 3,
        _ => 4,
    };
    (0..n)
        .map(|i| a.get(i).map_or(0.0, |v| doc.resolve(v).to_real()))
        .collect()
}

impl Writer<'_> {
    /// `pdf_annot_MK_BG` or `pdf_annot_MK_BC`.
    fn mk_color(&self, key: &[u8]) -> Vec<f32> {
        let mk = self.get(b"MK");
        color_of(self.doc, &self.doc.dict_get(&mk, key))
    }

    /// `pdf_annot_MK_BG_rgb` or `pdf_annot_MK_BC_rgb`: the colour as RGB,
    /// None when there is none.
    fn mk_rgb(&self, key: &[u8]) -> Option<[f32; 3]> {
        let c = self.mk_color(key);
        match c.len() {
            1 => Some([c[0]; 3]),
            3 => Some([c[0], c[1], c[2]]),
            4 => Some([
                1.0 - 1f32.min(c[0] + c[3]),
                1.0 - 1f32.min(c[1] + c[3]),
                1.0 - 1f32.min(c[2] + c[3]),
            ]),
            _ => None,
        }
    }

    /// `pdf_write_MK_BG_appearance`.
    fn mk_bg(&mut self) -> bool {
        let c = self.mk_color(b"BG");
        self.color(&c, false)
    }

    /// `pdf_write_MK_BC_appearance`.
    fn mk_bc(&mut self) -> bool {
        let c = self.mk_color(b"BC");
        self.color(&c, true)
    }

    /// `/MK /R` as `pdf_dict_get_int` reads it, less whole turns.
    fn mk_rotation(&self) -> i32 {
        let mk = self.get(b"MK");
        self.doc.dict_get(&mk, b"R").to_int() % 360
    }

    /// `pdf_annot_quadding`: /Q of the widget itself, 0 when out of range.
    fn quadding(&self) -> i32 {
        match self.get(b"Q").to_int() {
            q @ 0..=2 => q,
            _ => 0,
        }
    }
}

/// The box a widget is drawn in, and the rotation it is drawn with: /Rect's
/// size, turned with /MK /R.
fn widget_box(w: &Writer, rect: Rect) -> (f32, f32, Matrix) {
    let r = w.mk_rotation();
    let (mut bw, mut bh) = (rect.x1 - rect.x0, rect.y1 - rect.y0);
    if r == 90 || r == 270 {
        std::mem::swap(&mut bw, &mut bh);
    }
    (bw, bh, rotate(r as f32))
}

/// `pdf_write_tx_widget_appearance`: `text` in the field's box, on its
/// /MK background and border, as one line, a comb or many lines by `ff`.
/// Text MuPDF lays out with its HTML engine (/RV, /DS, or characters no
/// base-14 or CJK font has) is not supported. Password fields are drawn
/// in clear, as MuPDF draws them.
fn tx(
    w: &mut Writer,
    rect: Rect,
    matrix: &mut Matrix,
    text: &str,
    ff: i32,
    field: &Object,
) -> Result<Rect, String> {
    let doc = w.doc;
    let q = w.quadding();
    let da = fonts::default_appearance(doc, field)?;
    let lang = fonts::language(doc, field)?;
    let (bw, bh, m) = widget_box(w, rect);
    *matrix = m;
    let bbox = Rect::new(0.0, 0.0, bw, bh);

    w.s("/Tx BMC\nq\n");
    if w.mk_bg() {
        w.s("0 0 ");
        w.op(&[bw, bh], "re\nf");
    }
    let b = w.border()?;
    let mut has_bc = false;
    if b > 0.0 && w.mk_bc() {
        w.op(&[b / 2.0, b / 2.0, bw - b, bh - b], "re\ns");
        has_bc = true;
    }
    w.op(&[b, b, bw - b * 2.0, bh - b * 2.0], "re\nW\nn");

    let opt_string = |key: &[u8]| matches!(w.get(key), Object::String(_));
    if opt_string(b"DS") || opt_string(b"RV") || text_needs_rich_layout(text) {
        return Err("rich text appearances are not supported".into());
    }

    let color = da.color();
    let font = da.mapped_font();
    let mut tb = TextBox {
        size: da.size,
        q,
        w: bw,
        h: bh,
        padding: b * 2.0,
        baseline: 0.8,
        lineheight: 1.2,
        multiline: false,
        comb: 0,
        adjust_baseline: false,
    };
    if ff & TX_MULTILINE != 0 {
        tb.baseline = 1.116;
        tb.lineheight = 1.116;
        tb.multiline = true;
        tb.adjust_baseline = true;
    } else if ff & TX_COMB != 0 {
        let max_len =
            fonts::inheritable(doc, field, b"MaxLen")?.map_or(0, |v| doc.resolve(&v).to_int());
        if has_bc && max_len > 1 {
            let cell_w = (bw - 2.0 * b) / max_len as f32;
            for i in 1..max_len {
                let x = cell_w.madd(i as f32, b);
                w.op(&[x, b], "m");
                w.buf.pop();
                w.buf.push(b' ');
                w.op(&[x, bh - b], "l s");
            }
        }
        tb.padding = 0.0;
        tb.comb = max_len;
    }
    layout::write_variable_text(w, lang, text, font, color, &tb)?;
    w.s("Q\nEMC\n");
    Ok(bbox)
}

/// `pdf_write_ch_widget_appearance`: a combo box as a one-line text field
/// with its value; a list box as its options, from /TI on, one a line.
fn ch(w: &mut Writer, rect: Rect, matrix: &mut Matrix, field: &Object) -> Result<Rect, String> {
    let doc = w.doc;
    if form::flags(doc, field) & form::CH_COMBO != 0 {
        let value = form::value(doc, field);
        return tx(w, rect, matrix, &value, 0, field);
    }
    let opt = w.get(b"Opt");
    let first = w.get(b"TI").to_int().max(0) as usize;
    let text_of = |o: &Object| match doc.resolve(o) {
        Object::String(s) => crate::text_string::text_string(&s),
        _ => String::new(),
    };
    let mut text = String::new();
    let items = doc.resolve(&opt);
    for i in first..len(doc, &opt) {
        let val = items.as_array().and_then(|a| a.get(i)).cloned();
        let val = val.map(|v| doc.resolve(&v)).unwrap_or(Object::Null);
        match &val {
            Object::Array(pair) => text.push_str(&pair.get(1).map(text_of).unwrap_or_default()),
            v => text.push_str(&text_of(v)),
        }
        text.push('\n');
    }
    tx(w, rect, matrix, &text, TX_MULTILINE, field)
}

/// `pdf_write_sig_widget_appearance`: a crossed box. MuPDF makes unsigned
/// signatures' appearances another way (`pdf_signature_appearance_unsigned`).
pub(super) fn sig(w: &mut Writer, rect: Rect, matrix: &mut Matrix) -> Rect {
    let (x0, y0) = (rect.x0 + 1.0, rect.y0 + 1.0);
    let (x1, y1) = (rect.x1 - 1.0, rect.y1 - 1.0);
    w.s("1 w\n0 G\n");
    w.op(&[x0, y0, x1 - x0, y1 - y0], "re");
    for (a, b, c, d) in [(x0, y0, x1, y1), (x1, y0, x0, y1)] {
        w.op(&[a, b], "m");
        w.buf.pop();
        w.buf.push(b' ');
        w.op(&[c, d], "l");
    }
    w.s("s\n");
    *matrix = Matrix::IDENTITY;
    rect
}

/// `pdf_write_widget_appearance` as `pdf_update_appearance` reaches it,
/// for text and choice fields: the bounding box. `field` is the widget as
/// stored, for the inherited field values.
pub(super) fn widget(
    w: &mut Writer,
    rect: Rect,
    matrix: &mut Matrix,
    field: &Object,
) -> Result<Rect, String> {
    let doc = w.doc;
    let ft = fonts::inheritable(doc, field, b"FT")?
        .map(|v| doc.resolve(&v))
        .unwrap_or(Object::Null);
    match ft.as_name() {
        Some(b"Tx") => {
            // Without JavaScript, the value is not formatted.
            let ff = form::flags(doc, field);
            let value = form::value(doc, field);
            tx(w, rect, matrix, &value, ff, field)
        }
        Some(b"Ch") => ch(w, rect, matrix, field),
        Some(b"Sig") => Err(
            "unsigned signature appearances are drawn through a display list, \
             which is not supported"
                .into(),
        ),
        // Buttons, an appearance per state, are `button`'s.
        other => Err(format!(
            "cannot create appearance stream for {} widgets",
            String::from_utf8_lossy(other.unwrap_or(b""))
        )),
    }
}

/// A form XObject's parts, as `pdf_new_xobject` puts them together.
#[derive(Debug, Clone)]
pub struct Form {
    pub bbox: Rect,
    pub matrix: Matrix,
    pub resources: Option<Dict>,
    pub content: Vec<u8>,
}

/// A button's appearances as `pdf_update_button_appearance` makes them:
/// the widget as it is after (with the /AS it sets), and each stream, by
/// its /AP key (N or D) and, for check boxes and radio buttons, its state.
#[derive(Debug, Clone)]
pub struct ButtonSynthesis {
    pub annot: Object,
    pub forms: Vec<(&'static str, Option<Vec<u8>>, Form)>,
}

/// One stream drawn by `f` with a writer of its own.
fn draw(
    doc: &Document,
    annot: &Object,
    ctm: Matrix,
    bbox: Rect,
    matrix: Matrix,
    f: impl FnOnce(&mut Writer) -> Result<(), String>,
) -> Result<Form, String> {
    let mut w = Writer {
        doc,
        annot: annot.clone(),
        ctm,
        buf: Vec::with_capacity(1024),
        res: None,
    };
    f(&mut w)?;
    Ok(Form {
        bbox,
        matrix,
        resources: w.res,
        content: w.buf,
    })
}

/// `draw_push_button`: the caption on a bevelled face, pressed in when
/// `down`.
#[allow(clippy::too_many_arguments)]
fn push_button(
    w: &mut Writer,
    bw: f32,
    bh: f32,
    caption: &str,
    da: &fonts::DefaultAppearance,
    down: bool,
) -> Result<(), String> {
    let b = w.border_width()?;
    let bc = w.mk_rgb(b"BC");
    let bg = w.mk_rgb(b"BG");
    let face = bg.unwrap_or([0.8; 3]);
    let (mut hi, mut sh) = ([0.0f32; 3], [0.0f32; 3]);
    for i in 0..3 {
        let light = 1.0 - (1.0 - face[i]) / 2.0;
        let dark = face[i] / 2.0;
        (hi[i], sh[i]) = if down { (dark, light) } else { (light, dark) };
    }
    w.s("q\n");
    w.op(&[b], "w");
    if bg.is_some() {
        w.op(&face, "rg");
        // MuPDF passes the integers 0, 0 for the box's size.
        w.s("0 0 0 0 re\nf\n");
    }
    if let Some(bc) = bc
        && b > 0.0
    {
        w.op(&bc, "RG");
        w.op(&[b / 2.0, b / 2.0, bw - b, bh - b], "re\nS");
    }
    if bg.is_some() {
        w.op(&hi, "rg");
        let path = |w: &mut Writer, p: [f32; 12]| {
            for (i, xy) in p.chunks(2).enumerate() {
                w.g(xy[0]);
                w.buf.push(b' ');
                w.g(xy[1]);
                w.s(if i == 0 { " m " } else { " l " });
            }
            w.s("f\n");
        };
        path(
            w,
            [
                b,
                b,
                b,
                bh - b,
                bw - b,
                bh - b,
                bw - b - 2.0,
                bh - b - 2.0,
                b + 2.0,
                bh - b - 2.0,
                b + 2.0,
                b + 2.0,
            ],
        );
        w.op(&sh, "rg");
        path(
            w,
            [
                b,
                b,
                b + 2.0,
                b + 2.0,
                bw - b - 2.0,
                b + 2.0,
                bw - b - 2.0,
                bh - b - 2.0,
                bw - b,
                bh - b,
                bw - b,
                b,
            ],
        );
    }
    if down {
        w.s("1 0 0 1 2 -2 cm\n");
    }
    layout::write_variable_text(
        w,
        LANG_UNSET,
        caption,
        da.mapped_font(),
        da.color(),
        &TextBox {
            size: da.size,
            q: 1,
            w: bw,
            h: bh,
            padding: b + 6.0,
            baseline: 0.8,
            lineheight: 1.2,
            multiline: false,
            comb: 0,
            adjust_baseline: false,
        },
    )?;
    w.s("Q\n");
    Ok(())
}

/// `draw_circle`.
fn draw_circle(w: &mut Writer, rx: f32, ry: f32, cx: f32, cy: f32) {
    let mx = rx * CIRCLE_MAGIC;
    let my = ry * CIRCLE_MAGIC;
    w.op(&[cx, cy + ry], "m");
    w.op(&[cx + mx, cy + ry, cx + rx, cy + my, cx + rx, cy], "c");
    w.op(&[cx + rx, cy - my, cx + mx, cy - ry, cx, cy - ry], "c");
    w.op(&[cx - mx, cy - ry, cx - rx, cy - my, cx - rx, cy], "c");
    w.op(&[cx - rx, cy + my, cx - mx, cy + ry, cx, cy + ry], "c");
}

/// `draw_circle_in_box`.
fn draw_circle_in_box(w: &mut Writer, lw: f32, x0: f32, y0: f32, x1: f32, y1: f32) {
    let rx = (x1 - x0) / 2.0 - lw / 2.0;
    let ry = (y1 - y0) / 2.0 - lw / 2.0;
    let cx = x0 + lw / 2.0 + rx;
    let cy = y0 + lw / 2.0 + ry;
    draw_circle(w, rx, ry, cx, cy);
}

/// `draw_radio_button`: a disc, with a dot when `yes`.
fn radio_button(w: &mut Writer, bw: f32, bh: f32, yes: bool) -> Result<(), String> {
    w.s("q\n");
    if w.mk_bg() {
        draw_circle_in_box(w, 0.0, 0.0, 0.0, bw, bh);
        w.s("f\n");
    }
    let b = w.border()?;
    if b > 0.0 && w.mk_bc() {
        draw_circle_in_box(w, b, 0.0, 0.0, bw, bh);
        w.s("s\n");
    }
    if yes {
        w.s("0 g\n");
        draw_circle(
            w,
            (bw - b * 2.0) / 4.0,
            (bh - b * 2.0) / 4.0,
            bw / 2.0,
            bh / 2.0,
        );
        w.s("f\n");
    }
    w.s("Q\n");
    Ok(())
}

/// `draw_check_button`: a box, with a ZapfDingbats check when `yes`.
fn check_button(w: &mut Writer, bw: f32, bh: f32, yes: bool) -> Result<(), String> {
    w.s("q\n");
    if w.mk_bg() {
        w.s("0 0 ");
        w.op(&[bw, bh], "re\nf");
    }
    let b = w.border()?;
    if b > 0.0 && w.mk_bc() {
        w.op(&[b / 2.0, b / 2.0, bw - b, bh - b], "re\nS");
    }
    if yes {
        layout::write_variable_text(
            w,
            LANG_UNSET,
            "3",
            "ZaDb",
            &[0.0],
            &TextBox {
                size: bh,
                q: 0,
                w: bw,
                h: bh,
                padding: b + bh / 10.0,
                baseline: 0.8,
                lineheight: 1.2,
                multiline: false,
                comb: 0,
                adjust_baseline: false,
            },
        )?;
    }
    w.s("Q\n");
    Ok(())
}

/// `pdf_button_field_on_state`: the first state of /AP /N (or /D) that is
/// not Off, else Yes.
fn on_state(doc: &Document, annot: &Object) -> Vec<u8> {
    let ap = doc.dict_get(annot, b"AP");
    for key in [&b"N"[..], b"D"] {
        let states = doc.dict_get(&ap, key);
        if let Some(d) = states.as_dict()
            && let Some((k, _)) = d.iter().find(|(k, _)| *k != b"Off")
        {
            return k.to_vec();
        }
    }
    b"Yes".to_vec()
}

/// `pdf_update_button_appearance`: a push button's normal and down
/// appearances with its /MK captions, or a check box's or radio button's
/// Off and on states; /AS is set to Off when it has none.
pub fn button(doc: &Document, field: &Object, ctm: Matrix) -> Result<ButtonSynthesis, String> {
    let annot = doc.resolve(field);
    if annot.as_dict().is_none() {
        return Err("annotation is not a dictionary".into());
    }
    let mut w = Writer {
        doc,
        annot,
        ctm,
        buf: Vec::new(),
        res: None,
    };
    let ff = form::flags(doc, field);
    let rect = doc.to_rect(Some(&w.get(b"Rect")));
    let (mut bw, mut bh, matrix) = widget_box(&w, rect);
    let bbox = Rect::new(0.0, 0.0, bw, bh);
    let annot = w.annot.clone();
    let text_of = |o: &Object| match doc.resolve(o) {
        Object::String(s) => crate::text_string::text_string(&s),
        _ => String::new(),
    };

    if ff & form::BTN_PUSHBUTTON != 0 {
        let da = fonts::default_appearance(doc, field)?;
        let mk = w.get(b"MK");
        let ca = doc.dict_get(&mk, b"CA");
        let ac = doc.dict_get(&mk, b"AC");
        let label = text_of(&ca);
        let n = draw(doc, &annot, ctm, bbox, matrix, |w| {
            push_button(w, bw, bh, &label, &da, false)
        })?;
        let label = text_of(if matches!(ac, Object::Null) { &ca } else { &ac });
        let d = draw(doc, &annot, ctm, bbox, matrix, |w| {
            push_button(w, bw, bh, &label, &da, true)
        })?;
        return Ok(ButtonSynthesis {
            annot,
            forms: vec![("N", None, n), ("D", None, d)],
        });
    }

    if bw > bh {
        bw = bh;
    }
    if bh > bw {
        bh = bw;
    }
    let radio = ff & form::BTN_RADIO != 0;
    let state = |yes: bool| {
        draw(doc, &annot, ctm, bbox, matrix, |w| {
            if radio {
                radio_button(w, bw, bh, yes)
            } else {
                check_button(w, bw, bh, yes)
            }
        })
    };
    let off = state(false)?;
    let yes = state(true)?;
    let on = match w.get(b"AS") {
        Object::Null => {
            w.put(b"AS", Object::name(b"Off"));
            on_state(doc, &annot)
        }
        Object::Name(n) if &*n == b"Off" => on_state(doc, &annot),
        Object::Name(n) => n.to_vec(),
        _ => return Err("key is not a name".into()),
    };
    Ok(ButtonSynthesis {
        annot: w.annot,
        forms: vec![("N", Some(b"Off".to_vec()), off), ("N", Some(on), yes)],
    })
}

#[cfg(test)]
mod tests {
    //! Each case's content is MuPDF 1.28.6's for the same widget.

    use super::super::synthesize;
    use super::*;
    use crate::object::ObjRef;

    fn open(dict: &str) -> Document {
        let pdf = format!(
            "%PDF-1.7\n1 0 obj\n<< /Type /Annot /Subtype /Widget {dict} >>\nendobj\n\
             2 0 obj\n<< /Type /Catalog /AcroForm << /DA (/Helv 0 Tf 0 g) >> >>\nendobj\n\
             trailer\n<< /Size 3 /Root 2 0 R >>\n"
        );
        Document::open(pdf.into_bytes()).unwrap()
    }

    fn widget() -> Object {
        Object::Ref(ObjRef::new(1, 0))
    }

    fn bytes(content: &[u8]) -> String {
        content.iter().map(|&b| char::from(b)).collect()
    }

    fn check(dict: &str, content: &str, bbox: [f32; 4], matrix: Matrix) {
        let doc = open(dict);
        let s = synthesize(&doc, &widget(), Matrix::IDENTITY).unwrap();
        assert_eq!(bytes(&s.content), content);
        assert_eq!(s.bbox, Rect::new(bbox[0], bbox[1], bbox[2], bbox[3]));
        assert_eq!(s.matrix, matrix);
    }

    #[test]
    fn text_field_centred() {
        check(
            "/FT /Tx /V (Centred) /DA (/TiRo 10 Tf 1 0 0 rg) /Q 1 /Rect [320 740 560 764]",
            "/Tx BMC\nq\n1 w\n1 1 238 22 re\nW\nn\nBT\n1 0 0 rg\n104.17 9 Td\n/TiRo 10 Tf\n\
             (Centred) Tj\nET\nQ\nEMC\n",
            [0.0, 0.0, 240.0, 24.0],
            Matrix::IDENTITY,
        );
    }

    #[test]
    fn text_field_multiline_wraps() {
        check(
            "/FT /Tx /V (Auto size multiline text that is long enough to wrap around the box) \
             /DA (/Helv 0 Tf) /Ff 4096 /Rect [40 604 280 664]",
            "/Tx BMC\nq\n1 w\n1 1 238 58 re\nW\nn\nBT\n0 g\n2 58 Td\n0 -13.392 Td\n/Helv 12 Tf\n\
             (Auto size multiline text that is long enough ) Tj\n0 -13.392 Td\n/Helv 12 Tf\n\
             (to wrap around the box) Tj\nET\nQ\nEMC\n",
            [0.0, 0.0, 240.0, 60.0],
            Matrix::IDENTITY,
        );
    }

    #[test]
    fn text_field_comb_draws_cells() {
        check(
            "/FT /Tx /V (12345) /DA (/Cour 12 Tf) /Ff 16777216 /MaxLen 8 /MK << /BC [0] >> \
             /Rect [40 536 280 560]",
            "/Tx BMC\nq\n1 w\n0 G\n.5 .5 239 23 re\ns\n1 1 238 22 re\nW\nn\n\
             30.75 1 m 30.75 23 l s\n60.5 1 m 60.5 23 l s\n90.25 1 m 90.25 23 l s\n\
             120 1 m 120 23 l s\n149.75 1 m 149.75 23 l s\n179.5 1 m 179.5 23 l s\n\
             209.25 1 m 209.25 23 l s\nBT\n0 g\n0 8.4 Td\n/Cour 12 Tf\n\
             [-950(1)-1900(2)-1900(3)-1900(4)-1900(5)] TJ\nET\nQ\nEMC\n",
            [0.0, 0.0, 240.0, 24.0],
            Matrix::IDENTITY,
        );
    }

    #[test]
    fn text_field_rotated() {
        check(
            "/FT /Tx /V (Rotated 90) /DA (/Helv 10 Tf) /MK << /R 90 /BG [0.8 0.9 1] >> \
             /Rect [40 400 70 520]",
            "/Tx BMC\nq\n.8 .9 1 rg\n0 0 120 30 re\nf\n1 w\n1 1 118 28 re\nW\nn\nBT\n0 g\n\
             2 12 Td\n/Helv 10 Tf\n(Rotated 90) Tj\nET\nQ\nEMC\n",
            [0.0, 0.0, 120.0, 30.0],
            Matrix {
                a: 0.0,
                b: 1.0,
                c: -1.0,
                d: 0.0,
                e: 0.0,
                f: 0.0,
            },
        );
    }

    #[test]
    fn list_box_starts_at_top_index() {
        check(
            "/FT /Ch /Opt [[(a) (Apple)] [(b) (Banana)] (Cherry)] /TI 1 /DA (/TiRo 9 Tf) \
             /MK << /BC [0 0 1] >> /Rect [320 604 520 684]",
            "/Tx BMC\nq\n1 w\n0 0 1 RG\n.5 .5 199 79 re\ns\n1 1 198 78 re\nW\nn\nBT\n0 g\n\
             2 78 Td\n0 -10.044001 Td\n/TiRo 9 Tf\n(Banana) Tj\n0 -10.044001 Td\n/TiRo 9 Tf\n\
             (Cherry) Tj\nET\nQ\nEMC\n",
            [0.0, 0.0, 200.0, 80.0],
            Matrix::IDENTITY,
        );
    }

    #[test]
    fn signed_signatures_are_not_made_again() {
        let doc = open("/FT /Sig /V << /Type /Sig >> /Rect [0 0 10 10]");
        assert!(!super::super::widget_made_again(&doc, &widget()));
        let doc = open("/FT /Sig /Rect [0 0 10 10]");
        assert!(super::super::widget_made_again(&doc, &widget()));
        assert!(synthesize(&doc, &widget(), Matrix::IDENTITY).is_err());
    }

    #[test]
    fn push_button_normal_and_down() {
        let doc = open(
            "/FT /Btn /Ff 65536 /MK << /CA (Normal) /AC (Down) /BG [0.6 0.7 0.9] /BC [0 0 0.5] >> \
             /DA (/TiRo 0 Tf 1 0 0 rg) /BS << /W 2 >> /Rect [40 706 240 730]",
        );
        let s = button(&doc, &widget(), Matrix::IDENTITY).unwrap();
        let got: Vec<(&str, String)> = s
            .forms
            .iter()
            .map(|(k, _, f)| (*k, bytes(&f.content)))
            .collect();
        assert_eq!(
            got,
            [
                (
                    "N",
                    "q\n2 w\n.6 .7 .9 rg\n0 0 0 0 re\nf\n0 0 .5 RG\n1 1 198 22 re\nS\n.8 .85 .95 rg\n\
                     2 2 m 2 22 l 198 22 l 196 20 l 4 20 l 4 4 l f\n.3 .35 .45 rg\n\
                     2 2 m 4 4 l 196 4 l 196 20 l 198 22 l 198 2 l f\nBT\n1 0 0 rg\n87.78 9.6 Td\n\
                     /TiRo 8 Tf\n(Normal) Tj\nET\nQ\n"
                        .to_string()
                ),
                (
                    "D",
                    "q\n2 w\n.6 .7 .9 rg\n0 0 0 0 re\nf\n0 0 .5 RG\n1 1 198 22 re\nS\n.3 .35 .45 rg\n\
                     2 2 m 2 22 l 198 22 l 196 20 l 4 20 l 4 4 l f\n.8 .85 .95 rg\n\
                     2 2 m 4 4 l 196 4 l 196 20 l 198 22 l 198 2 l f\n1 0 0 1 2 -2 cm\nBT\n\
                     1 0 0 rg\n90.224 9.6 Td\n/TiRo 8 Tf\n(Down) Tj\nET\nQ\n"
                        .to_string()
                ),
            ]
        );
        assert!(matches!(doc.dict_get(&s.annot, b"AS"), Object::Null));
    }

    #[test]
    fn check_box_takes_its_on_state_from_the_old_appearance() {
        let doc = open(
            "/FT /Btn /AS /Off /AP << /N << /Off 1 0 R /Choice1 1 0 R >> >> /Rect [40 468 60 498]",
        );
        let s = button(&doc, &widget(), Matrix::IDENTITY).unwrap();
        let got: Vec<(Option<&[u8]>, String)> = s
            .forms
            .iter()
            .map(|(_, st, f)| (st.as_deref(), bytes(&f.content)))
            .collect();
        assert_eq!(
            got,
            [
                (Some(&b"Off"[..]), "q\n1 w\nQ\n".to_string()),
                (
                    Some(&b"Choice1"[..]),
                    "q\n1 w\nBT\n0 g\n3 4 Td\n/ZaDb 20 Tf\n(3) Tj\nET\nQ\n".to_string()
                ),
            ]
        );
        assert_eq!(s.forms[0].2.bbox, Rect::new(0.0, 0.0, 20.0, 30.0));
    }

    #[test]
    fn check_box_without_state_is_set_off() {
        let doc = open("/FT /Btn /Rect [40 502 70 522]");
        let s = button(&doc, &widget(), Matrix::IDENTITY).unwrap();
        assert!(doc.dict_get(&s.annot, b"AS").is_name(b"Off"));
        assert_eq!(s.forms[1].1.as_deref(), Some(&b"Yes"[..]));
    }

    #[test]
    fn radio_button_dot() {
        let doc = open(
            "/FT /Btn /Ff 49152 /AS /One /MK << /BG [0.9] /BC [1 0 0] >> /BS << /W 2 >> \
             /Rect [40 332 64 356]",
        );
        let s = button(&doc, &widget(), Matrix::IDENTITY).unwrap();
        assert_eq!(s.forms[1].1.as_deref(), Some(&b"One"[..]));
        assert_eq!(
            bytes(&s.forms[1].2.content),
            "q\n.9 g\n12 24 m\n18.62298 24 24 18.62298 24 12 c\n24 5.37702 18.62298 0 12 0 c\n\
             5.37702 0 0 5.37702 0 12 c\n0 18.62298 5.37702 24 12 24 c\nf\n2 w\n1 0 0 RG\n\
             12 23 m\n18.071064 23 23 18.071064 23 12 c\n23 5.928935 18.071064 1 12 1 c\n\
             5.928935 1 1 5.928935 1 12 c\n1 18.071064 5.928935 23 12 23 c\ns\n0 g\n12 17 m\n\
             14.759575 17 17 14.759575 17 12 c\n17 9.240425 14.759575 7 12 7 c\n\
             9.240425 7 7 9.240425 7 12 c\n7 14.759575 9.240425 17 12 17 c\nf\nQ\n"
        );
    }
}
