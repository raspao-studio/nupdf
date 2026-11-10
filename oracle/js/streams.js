// Oracle for the `streams` check: every stream's decoded data.
// Usage: mutool run oracle/js/streams.js <file> <scratch-dir>
// Each decoded stream is written to <scratch-dir>/<num>; the harness
// replaces the "file" field with the BLAKE3 hash of what was written.
// Objects are loaded in ascending order, as in objects.js (looking up a
// method on an indirect object loads it). Streams with an image filter are
// reported as {"image": true}: their pixels are compared by another check.

var doc = Document.openDocument(scriptArgs[0]);
var dir = scriptArgs[1];
var IMAGE_FILTERS = {
  CCITTFaxDecode: true, CCF: true, DCTDecode: true, DCT: true,
  JBIG2Decode: true, JPXDecode: true,
};

// pdf_dict_geta: the abbreviation wins when both keys are present.
function hasImageFilter(dict) {
  var f = dict.get("F");
  if (f === null) f = dict.get("Filter");
  if (f === null) return false;
  // Names can be indirect; String() of a reference is "n g R".
  var named = function (v) {
    v = v.resolve();
    return v.isName() && IMAGE_FILTERS[String(v).slice(1)] === true;
  };
  if (f.isName()) return named(f);
  var image = false;
  if (f.isArray()) {
    f.forEach(function (v) {
      if (named(v)) image = true;
    });
  }
  return image;
}

var count = doc.countObjects();
var out = { object_count: count, streams: {} };
for (var i = 1; i < count; i++) {
  var ref = doc.newIndirect(i, 0);
  if (!ref.isStream()) continue;
  if (hasImageFilter(ref.resolve())) {
    out.streams[i] = { image: true };
    continue;
  }
  try {
    var buf = ref.readStream();
    buf.save(dir + "/" + i);
    out.streams[i] = { len: buf.getLength(), file: String(i) };
  } catch (e) {
    out.streams[i] = { error: String(e.message || e) };
  }
}
print(JSON.stringify(out));
