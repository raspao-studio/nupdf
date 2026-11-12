// Oracle for the `clean` checks: the document as `mutool clean` writes it,
// in reproducible mode (no version in the comment line) and keeping /ID,
// as `mutool clean` does. Further write options, such as "garbage=2", may
// follow the scratch directory. A "pages=..." option keeps those pages, as
// `mutool clean`'s page arguments do (pdf_rearrange_pages), with ";" for
// the commas in a page list. Edits as the `meta` and `pages` commands make
// them may come too, applied in order: "meta=KEY=VALUE" (setMetaData, as
// pdf_set_metadata), "strip-meta" (drops /Info and the catalog's
// /Metadata), "rotate=DEGREES@PAGES" (adds to each page's inherited
// /Rotate, put on the page) and "bake", "bake-annots" or "bake-widgets"
// (pdf_bake_document, as `flatten` bakes). Form edits as `forms --set`
// and `--toggle` make them come as "fill=NAME=VALUE" (setTextValue, or setChoiceValue for
// a choice field, on the first widget of the field with that full name),
// "toggle=NAME" (toggle) and "fill-first" (the first text widget set to
// "nupdf", then the first check box toggled); every page is loaded first
// and kept open, and each page is updated after the edits. JavaScript is
// off, as `mutool run` leaves it.
// With "annots", the arguments
// after the options are annotation edits as `annots` makes them:
// "delete=p1.a2" (page.deleteAnnotation) and "add=JSON" (createAnnotation,
// then the setters in `annots --add`'s order), deletions first, then each
// page updated.
// Usage: mutool run oracle/js/clean.js <file> <scratch-dir> [options] [edits]
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

// Every page, loaded once and kept open, as filling needs them.
var openPages = null;
function loadPages() {
  if (openPages) return openPages;
  openPages = [];
  for (var i = 0; i < pdf.countPages(); i++) openPages.push(pdf.loadPage(i));
  return openPages;
}

function widgetNamed(name) {
  var pages = loadPages();
  for (var i = 0; i < pages.length; i++) {
    var ws = pages[i].getWidgets();
    for (var k = 0; k < ws.length; k++) {
      var n;
      try { n = ws[k].getName(); } catch (e) { continue; }
      if (n === name) return ws[k];
    }
  }
  throw new Error("no field named '" + name + "'");
}

function firstWidget(type) {
  var pages = loadPages();
  for (var i = 0; i < pages.length; i++) {
    var ws = pages[i].getWidgets();
    for (var k = 0; k < ws.length; k++) {
      try { ws[k].getName(); } catch (e) { continue; }
      if (ws[k].getFieldType() === type) return ws[k];
    }
  }
  return null;
}

function setValue(w, value) {
  var ty = w.getFieldType();
  if (ty === "combobox" || ty === "listbox") w.setChoiceValue(value);
  else w.setTextValue(value);
}

var filled = false;
function fill(o) {
  check();
  filled = true;
  if (o == "fill-first") {
    var t = firstWidget("text");
    if (t) setValue(t, "nupdf");
    var c = firstWidget("checkbox");
    if (c) c.toggle();
  } else if (o.indexOf("fill=") == 0) {
    var kv = o.slice(5);
    var eq = kv.indexOf("=");
    setValue(widgetNamed(kv.slice(0, eq)), kv.slice(eq + 1));
  } else {
    widgetNamed(o.slice(7)).toggle();
  }
}

// `annots --add/--delete`: the pages loaded first, in the order first
// named (MuPDF updates its open pages newest first); the deletions by the
// listing before any of them; then each addition's setters in the order
// `annots` applies them; then the pages updated.
function annotEdits(specs) {
  var adds = [], dels = [], needles = [];
  specs.forEach(function (s) {
    if (s.indexOf("add=") == 0) adds.push(JSON.parse(s.slice(4)));
    else if (s.indexOf("highlight=") == 0) needles.push(s.slice(10));
    else if (s.indexOf("delete=") == 0) {
      var m = /^p([0-9]+)\.a([0-9]+)$/.exec(s.slice(7));
      if (!m) throw new Error("not an annotation id: " + s.slice(7));
      dels.push([parseInt(m[1], 10) - 1, parseInt(m[2], 10) - 1, s.slice(7)]);
    } else throw new Error("unknown annotation edit: " + s);
  });
  var n = pdf.countPages();
  var order = [], pages = {}, hits = [];
  // "highlight=TEXT": a Highlight over each hit, case and all, every page
  // loaded first and searched before any change.
  if (needles.length) {
    for (var p = 0; p < n; p++) {
      order.push(p);
      pages[p] = pdf.loadPage(p);
    }
    for (var q = 0; q < n; q++)
      needles.forEach(function (t) {
        pages[q].search(t, 0, 1000000).forEach(function (h) { hits.push([q, h]); });
      });
  }
  dels.map(function (d) { return d[0]; })
    .concat(adds.map(function (a) { return a.page - 1; }))
    .forEach(function (p) { if (order.indexOf(p) < 0) order.push(p); });
  order.forEach(function (p) {
    if (p >= n) throw new Error("no page " + (p + 1) + " (the document has " + n + ")");
    pages[p] = pdf.loadPage(p);
  });
  var targets = dels.map(function (d) {
    var list = pages[d[0]].getAnnotations();
    if (d[1] >= list.length) throw new Error("no annotation " + d[2]);
    return [d[0], list[d[1]]];
  });
  targets.forEach(function (t) { pages[t[0]].deleteAnnotation(t[1]); });
  adds.forEach(function (a) {
    var annot = pages[a.page - 1].createAnnotation(a.type);
    var width = a.border_width;
    if (width === undefined && a.border) width = a.border.width;
    if (a.rect) annot.setRect(a.rect);
    if (a.popup) annot.setPopup(a.popup);
    if (a.quad_points) annot.setQuadPoints(a.quad_points);
    if (a.ink_list) annot.setInkList(a.ink_list);
    if (a.line) annot.setLine(a.line[0], a.line[1]);
    if (a.vertices) annot.setVertices(a.vertices);
    if (a.contents !== undefined) annot.setContents(a.contents);
    if (a.author !== undefined) annot.setAuthor(a.author);
    if (a.subject !== undefined) annot.setSubject(a.subject);
    if (a.color) annot.setColor(a.color);
    if (a.interior_color) annot.setInteriorColor(a.interior_color);
    if (a.opacity !== undefined) annot.setOpacity(a.opacity);
    if (a.icon !== undefined) annot.setIcon(a.icon);
    if (width !== undefined) annot.setBorderWidth(width);
    if (a.flags !== undefined) annot.setFlags(a.flags);
    if (a.open !== undefined) annot.setIsOpen(a.open);
  });
  hits.forEach(function (h) {
    pages[h[0]].createAnnotation("Highlight").setQuadPoints(h[1]);
  });
  order.forEach(function (p) { pages[p].update(); });
}

if (scriptArgs[2]) {
  var rest = [];
  scriptArgs[2].split(",").forEach(function (o) {
    if (o == "annots") {
      check();
      annotEdits(scriptArgs.slice(3));
    } else if (o.indexOf("pages=") == 0) {
      check();
      pdf.rearrangePages(pageList(o.slice(6)));
    } else if (o.indexOf("meta=") == 0) {
      check();
      var kv = o.slice(5);
      var eq = kv.indexOf("=");
      pdf.setMetaData(kv.slice(0, eq), kv.slice(eq + 1));
    } else if (o == "bake" || o == "bake-annots" || o == "bake-widgets") {
      check();
      pdf.bake(o != "bake-widgets", o != "bake-annots");
    } else if (o == "strip-meta") {
      check();
      stripMeta();
    } else if (o == "fill-first" || o.indexOf("fill=") == 0 || o.indexOf("toggle=") == 0) {
      fill(o);
    } else if (o.indexOf("rotate=") == 0) {
      check();
      rotate(o.slice(7));
    } else {
      // mutool clean loads every object before cleaning content streams.
      if (o == "clean" || o == "sanitize") check();
      rest.push(o);
    }
  });
  if (rest.length) options += "," + rest.join(",");
}
if (filled) loadPages().forEach(function (page) { page.update(); });
pdf.save(scriptArgs[1] + "/clean.pdf", options);
print(JSON.stringify({ output: { file: "clean.pdf" } }));
