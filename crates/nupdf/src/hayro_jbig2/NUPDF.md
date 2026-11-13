# hayro-jbig2 0.4.0, patched for nupdf

This is hayro-jbig2 0.4.0 from crates.io (MIT OR Apache-2.0, see LICENSE-MIT and LICENSE-APACHE), carried as a module of the nupdf crate, since crates.io does not apply a workspace's `[patch]`. As a module: `crate::` paths point into it, `alloc::` is `std::`, its `std` and `simd` features are always on and `image` off (their other branches and `integration.rs` removed). The code changes are the guards below in `decode/symbol.rs`, and two places that panicked on damaged data now fail instead (`decode/generic.rs`, a region shorter than its row count; `huffman_table.rs`, a table too large to index). Each is marked `nupdf:`.

Past the end of its data, the arithmetic decoder keeps returning values, so a damaged symbol dictionary can decode as an endless list of empty export runs or of empty height classes, and decoding never ends. nupdf's fuzzer found both. jbig2dec fails a dictionary after 1000 empty export runs; the patch does the same for runs and, which jbig2dec does not, for height classes. Both fail with `SymbolError::Invalid`.

Decoding time also follows the symbol sizes a dictionary declares, so a few hundred bytes can ask for minutes of work (jbig2dec takes even longer). The patch caps a dictionary at 2^28 pixels of symbols, far more than real dictionaries hold, and fails with `OverflowError::BitmapDimension` beyond it.

To be dropped once upstream has equivalent guards.

```diff
--- a/src/decode/symbol.rs
+++ b/src/decode/symbol.rs
@@ -23,6 +23,8 @@
 use crate::reader::Reader;
 
 pub(crate) const MAX_SYMBOLS: u32 = 1 << 20;
+/// nupdf: the most pixels one symbol dictionary may decode.
+const MAX_SYMBOL_PIXELS: u64 = 1 << 28;
 
 /// Decode a symbol dictionary segment (7.4.2, 6.5).
 pub(crate) fn decode(
@@ -73,6 +75,14 @@
         }
     };
 
+    // nupdf: damaged data can decode as endless empty height classes (the
+    // decoders keep returning values past its end), so give up after many
+    // in a row, as jbig2dec does for empty export runs.
+    let mut empty_classes = 0u32;
+    // nupdf: decoding time follows the symbol sizes a dictionary declares,
+    // so a few hundred bytes can ask for minutes of work. Real dictionaries
+    // hold far fewer pixels than this.
+    let mut pixels: u64 = 0;
     while ctx.symbols_decoded_count < num_new_symbols {
         let height_class_delta =
             read_height_class_delta(&mut ctx)?.ok_or(SymbolError::UnexpectedOob)?;
@@ -101,6 +111,10 @@
                 .total_width
                 .checked_add(symbol_width)
                 .ok_or(OverflowError::BitmapDimension)?;
+            pixels += u64::from(symbol_width) * u64::from(ctx.height_class_height);
+            if pixels > MAX_SYMBOL_PIXELS {
+                bail!(OverflowError::BitmapDimension);
+            }
 
             match (ctx.header.flags.use_huffman, ctx.header.flags.use_refagg) {
                 (false, false) => {
@@ -140,6 +154,15 @@
             // the previously decoded symbol widths to decode the collective bitmap
             // and extract the individual symbols from that bitmap.
             decode_height_class_collective_bitmap(&mut ctx)?;
+        }
+
+        if ctx.symbols_decoded_count == ctx.height_class_first_symbol {
+            empty_classes += 1;
+            if empty_classes == 1000 {
+                bail!(SymbolError::Invalid);
+            }
+        } else {
+            empty_classes = 0;
         }
     }
 
@@ -708,9 +731,20 @@
     let mut exported = Vec::with_capacity(ctx.header.num_exported_symbols as usize);
     let mut index: u32 = 0;
     let mut should_export = false;
+    // nupdf: as jbig2dec, stop an endless list of empty runs (damaged data
+    // decodes as zeros past its end).
+    let mut empty_runs = 0u32;
 
     while index < total_symbols {
         let run_length = read_run_length()?;
+        if run_length == 0 {
+            empty_runs += 1;
+            if empty_runs == 1000 {
+                bail!(SymbolError::Invalid);
+            }
+        } else {
+            empty_runs = 0;
+        }
 
         let end_index = index.checked_add(run_length).ok_or(OverflowError::Index)?;
         if end_index > total_symbols {
```
