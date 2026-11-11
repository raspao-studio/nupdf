// Oracle for the `operators` checks: each page's content-stream operators,
// as MuPDF's interpreter hands them to a processor (PDFPage.process).
// Usage: mutool run oracle/js/operators.js <file> [bare]
//
// Each operator is an array: its name (the processor callback's, without
// "op_"), then its operands. Numbers are as the interpreter passes them,
// strings hex, names in MuPDF's printed form, objects in the encoding of
// objects.js. With "bare", each page loses its /Annots and gets empty
// /Resources before it is loaded, so that only its content streams count:
// every resource lookup fails. Two operators are left out, for quirks of
// the script glue: `"`, whose op_dquote is called with the wrong
// arguments, and BI, whose op_BI crashes the script engine for an image
// without a /CS name (it pushes a null string).

var doc = Document.openDocument(scriptArgs[0]);
var bare = scriptArgs[1] === "bare";

function hex2(b) {
  return (b < 16 ? "0" : "") + b.toString(16);
}

// A byte string: a JS string when it is ASCII, else an array of bytes.
function bytes(v) {
  var s = "";
  if (typeof v === "string") {
    for (var i = 0; i < v.length; i++) s += hex2(v.charCodeAt(i));
  } else {
    for (var j = 0; j < v.length; j++) s += hex2(v[j]);
  }
  return s;
}

function name(s) {
  if (s === null || s === undefined) return null;
  return String(doc.newName(s)).slice(1);
}

function hexObj(b) {
  var s = "";
  for (var i = 0; i < b.length; i++) s += hex2(b[i]);
  return s;
}

function enc(o) {
  if (o === null || o === undefined) return null;
  if (o.isIndirect()) return { ref: String(o) };
  if (o.isNull()) return null;
  if (o.isBoolean()) return o.asBoolean();
  if (o.isInteger()) return Number(String(o));
  if (o.isReal()) return { r: o.asNumber() };
  if (o.isName()) return { n: String(o).slice(1) };
  if (o.isString()) return { s: hexObj(o.asByteString()) };
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

var ops = [];

function plain(op) {
  return function () {
    var a = [op];
    for (var i = 0; i < arguments.length; i++) a.push(arguments[i]);
    ops.push(a);
  };
}

function named(op) {
  return function (n) { ops.push([op, name(n)]); };
}

var proc = {
  op_d: function (array, phase) { ops.push(["d", array, phase]); },
  op_ri: named("ri"),
  op_gs: named("gs"),
  op_Tf: function (n, size) { ops.push(["Tf", name(n), size]); },
  op_TJ: function (array) {
    var a = [];
    for (var i = 0; i < array.length; i++)
      a.push(typeof array[i] === "number" ? array[i] : bytes(array[i]));
    ops.push(["TJ", a]);
  },
  op_Tj: function (s) { ops.push(["Tj", bytes(s)]); },
  op_squote: function (s) { ops.push(["squote", bytes(s)]); },
  op_CS: named("CS"),
  op_cs: named("cs"),
  op_SC_pattern: function (n, id, color) { ops.push(["SC_pattern", name(n), color]); },
  op_sc_pattern: function (n, id, color) { ops.push(["sc_pattern", name(n), color]); },
  op_SC_shade: named("SC_shade"),
  op_sc_shade: named("sc_shade"),
  op_sh: named("sh"),
  op_Do_image: named("Do_image"),
  op_Do_form: named("Do_form"),
  op_MP: named("MP"),
  op_DP: function (tag, raw) { ops.push(["DP", name(tag), enc(raw)]); },
  op_BMC: named("BMC"),
  op_BDC: function (tag, raw) { ops.push(["BDC", name(tag), enc(raw)]); },
};
[
  "w", "j", "J", "M", "i", "q", "Q", "cm", "m", "l", "c", "v", "y", "h", "re",
  "S", "s", "F", "f", "fstar", "B", "Bstar", "b", "bstar", "n", "W", "Wstar",
  "BT", "ET", "Tc", "Tw", "Tz", "TL", "Tr", "Ts", "Td", "TD", "Tm", "Tstar",
  "d0", "d1", "SC_color", "sc_color", "G", "g", "RG", "rg", "K", "k", "EMC",
  "BX", "EX",
].forEach(function (op) { proc["op_" + op] = plain(op); });

var out = { page_count: null, pages: [] };
try {
  out.page_count = doc.countPages();
} catch (e) {
  out.page_count_error = String(e.message || e);
}
for (var p = 0; p < (out.page_count || 0); p++) {
  ops = [];
  var entry = { ops: ops };
  try {
    var obj = doc.findPage(p);
    if (!obj.isDictionary()) throw new Error("page object is not a dictionary");
    if (bare) {
      obj.delete("Annots");
      obj.put("Resources", doc.newDictionary());
    }
    var page = doc.loadPage(p);
    try {
      page.process(proc);
    } catch (e) {
      entry.error = String(e.message || e);
    }
  } catch (e) {
    entry = { error: String(e.message || e) };
  }
  out.pages.push(entry);
}
print(JSON.stringify(out));
