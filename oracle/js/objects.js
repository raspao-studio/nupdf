// Oracle for the `objects` check: every object in the xref as canonical JSON.
// Usage: mutool run oracle/js/objects.js <file>
//
// Encoding (shared with `nupdf objects`):
//   null, true/false, integers as numbers, reals as {"r": n},
//   names as {"n": "..."} in MuPDF's #xx-escaped form (so output is ASCII),
//   strings as {"s": "<hex>"}, references as
//   {"ref": "num gen R"}, arrays as [...], dictionaries as {"d": {...}},
//   streams as {"stream": {"d": ...}, "raw_len": bytes}.
// Objects are loaded in ascending order, so a lazy repair happens at the
// same point as in nupdf.

var doc = Document.openDocument(scriptArgs[0]);

function hex(bytes) {
  var s = "";
  for (var i = 0; i < bytes.length; i++) {
    var b = bytes[i];
    s += (b < 16 ? "0" : "") + b.toString(16);
  }
  return s;
}

// MuPDF's printed form of a name, without the leading slash.
function name(s) {
  return String(doc.newName(s)).slice(1);
}

function enc(o) {
  if (o === null || o === undefined) return null;
  if (o.isIndirect()) return { ref: String(o) };
  if (o.isNull()) return null;
  if (o.isBoolean()) return o.asBoolean();
  // asNumber() squeezes ints through an int and a float; print them instead.
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

var count = doc.countObjects();
var out = { object_count: count, objects: {} };
for (var i = 1; i < count; i++) {
  var ref = doc.newIndirect(i, 0);
  var o = ref.resolve();
  var e = o === null || o === undefined ? null : enc(o);
  if (ref.isStream()) {
    var len = null;
    try { len = ref.readRawStream().length; } catch (err) {}
    e = { stream: e, raw_len: len };
  }
  out.objects[i] = e;
}
print(JSON.stringify(out));
