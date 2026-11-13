// Oracle for the `text` checks: each page's text as MuPDF writes it
// (StructuredText.asText, fz_print_stext_page_as_text) from
// Page.toStructuredText.
// Usage: mutool run oracle/js/text.js <file> [options] [annots]
//
// Each page is {id, text}, or {id, error}. Pages lose their /Annots
// before they are loaded, as for the trace and stext checks.
//
// With "annots", the whole document's text as `mutool draw -F txt` writes
// it: each page run whole (fz_run_page: contents, annotations, widgets)
// into a display list and its structured text, then a form feed and a
// newline; a page that fails is left out. The result is {text}.

var doc = Document.openDocument(scriptArgs[0]);
var options = scriptArgs.length > 1 ? scriptArgs[1] : "";
var annots = scriptArgs[2] === "annots";

var out = { page_count: null, pages: [] };
try {
  out.page_count = doc.countPages();
} catch (e) {
  out.page_count_error = String(e.message || e);
}
var all = "";
for (var p = 0; p < (out.page_count || 0); p++) {
  var id = "p" + (p + 1);
  var entry;
  try {
    var obj = doc.findPage(p);
    if (!obj.isDictionary()) throw new Error("page object is not a dictionary");
    if (annots) {
      var page = doc.loadPage(p);
      all += page.toDisplayList(true).toStructuredText(options).asText() + "\f\n";
      continue;
    }
    obj.delete("Annots");
    var page = doc.loadPage(p);
    entry = { id: id, text: page.toStructuredText(options).asText() };
  } catch (e) {
    entry = { id: id, error: String(e.message || e) };
  }
  out.pages.push(entry);
}
print(JSON.stringify(annots ? { text: all } : out));
