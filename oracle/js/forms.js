// Oracle for the forms check: each page's widgets as `Page.getWidgets`
// lists them, with what the widget getters read of each field, in the JSON
// `nupdf forms` gives.
// Usage: mutool run oracle/js/forms.js <file> [synthesize]
//
// With `synthesize`, each widget's appearance is made again
// (requestResynthesis, then update) and given as `nupdf forms
// --synthesize` gives it, as the annots oracle gives annotations': the
// /Rect and /RD written back and the new form XObject's BBox, Matrix,
// Resources and content. A button has a stream per state: its /AS and,
// under `states`, each stream by its /AP key, and its state after a slash
// for a check box or radio button ("N/Off"). Objects the synthesis made are
// given inline; a widget whose appearance was not made again (a signed
// signature) has no synthesis.

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

// The objects check's encoding, with new objects inline (annots.js).
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

function isNew(f) {
  return f && f.isStream() && f.isIndirect() && f.asIndirect() >= fileObjects;
}

function form(f) {
  var buf = f.readStream();
  var content = "";
  for (var i = 0; i < buf.getLength(); i++) content += String.fromCharCode(buf.readByte(i));
  var res = f.get("Resources");
  return {
    bbox: numbers(f.get("BBox"), 4),
    matrix: numbers(f.get("Matrix"), 6),
    resources: !res || res.isNull() ? null : enc(res.resolve()),
    content: content,
  };
}

function synthesis(w, ty) {
  var o = w.getObject();
  var ap = o.get("AP");
  if (!ap || ap.isNull()) return null;
  var out = { rect: enc(o.get("Rect")), rd: enc(o.get("RD")) };
  if (ty === "button" || ty === "checkbox" || ty === "radiobutton") {
    var states = {}, made = false;
    ["N", "D"].forEach(function (k) {
      var v = ap.get(k);
      if (!v || v.isNull()) return;
      if (v.isStream()) {
        made = made || isNew(v);
        states[k] = form(v);
      } else if (v.isDictionary()) {
        v.forEach(function (f, name) {
          if (!f.isStream()) return;
          made = made || isNew(f);
          states[k + "/" + String(doc.newName(name)).slice(1)] = form(f);
        });
      }
    });
    if (!made) return null;
    out.as = enc(o.get("AS"));
    out.states = states;
    return out;
  }
  var n = ap.get("N");
  if (!isNew(n)) return null;
  var f = form(n);
  for (var k in f) out[k] = f[k];
  return out;
}
var n = doc.countPages();
var report = { page_count: n, pages: [] };

for (var i = 0; i < n; i++) {
  var entry = { page: i + 1 };
  try {
    var obj = doc.findPage(i);
    if (!obj.isDictionary()) throw new Error("page object is not a dictionary");
    var page = doc.loadPage(i);
    var widgets = page.getWidgets();
    var made = [];
    if (synthesize) {
      for (var k = 0; k < widgets.length; k++) widgets[k].requestResynthesis();
      for (var k = 0; k < widgets.length; k++) made.push(widgets[k].update());
    }
    entry.widgets = [];
    for (var k = 0; k < widgets.length; k++) {
      var w = widgets[k];
      var o = w.getObject();
      var ty = w.getFieldType();
      var v = {
        id: "p" + (i + 1) + ".w" + (k + 1),
        object: o.isIndirect() ? o.asIndirect() : null,
        type: ty,
        flags: w.getFieldFlags(),
        value: w.getValue(),
        label: w.getLabel(),
        read_only: !!w.isReadOnly(),
      };
      try {
        v.name = w.getName();
      } catch (e) {
        v.name_error = String(e.message || e);
      }
      if (w.hasRect()) v.rect = w.getRect();
      if (ty === "text") v.max_len = w.getMaxLen();
      if (ty === "combobox" || ty === "listbox") {
        var ex = w.getOptions(true), shown = w.getOptions(false);
        v.options = [];
        for (var j = 0; j < ex.length; j++) v.options.push({ export: ex[j], text: shown[j] });
      }
      if (ty === "signature") v.signed = !!w.isSigned();
      if (synthesize) v.synthesis = made[k] ? synthesis(w, ty) : null;
      entry.widgets.push(v);
    }
  } catch (e) {
    entry.error = String(e.message || e);
  }
  report.pages.push(entry);
}
var pdf = doc.asPDF();
function get(o, k) {
  return o && !o.isNull() && (o.isDictionary() || o.isStream()) ? o.get(k) : null;
}
var acro = pdf ? get(get(pdf.getTrailer(), "Root"), "AcroForm") : null;
var na = get(acro, "NeedAppearances");
report.need_appearances = !!(na && na.isBoolean() && na.asBoolean());
report.xfa = !!(get(acro, "XFA") && !get(acro, "XFA").isNull());
print(JSON.stringify(report));
