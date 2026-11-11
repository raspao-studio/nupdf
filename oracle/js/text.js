// Oracle for the `text` checks: each page's text as MuPDF writes it
// (StructuredText.asText, fz_print_stext_page_as_text) from
// Page.toStructuredText.
// Usage: mutool run oracle/js/text.js <file> [options]
//
// Each page is {id, text}, or {id, error}. Pages lose their /Annots
// before they are loaded, as for the trace and stext checks.

var doc = Document.openDocument(scriptArgs[0]);
var options = scriptArgs.length > 1 ? scriptArgs[1] : "";

var out = { page_count: null, pages: [] };
try {
  out.page_count = doc.countPages();
} catch (e) {
  out.page_count_error = String(e.message || e);
}
for (var p = 0; p < (out.page_count || 0); p++) {
  var id = "p" + (p + 1);
  var entry;
  try {
    var obj = doc.findPage(p);
    if (!obj.isDictionary()) throw new Error("page object is not a dictionary");
    obj.delete("Annots");
    var page = doc.loadPage(p);
    entry = { id: id, text: page.toStructuredText(options).asText() };
  } catch (e) {
    entry = { id: id, error: String(e.message || e) };
  }
  out.pages.push(entry);
}
print(JSON.stringify(out));
