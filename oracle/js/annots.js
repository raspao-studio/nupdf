// Oracle for the annots check: each page's annotations as MuPDF lists
// them, with what its getters read of each, in the JSON `nupdf annots`
// gives.
// Usage: mutool run oracle/js/annots.js <file> [synthesize]
//
// With `synthesize`, each annotation's appearance is made again
// (requestResynthesis, then update) and given as `nupdf annots --synthesize`
// gives it: the /Rect and /RD written back, and the new form XObject's
// BBox, Matrix, Resources and content (each byte as one character).
// Objects the synthesis made (fonts, numbered past the file's objects) are
// given inline where they are referenced, as nupdf keeps them direct; the
// file's own objects stay references. An annotation whose appearance was
// not replaced (a Stamp with a non-standard name and an appearance, which
// pdf_update_appearance leaves alone) has no synthesis.

var doc = Document.openDocument(scriptArgs[0]);
var synthesize = scriptArgs[1] === "synthesize";
var fileObjects = doc.countObjects();

function hex(bytes) {
  var s = "";
  for (var i = 0; i < bytes.length; i++) {
    var b = bytes[i];
    s += (b < 16 ? "0" : "") + b.toString(16);
  }
  return s;
}

// The objects check's encoding (oracle/js/objects.js).
function enc(o) {
  if (o === null || o === undefined) return null;
  if (o.isIndirect() && o.asIndirect() < fileObjects) return { ref: String(o) };
  if (o.isIndirect()) return enc(o.resolve());
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
    o.forEach(function (v, k) { d[String(doc.newName(k)).slice(1)] = enc(v); });
    return { d: d };
  }
  return { unknown: String(o) };
}

function numbers(o, n) {
  var a = [];
  for (var i = 0; i < n; i++) a.push(o.get(i).asNumber());
  return a;
}

// The number of the annotation's normal appearance stream, if it has one.
function appearanceNumber(a) {
  var ap = a.getObject().get("AP");
  var form = !ap || ap.isNull() ? null : ap.get("N");
  return form && form.isIndirect() && form.isStream() ? form.asIndirect() : null;
}

function synthesis(a, before) {
  var o = a.getObject();
  var ap = o.get("AP");
  var form = !ap || ap.isNull() ? null : ap.get("N");
  if (!form || !form.isStream()) return null;
  if (form.isIndirect() && form.asIndirect() === before) return null;
  var buf = form.readStream();
  var content = "";
  for (var i = 0; i < buf.getLength(); i++) content += String.fromCharCode(buf.readByte(i));
  var res = form.get("Resources");
  return {
    rect: enc(o.get("Rect")),
    rd: enc(o.get("RD")),
    bbox: numbers(form.get("BBox"), 4),
    matrix: numbers(form.get("Matrix"), 6),
    resources: !res || res.isNull() ? null : enc(res.resolve()),
    content: content,
  };
}
var n = doc.countPages();
var report = { page_count: n, pages: [] };

function time(d) {
  var t = Math.round(d.getTime() / 1000);
  return t === -1 ? null : t;
}

for (var i = 0; i < n; i++) {
  var entry = { page: i + 1 };
  try {
    // As the stext oracle loads pages: the page object first.
    var obj = doc.findPage(i);
    if (!obj.isDictionary()) throw new Error("page object is not a dictionary");
    var page = doc.loadPage(i);
    var annots = page.getAnnotations();
    var made = [];
    var before = [];
    if (synthesize) {
      for (var k = 0; k < annots.length; k++) before.push(appearanceNumber(annots[k]));
      for (var k = 0; k < annots.length; k++) annots[k].requestResynthesis();
      for (var k = 0; k < annots.length; k++) made.push(annots[k].update());
    }
    entry.annots = [];
    for (var k = 0; k < annots.length; k++) {
      var a = annots[k];
      var o = a.getObject();
      var v = {
        id: "p" + (i + 1) + ".a" + (k + 1),
        object: o.isIndirect() ? o.asIndirect() : null,
        type: a.getType(),
        flags: a.getFlags(),
        contents: a.getContents(),
        color: a.getColor(),
        opacity: a.getOpacity(),
        created: time(a.getCreationDate()),
        modified: time(a.getModificationDate()),
      };
      if (a.hasRect()) v.rect = a.getRect();
      if (a.hasAuthor()) v.author = a.getAuthor();
      if (a.hasSubject()) v.subject = a.getSubject();
      if (a.hasInteriorColor()) v.interior_color = a.getInteriorColor();
      if (a.hasIcon()) v.icon = a.getIcon();
      if (a.hasOpen()) v.open = !!a.getIsOpen();
      if (a.hasBorder()) v.border = { style: a.getBorderStyle(), width: a.getBorderWidth() };
      if (a.hasLine()) v.line = a.getLine();
      if (a.hasVertices()) v.vertices = a.getVertices();
      if (a.hasQuadPoints()) v.quad_points = a.getQuadPoints();
      if (a.hasInkList()) v.ink_list = a.getInkList();
      if (synthesize) v.synthesis = made[k] ? synthesis(a, before[k]) : null;
      entry.annots.push(v);
    }
  } catch (e) {
    entry.error = String(e.message || e);
  }
  report.pages.push(entry);
}
print(JSON.stringify(report));
