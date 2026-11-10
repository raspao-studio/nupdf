// Oracle for the `merge` checks: the pages of one or more documents merged
// into a new one, as `mutool merge` does (pdfmerge.c): pdf_create_document,
// then for each input a graft map of its own and pdf_graft_mapped_page for
// each page of its range, in order. Links and outlines are not copied yet.
// The new document's /Producer is nupdf's, so that the bytes compare; the
// save is in reproducible mode, as `clean.js` saves.
// Usage: mutool run oracle/js/merge.js <file> <scratch-dir> [options]
// Options, separated by commas: "pages=RANGE" for the file's page range
// (`;` for its commas), and "twice" to merge the file a second time, all
// of it, with a new graft map. The output is <scratch-dir>/merge.pdf.

// fz_parse_page_range over a whole list: 1-based pages, as merge_range
// steps through them.
function parsePages(spec, n) {
  var out = [];
  var s = spec;
  function strtol() {
    var m = /^[ \t\n\r\v\f]*([+-]?[0-9]+)/.exec(s);
    if (!m) return 0;
    s = s.slice(m[0].length);
    return parseInt(m[1], 10);
  }
  function clamp(v) { return v < 1 ? 1 : v > n ? n : v; }
  while (s.length > 0) {
    var orig = s.length;
    if (s[0] == ",") s = s.slice(1);
    var a, b;
    if (s[0] == "N") { a = n; s = s.slice(1); } else a = strtol();
    if (s[0] == "-") {
      if (s[1] == "N") { b = n; s = s.slice(2); }
      else { s = s.slice(1); b = strtol(); }
    } else b = a;
    if (a < 0) a = n + 1 + a;
    if (b < 0) b = n + 1 + b;
    a = clamp(a); b = clamp(b);
    if (s.length == orig) break;
    if (a < b) for (var p = a; p <= b; p++) out.push(p);
    else for (var q = a; q >= b; q--) out.push(q);
  }
  return out;
}

var path = scriptArgs[0];
var range = "1-N";
var times = 1;
if (scriptArgs[2]) {
  scriptArgs[2].split(",").forEach(function (o) {
    if (o.indexOf("pages=") == 0) range = o.slice(6).replace(/;/g, ",");
    else if (o == "twice") times = 2;
  });
}

var dst = new PDFDocument();
dst.getTrailer().get("Root").get("Info").put("Producer", dst.newString("nupdf"));

// nupdf fails only when no input opens; MuPDF would save an empty
// document then.
var opened = 0;
var failed = [];
for (var t = 0; t < times; t++) {
  var spec = t == 0 ? range : "1-N";
  // As pdfmerge.c: an input that fails is reported and skipped, keeping
  // what it merged before failing.
  try {
    var src = Document.openDocument(path).asPDF();
    opened++;
    // A locked input is refused, as every nupdf command that writes
    // refuses one (DECISIONS D24).
    if (src.needsPassword() && !src.authenticatePassword(""))
      throw new Error("cannot authenticate password: " + path);
    var map = dst.newGraftMap();
    parsePages(spec, src.countPages()).forEach(function (p) {
      map.graftPage(-1, src, p - 1);
    });
  } catch (e) {
    failed.push(String(e));
  }
}
if (opened == 0)
  throw new Error("no input opens: " + failed.join("; "));
dst.save(scriptArgs[1] + "/merge.pdf", "reproducible,regenerate-id=no");
print(JSON.stringify({ output: { file: "merge.pdf" } }));
