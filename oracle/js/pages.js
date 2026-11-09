// Oracle for the `pages` check: page tree lookup and inherited attributes.
// Usage: mutool run oracle/js/pages.js <file>
//
// For each index below the page count: the page object MuPDF's lookup returns
// (pdf_lookup_page_obj, which may rewrite /Count when the tree has fewer pages
// than it claims) and the inherited MediaBox, CropBox, Rotate and Resources.
// Values use the encoding of objects.js. Resources are summarized as a
// reference or the sorted list of their keys.

var doc = Document.openDocument(scriptArgs[0]);

function hex(bytes) {
  var s = "";
  for (var i = 0; i < bytes.length; i++) {
    var b = bytes[i];
    s += (b < 16 ? "0" : "") + b.toString(16);
  }
  return s;
}

function name(s) {
  return String(doc.newName(s)).slice(1);
}

function enc(o) {
  if (o === null || o === undefined) return null;
  if (o.isIndirect()) return { ref: String(o) };
  if (o.isNull()) return null;
  if (o.isBoolean()) return o.asBoolean();
  if (o.isInteger()) return Number(String(o));
  if (o.isReal()) return { r: o.asNumber() };
  if (o.isName()) return { n: String(o).slice(1) };
  if (o.isString()) return { s: hex(o.asByteString()) };
  if (o.isArray()) {
    var a = [];
    o.forEach(function (v) { a.push(enc(v)); });
    return a;
  }
  if (o.isDictionary()) {
    var d = {};
    o.forEach(function (v, k) { d[name(k)] = enc(v); });
    return { d: d };
  }
  return { unknown: String(o) };
}

function summary(o) {
  if (o === null || o === undefined) return null;
  if (o.isIndirect()) return { ref: String(o) };
  if (o.isNull()) return null;
  if (!o.isDictionary()) return enc(o);
  var keys = [];
  o.forEach(function (v, k) { keys.push(name(k)); });
  keys.sort();
  return { keys: keys };
}

var out = { page_count: null, pages: [] };
try {
  out.page_count = doc.countPages();
} catch (e) {
  out.page_count_error = String(e.message || e);
}
for (var i = 0; i < (out.page_count || 0); i++) {
  try {
    var page = doc.findPage(i);
    out.pages.push({
      object: page.isIndirect() ? String(page) : "direct",
      MediaBox: enc(page.getInheritable("MediaBox")),
      CropBox: enc(page.getInheritable("CropBox")),
      Rotate: enc(page.getInheritable("Rotate")),
      Resources: summary(page.getInheritable("Resources")),
    });
  } catch (e) {
    out.pages.push({ error: String(e.message || e) });
  }
}
try {
  out.page_count_after = doc.countPages();
} catch (e) {
  out.page_count_after = null;
}
print(JSON.stringify(out));
