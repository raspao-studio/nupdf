// Bench side for `split`: each page written to a file of its own, from a
// fresh copy of the input, as `mutool clean <in> <out> <page>` would write
// it (pdf_rearrange_pages, then the save) and as `nupdf split` does.
// mutool has no split command; this is what a script would do.
// Usage: mutool run oracle/js/split.js <file> <prefix>; part N is <prefix>-N.pdf
var path = scriptArgs[0];
var count = Document.openDocument(path).countPages();
for (var p = 0; p < count; p++) {
  var doc = Document.openDocument(path).asPDF();
  for (var i = 1; i < doc.countObjects(); i++) {
    try { doc.newIndirect(i, 0).resolve(); } catch (e) {}
  }
  doc.rearrangePages([p]);
  doc.save(scriptArgs[1] + "-" + (p + 1) + ".pdf", "");
}
