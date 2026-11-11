// Oracle for the `stext` checks: each page's structured text, as
// Page.toStructuredText makes it (fz_new_stext_page_from_page: the page's
// contents run through the stext device, no annotations), walked block by
// block, line by line and character by character.
// Usage: mutool run oracle/js/stext.js <file> [options]
//
// The options are MuPDF's stext option string ("preserve-ligatures,..."),
// clipping only when absent. Colors are left out: MuPDF converts them with
// its color management. Vector blocks (option `vectors`) carry their flags
// as one number: stroked 1, rectangle 2, continues 4; structure blocks
// (option `segment`) hold the blocks within them. Each page loses its
// /Annots before it is loaded, as for the trace check.

var doc = Document.openDocument(scriptArgs[0]);
var options = scriptArgs.length > 1 ? scriptArgs[1] : "";

function list(v) {
  var a = [];
  for (var i = 0; i < v.length; i++) a.push(v[i]);
  return a;
}

function walk(text) {
  // Structure blocks hold the blocks within them; `blocks` is the list
  // being filled, and `outer` the lists around it.
  var blocks = [];
  var outer = [];
  var block = null;
  var line = null;
  text.walk({
    beginStruct: function (standard, raw, index) {
      var s = { type: "struct", standard: standard, raw: raw, index: index, blocks: [] };
      blocks.push(s);
      outer.push(blocks);
      blocks = s.blocks;
    },
    endStruct: function () {
      blocks = outer.pop();
    },
    onImageBlock: function (bbox, transform) {
      blocks.push({ type: "image", bbox: list(bbox), transform: list(transform) });
    },
    onVector: function (bbox, flags) {
      var f = (flags.isStroked ? 1 : 0) | (flags.isRectangle ? 2 : 0) | (flags.continues ? 4 : 0);
      blocks.push({ type: "vector", bbox: list(bbox), flags: f });
    },
    beginTextBlock: function (bbox, flags) {
      block = { type: "text", bbox: list(bbox), flags: flags, lines: [] };
    },
    beginLine: function (bbox, wmode, dir) {
      line = { bbox: list(bbox), wmode: wmode ? 1 : 0, dir: list(dir), chars: [] };
    },
    onChar: function (c, origin, font, size, quad, rgb, flags, bidi) {
      line.chars.push([c, list(origin), font.getName(), size, list(quad), flags, bidi]);
    },
    endLine: function () {
      block.lines.push(line);
    },
    endTextBlock: function () {
      blocks.push(block);
    },
  });
  return blocks;
}

var out = { page_count: null, pages: [] };
try {
  out.page_count = doc.countPages();
} catch (e) {
  out.page_count_error = String(e.message || e);
}
for (var p = 0; p < (out.page_count || 0); p++) {
  var entry;
  try {
    var obj = doc.findPage(p);
    if (!obj.isDictionary()) throw new Error("page object is not a dictionary");
    obj.delete("Annots");
    var page = doc.loadPage(p);
    entry = { blocks: walk(page.toStructuredText(options)) };
  } catch (e) {
    entry = { error: String(e.message || e) };
  }
  out.pages.push(entry);
}
print(JSON.stringify(out));
