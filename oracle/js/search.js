// Oracle for the `search` checks: each page's hits for a needle, as
// StructuredText.search finds them (fz_match_stext_page_cb) in
// Page.toStructuredText with default options, each hit its quads.
// Usage: mutool run oracle/js/search.js <file> <needle> <options>
//
// Each page is {id, hits: [[quad, ...], ...]}, or {id, error}. Pages lose
// their /Annots before they are loaded, as for the stext checks.

var doc = Document.openDocument(scriptArgs[0]);
var needle = scriptArgs[1];
var options = scriptArgs.length > 2 ? scriptArgs[2] : "";

function list(v) {
  var a = [];
  for (var i = 0; i < v.length; i++) a.push(v[i]);
  return a;
}

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
    var hits = page.toStructuredText("").search(needle, options, 1000000);
    entry = { id: id, hits: hits.map(function (h) { return list(h).map(list); }) };
  } catch (e) {
    entry = { id: id, error: String(e.message || e) };
  }
  out.pages.push(entry);
}
print(JSON.stringify(out));
