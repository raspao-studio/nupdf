// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-page.c (pdf_lookup_page_label,
// pdf_format_page_label, pdf_page_label),
// Copyright (C) 2004-2026 Artifex Software, Inc.

//! Page labels: the name a page carries in a viewer (`iv`, `A-3`), from the
//! catalog's /PageLabels number tree, as `pdf_page_label` makes it.

use std::collections::HashSet;

use crate::document::Document;
use crate::object::Object;

/// Number-tree nodes deeper than this are not looked into; MuPDF recurses
/// without a bound.
const MAX_DEPTH: usize = 64;

/// The bytes `fz_page_label`'s buffer holds, the terminating NUL included.
const BUF: usize = 100;

/// The labelling range a page falls in: the first page it covers and its
/// label dictionary.
struct Range {
    offset: i64,
    label: Option<Object>,
}

fn lookup(
    doc: &Document,
    node: &Object,
    index: i64,
    range: &mut Range,
    seen: &mut HashSet<i32>,
    depth: usize,
) {
    if depth > MAX_DEPTH {
        return;
    }
    if let Object::Ref(r) = node
        && !seen.insert(r.num)
    {
        return;
    }
    if let Object::Array(kids) = doc.resolve(&doc.dict_get(node, b"Kids")) {
        for kid in kids.iter() {
            lookup(doc, kid, index, range, seen, depth + 1);
        }
    }
    if let Object::Array(nums) = doc.resolve(&doc.dict_get(node, b"Nums")) {
        for pair in nums.chunks(2) {
            let k = i64::from(doc.resolve(&pair[0]).to_int());
            if k <= index {
                range.offset = k;
                range.label = pair.get(1).cloned();
            } else {
                // Past the page: stop looking in this node.
                return;
            }
        }
    }
}

/// `pdf_format_roman_page_label`.
fn roman(mut out: String, n: i64, upper: bool) -> String {
    const UC: [[&str; 10]; 3] = [
        ["", "I", "II", "III", "IV", "V", "VI", "VII", "VIII", "IX"],
        ["", "X", "XX", "XXX", "XL", "L", "LX", "LXX", "LXXX", "XC"],
        ["", "C", "CC", "CCC", "CD", "D", "DC", "DCC", "DCCC", "CM"],
    ];
    let sym = |i: usize, d: i64| {
        let s = UC[i][d as usize];
        if upper {
            s.to_string()
        } else {
            s.to_lowercase()
        }
    };
    let m = if upper { 'M' } else { 'm' };
    // The thousands fill the buffer at most.
    for _ in 0..(n / 1000).min(BUF as i64) {
        out.push(m);
    }
    out.push_str(&sym(2, (n / 100) % 10));
    out.push_str(&sym(1, (n / 10) % 10));
    out.push_str(&sym(0, n % 10));
    out
}

/// `pdf_format_page_label`: the page `index` pages into the range `dict`
/// starts.
fn format(doc: &Document, index: i64, dict: &Object) -> String {
    let style = doc.resolve(&doc.dict_get(dict, b"S"));
    let prefix = match doc.resolve(&doc.dict_get(dict, b"P")) {
        Object::String(s) => crate::text_string::text_string(&s),
        _ => String::new(),
    };
    let start = i64::from(doc.resolve(&doc.dict_get(dict, b"St")).to_int()).max(1);
    let v = start.saturating_add(index).max(1);
    let out = prefix;
    match style.as_name() {
        Some(b"D") => out + &v.to_string(),
        Some(b"R") => roman(out, v, true),
        Some(b"r") => roman(out, v, false),
        Some(b"A") | Some(b"a") => {
            let letter = (b'A' + ((v - 1) % 26) as u8) as char;
            let letter = if style.as_name() == Some(b"a") {
                letter.to_ascii_lowercase()
            } else {
                letter
            };
            let reps = ((v - 1) / 26 + 1).min(BUF as i64 - 1) as usize;
            out + &letter.to_string().repeat(reps)
        }
        _ => out,
    }
}

/// `fz_page_label` of the page at `index` (from 0): its label in the
/// /PageLabels tree, or its number from 1 where there is none; cut, as
/// MuPDF's buffer cuts it, to 99 bytes.
pub fn page_label(doc: &Document, index: i32) -> String {
    let root = doc.trailer().get(b"Root").cloned().unwrap_or(Object::Null);
    let labels = doc.dict_get(&root, b"PageLabels");
    let mut range = Range {
        offset: 0,
        label: None,
    };
    lookup(
        doc,
        &labels,
        i64::from(index),
        &mut range,
        &mut HashSet::new(),
        0,
    );
    let mut label = match range.label {
        Some(dict) => format(doc, i64::from(index) - range.offset, &dict),
        None => (i64::from(index) + 1).to_string(),
    };
    if label.len() > BUF - 1 {
        let mut end = BUF - 1;
        while !label.is_char_boundary(end) {
            end -= 1;
        }
        label.truncate(end);
    }
    label
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc_with_labels(labels: &str) -> Document {
        let objs = [
            format!("<</Type/Catalog/Pages 2 0 R/PageLabels {labels}>>"),
            "<</Type/Pages/Kids[3 0 R 4 0 R 5 0 R 6 0 R 7 0 R 8 0 R]/Count 6>>".to_string(),
        ];
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offs = Vec::new();
        for (i, o) in objs.iter().enumerate() {
            offs.push(out.len());
            out.extend_from_slice(format!("{} 0 obj {o} endobj\n", i + 1).as_bytes());
        }
        for i in 3..9 {
            offs.push(out.len());
            out.extend_from_slice(
                format!("{i} 0 obj <</Type/Page/Parent 2 0 R/MediaBox[0 0 10 10]>> endobj\n")
                    .as_bytes(),
            );
        }
        let xref = out.len();
        out.extend_from_slice(b"xref\n0 9\n0000000000 65535 f \n");
        for o in offs {
            out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!("trailer <</Size 9/Root 1 0 R>>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
        );
        Document::open(out).unwrap()
    }

    #[test]
    fn labels_follow_their_ranges_and_styles() {
        let doc = doc_with_labels("<</Nums[0<</S/r>> 2<</S/D/P(A-)/St 7>> 4<</S/A>> 5<</P(x)>>]>>");
        let got: Vec<String> = (0..6).map(|i| page_label(&doc, i)).collect();
        assert_eq!(got, ["i", "ii", "A-7", "A-8", "A", "x"]);
        // No tree: page numbers from 1.
        let plain = doc_with_labels("null");
        assert_eq!(page_label(&plain, 4), "5");
        // A tree of kids, ranges in each.
        let kids =
            doc_with_labels("<</Kids[<</Nums[0<</S/R/St 1998>>]>> <</Nums[3<</S/a/St 27>>]>>]>>");
        let got: Vec<String> = (0..5).map(|i| page_label(&kids, i)).collect();
        assert_eq!(got, ["MCMXCVIII", "MCMXCIX", "MM", "aa", "bb"]);
    }

    #[test]
    fn huge_numbers_fill_the_buffer_and_stop() {
        let doc = doc_with_labels("<</Nums[0<</S/R/St 2000000000>>]>>");
        assert_eq!(page_label(&doc, 0).len(), 99);
        let doc = doc_with_labels("<</Nums[0<</S/a/St 99999>>]>>");
        assert_eq!(page_label(&doc, 0).len(), 99);
    }
}
