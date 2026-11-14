// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
#![no_main]
use std::sync::Arc;

use libfuzzer_sys::fuzz_target;
use nupdf::content::{Op, ProcState, Processor, process_contents, process_in_windows};
use nupdf::lex::Lexer;
use nupdf::parse::parse_stm_obj;
use nupdf::{Dict, Document, Object};

/// Formats every operator, so that each of its fields is read, and
/// keeps a running hash of them, to tell two runs apart.
struct Sink(u64, ProcState);

impl Processor for Sink {
    fn state(&mut self) -> &mut ProcState {
        &mut self.1
    }

    fn usage(&self) -> Option<&'static str> {
        Some("View")
    }

    fn op(&mut self, op: Op<'_>) -> Result<(), String> {
        for b in format!("{op:?}\n").bytes() {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x100_0000_01b3);
        }
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

/// What `data` runs as and reports, whole (None) or in windows of the
/// second size read the first size at a time.
fn run(data: &[u8], resources: Option<&Object>, windows: Option<(usize, usize)>) -> Option<String> {
    let doc = Document::create().ok()?;
    let mut sink = Sink(0, ProcState::default());
    let ended = match windows {
        None => {
            let num = doc.add_stream(Dict::new(), data.to_vec());
            let contents = Object::Ref(nupdf::ObjRef::new(num, 0));
            process_contents(&doc, resources, Some(&contents), &mut sink)
        }
        Some((step, want)) => {
            process_in_windows(&doc, resources, data, step, want, &mut sink).map(|_| ())
        }
    };
    let reported = doc.diagnostics().snapshot();
    Some(format!("{:x} {ended:?} {reported:?}", sink.0))
}

fuzz_target!(|data: &[u8]| {
    // The first two bytes say how the stream is cut into windows; the
    // rest is the stream.
    let (&[step, want], data) = data.split_first_chunk::<2>().unwrap_or((&[0, 0], data));
    let diag = nupdf::Diagnostics::new();
    let resources = parse_stm_obj(&mut Lexer::new(RESOURCES, 0, &diag))
        .unwrap_or(Object::Dict(Arc::new(Dict::new())));
    for resources in [Some(&resources), None] {
        // A stream runs the same decoded a few bytes ahead as it runs
        // whole (DECISIONS D55).
        let whole = run(data, resources, None);
        let windows = (usize::from(step) + 1, usize::from(want) + 1);
        let cut = run(data, resources, Some(windows));
        assert_eq!(cut, whole, "read {} bytes at a time into windows of {}", windows.0, windows.1);
    }
});
