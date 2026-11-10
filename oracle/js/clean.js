// Oracle for the `clean` checks: the document as `mutool clean` writes it,
// in reproducible mode (no version in the comment line) and keeping /ID,
// as `mutool clean` does. Further write options, such as "garbage=2", may
// follow the scratch directory. A "pages=..." option keeps those pages, as
// `mutool clean`'s page arguments do (pdf_rearrange_pages), with ";" for
// the commas in a page list.
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
if (scriptArgs[2]) {
  var rest = [];
  scriptArgs[2].split(",").forEach(function (o) {
    if (o.indexOf("pages=") == 0) {
      // pdf_clean_file loads every object first (pdf_check_document), so
      // a repair happens before the edits, which it would lose.
      for (var i = 1; i < pdf.countObjects(); i++) {
        try { pdf.newIndirect(i, 0).resolve(); } catch (e) {}
      }
      // Commas separate the options, so ";" stands for one in the pages.
      pdf.rearrangePages(parsePages(o.slice(6).replace(/;/g, ","), pdf.countPages()));
    }
    else
      rest.push(o);
  });
  if (rest.length) options += "," + rest.join(",");
}
pdf.save(scriptArgs[1] + "/clean.pdf", options);
print(JSON.stringify({ output: { file: "clean.pdf" } }));
