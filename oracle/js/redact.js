// Oracle for the redaction checks: each page's hits of a needle, as
// StructuredText.search finds them in Page.toStructuredText with default
// options (the page's /Annots set aside while it is searched, as for the
// search checks), each hit made a Redact annotation with its quads, then
// PDFPage.applyRedactions (pdf_redact_page) with no black boxes, images
// and line art left alone. Every object is loaded first, as nupdf's
// redact does, so a repair cannot lose the edits.
// Usage: mutool run oracle/js/redact.js <file> <scratch-dir> <needle> <options>
// The output is written to <scratch-dir>/redact.pdf.

var doc = Document.openDocument(scriptArgs[0]);
var pdf = doc.asPDF ? doc.asPDF() : doc;
var needle = scriptArgs[2];
var options = scriptArgs.length > 3 ? scriptArgs[3] : "";
if (pdf.needsPassword() && !pdf.authenticatePassword(""))
  throw new Error("cannot authenticate password: " + scriptArgs[0]);
for (var i = 1; i < pdf.countObjects(); i++) {
  try { pdf.newIndirect(i, 0).resolve(); } catch (e) {}
}
var n = pdf.countPages();
for (var p = 0; p < n; p++) {
  var obj = pdf.findPage(p);
  var annots = obj.get("Annots");
  var had = obj.isDictionary() && annots && !annots.isNull();
  if (had) obj.delete("Annots");
  var page = pdf.loadPage(p);
  var hits = page.toStructuredText("").search(needle, options, 1000000);
  if (had) obj.put("Annots", annots);
  if (hits.length == 0) continue;
  hits.forEach(function (h) {
    var a = page.createAnnotation("Redact");
    a.setQuadPoints(h);
  });
  page.applyRedactions(false, 0, 0, 0);
}
pdf.save(scriptArgs[1] + "/redact.pdf", "garbage=1,compress");
print(JSON.stringify({ output: { file: "redact.pdf" } }));
