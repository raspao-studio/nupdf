// Oracle for the `clean` checks: the document as `mutool clean` writes it,
// in reproducible mode (no version in the comment line) and keeping /ID,
// as `mutool clean` does. Further write options, such as "garbage=2", may
// follow the scratch directory. A "pages=..." option keeps those pages, as
// `mutool clean`'s page arguments do (pdf_rearrange_pages), with ";" for
// the commas in a page list. Edits as the `meta` and `pages` commands make
// them may come too, applied in order: "meta=KEY=VALUE" (setMetaData, as
// pdf_set_metadata), "strip-meta" (drops /Info and the catalog's
// /Metadata) and "rotate=DEGREES@PAGES" (adds to each page's inherited
// /Rotate, put on the page).
// Usage: mutool run oracle/js/clean.js <file> <scratch-dir> [options]
// The output is written to <scratch-dir>/clean.pdf; the harness replaces
// the "file" field with the BLAKE3 hash of what was written, after naming
// nupdf in the comment line (both lines are the same length).

// A failed save is an error, so the harness sees MuPDF fail.

// fz_parse_page_range over a whole list: 0-based page indices.
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
    if (a < b) for (var p = a; p <= b; p++) out.push(p - 1);
    else for (var q = a; q >= b; q--) out.push(q - 1);
  }
  return out;
}

var doc = Document.openDocument(scriptArgs[0]);
var pdf = doc.asPDF ? doc.asPDF() : doc;
// pdf_clean_file: without -p, only the empty password is tried.
if (pdf.needsPassword() && !pdf.authenticatePassword(""))
  throw new Error("cannot authenticate password: " + scriptArgs[0]);
var options = "reproducible,regenerate-id=no";
// pdf_clean_file loads every object first (pdf_check_document), so a
// repair happens before the edits, which it would lose.
var checked = false;
function check() {
  if (checked) return;
  checked = true;
  for (var i = 1; i < pdf.countObjects(); i++) {
    try { pdf.newIndirect(i, 0).resolve(); } catch (e) {}
  }
}

// Commas separate the options, so ";" stands for one in a page list.
function pageList(spec) {
  return parsePages(spec.replace(/;/g, ","), pdf.countPages());
}

function rotate(spec) {
  var at = spec.indexOf("@");
  var degrees = parseInt(at < 0 ? spec : spec.slice(0, at), 10);
  var pages = at < 0 ? null : pageList(spec.slice(at + 1));
  if (pages == null) {
    pages = [];
    for (var p = 0; p < pdf.countPages(); p++) pages.push(p);
  }
  pages.forEach(function (i) {
    var page = pdf.findPage(i);
    var r = page.getInheritable("Rotate");
    // pdf_to_int: reals round, anything else is 0.
    var current = r && r.isNumber() ? Math.floor(r.asNumber() + 0.5) : 0;
    page.put("Rotate", (((current + degrees) % 360) + 360) % 360);
  });
}

// A trailer that is an xref stream's dictionary belongs to that object, so
// MuPDF's first edit to it moves the object into a new incremental section
// whose trailer is copied before the edit, and the copy is what it writes.
// The second delete edits the copy, so the entry is gone either way.
function stripMeta() {
  pdf.getTrailer().delete("Info");
  pdf.getTrailer().delete("Info");
  var root = pdf.getTrailer().get("Root");
  if (root && root.isDictionary()) root.delete("Metadata");
}

if (scriptArgs[2]) {
  var rest = [];
  scriptArgs[2].split(",").forEach(function (o) {
    if (o.indexOf("pages=") == 0) {
      check();
      pdf.rearrangePages(pageList(o.slice(6)));
    } else if (o.indexOf("meta=") == 0) {
      check();
      var kv = o.slice(5);
      var eq = kv.indexOf("=");
      pdf.setMetaData(kv.slice(0, eq), kv.slice(eq + 1));
    } else if (o == "strip-meta") {
      check();
      stripMeta();
    } else if (o.indexOf("rotate=") == 0) {
      check();
      rotate(o.slice(7));
    } else
      rest.push(o);
  });
  if (rest.length) options += "," + rest.join(",");
}
pdf.save(scriptArgs[1] + "/clean.pdf", options);
print(JSON.stringify({ output: { file: "clean.pdf" } }));
