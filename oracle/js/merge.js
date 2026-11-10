// Oracle for the `merge` checks: one or more documents merged into a new
// one, as `mutool merge` does (pdfmerge.c, mirrored step for step):
// pdf_create_document; then for each input a graft map of its own,
// pdf_graft_mapped_page for each page of its range with the page's
// external links recreated on the copy, and the outline items that lead
// to merged pages, with their ancestors, appended to the outline. The new
// document's /Producer is nupdf's, so that the bytes compare; the save is
// in reproducible mode, as `clean.js` saves.
// Usage: mutool run oracle/js/merge.js <file> <scratch-dir> [options]
// Options, separated by commas: "pages=RANGE" for the file's page range
// (`;` for its commas), and "twice" to merge the file a second time, all
// of it, with a new graft map. The output is <scratch-dir>/merge.pdf.

// fz_parse_page_range: each range as [first, last], 1-based and clamped.
function parseRanges(spec, n) {
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
    out.push([a, b]);
  }
  return out;
}

// position_in_range, with its count for backward ranges.
function positionInRange(range, count, page) {
  var n = 0;
  var pairs = parseRanges(range, count);
  for (var i = 0; i < pairs.length; i++) {
    var start = pairs[i][0], end = pairs[i][1];
    if (start < end) {
      if (start <= page && page <= end) return n + page - start + 1;
      n += end - start + 1;
    } else {
      if (end <= page && page <= start) return n + page - end + 1;
      n += start - end + 1;
    }
  }
  return 0;
}

function rewritePage(uri, n) {
  if (uri === undefined || uri === null) return uri;
  if (uri.indexOf("#page=") != 0) return uri;
  var amp = uri.indexOf("&", 6);
  return amp < 0 ? "#page=" + n : "#page=" + n + uri.slice(amp);
}

function outlineItem(title, uri, open) {
  var item = { open: open };
  if (title !== undefined && title !== null) item.title = title;
  if (uri !== undefined && uri !== null) item.uri = uri;
  return item;
}

function copyOutlineRange(itDst, itSrc, src, range, count, base) {
  var items = [];
  var copied = 0;
  function copyItem() {
    while (copied < items.length) {
      var it = items[copied];
      itDst.insert(outlineItem(it.title, items[items.length - 1].uri, it.open));
      copied++;
      itDst.prev();
      itDst.down();
    }
  }
  function doRange() {
    do {
      var item = itSrc.item();
      var uri = item.uri === undefined || item.uri === null ? "" : item.uri;
      var pageNum = src.resolveLink(uri);
      var inRange = positionInRange(range, count, pageNum + 1);
      items.push({ title: item.title, uri: rewritePage(item.uri, inRange + base), open: item.open });
      if (inRange != 0) copyItem();
      var hasChildren = itSrc.down();
      if (hasChildren == 0) doRange();
      if (hasChildren >= 0) itSrc.up();
      items.pop();
      if (copied > items.length) {
        copied = items.length;
        itDst.up();
      }
      itDst.next();
    } while (itSrc.next() == 0);
  }
  doRange();
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
var outputPageCount = 0;
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
    var count = src.countPages();
    var map = dst.newGraftMap();
    var merged = 0;
    parseRanges(spec, count).forEach(function (r) {
      var step = r[0] < r[1] ? 1 : -1;
      for (var p = r[0]; ; p += step) {
        map.graftPage(-1, src, p - 1);
        var pageSrc = src.loadPage(p - 1);
        var pageDst = dst.loadPage(dst.countPages() - 1);
        var links = pageSrc.getLinks();
        for (var i = 0; i < links.length; i++)
          if (links[i].isExternal())
            pageDst.createLink(links[i].getBounds(), links[i].getURI());
        merged++;
        if (p == r[1]) break;
      }
    });
    var itSrc = src.outlineIterator();
    var itDst = dst.outlineIterator();
    if (itDst.item()) while (itDst.next() == 0);
    if (itSrc.item()) copyOutlineRange(itDst, itSrc, src, spec, count, outputPageCount);
    outputPageCount += merged;
  } catch (e) {
    failed.push(String(e));
  }
}
if (opened == 0)
  throw new Error("no input opens: " + failed.join("; "));
dst.save(scriptArgs[1] + "/merge.pdf", "reproducible,regenerate-id=no");
print(JSON.stringify({ output: { file: "merge.pdf" } }));
