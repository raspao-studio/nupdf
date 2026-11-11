// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
#![no_main]
use std::sync::Arc;

use libfuzzer_sys::fuzz_target;
use nupdf::content::{Op, Processor, process_contents};
use nupdf::lex::Lexer;
use nupdf::parse::parse_stm_obj;
use nupdf::{Dict, Document, Object};

/// Formats every operator, so that each of its fields is read.
struct Sink(usize);

impl Processor for Sink {
    fn op(&mut self, op: Op<'_>) -> Result<(), String> {
        self.0 += format!("{op:?}").len();
        Ok(())
    }
}

/// Resources with one of each kind the interpreter looks up.
const RESOURCES: &[u8] = b"<</Font<</F1<</Type/Font>>>>\
/ColorSpace<</CS0[/Indexed/RGB 1<ff00ff>]/CS1[/Pattern]/CS2/Foo>>\
/Pattern<</P1<</PatternType 1>>/P2<</PatternType 2>>/P3<<>>>>\
/XObject<</X<</Subtype/Form>>/I<</Subtype/Image>>/PS<</Subtype/PS>>>>\
/ExtGState<</GS<</LW 2/LC 9/D[[1 2]3]/RI/Saturation/Font[<<>>12]/TR/F/BM[/Multiply]/SMask<<>>>>>>\
/Shading<</Sh<<>>>>>>";

fuzz_target!(|data: &[u8]| {
    let Ok(doc) = Document::create() else {
        return;
    };
    let num = doc.add_stream(Dict::new(), data.to_vec());
    let contents = Object::Ref(nupdf::ObjRef::new(num, 0));
    let diag = nupdf::Diagnostics::new();
    let resources = parse_stm_obj(&mut Lexer::new(RESOURCES, 0, &diag))
        .unwrap_or(Object::Dict(Arc::new(Dict::new())));
    let mut sink = Sink(0);
    let _ = process_contents(&doc, Some(&resources), Some(&contents), &mut sink);
    let _ = process_contents(&doc, None, Some(&contents), &mut sink);
});
