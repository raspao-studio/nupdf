// Judge for the redaction tier: where MuPDF finds a secret in a document.
// Each page's text with its annotations (Page.toStructuredText, all
// optional content as the default view shows it), every string of every
// object, and every stream's decoded data that is not an image or a font.
// Usage: mutool run oracle/js/leaks.js <file> <secret>...
// Prints {pages: [n...], objects: [num...], found: [secret...]} of where
// any secret is.

var doc = Document.openDocument(scriptArgs[0]);
var secrets = scriptArgs.slice(1);
var found = {};
function hit(s) {
  var any = false;
  secrets.forEach(function (x) { if (s.indexOf(x) >= 0) { found[x] = 1; any = true; } });
  return any;
}
var pdf = doc.asPDF ? doc.asPDF() : doc;
var out = { pages: [], objects: [], found: [] };
for (var p = 0; p < doc.countPages(); p++) {
  try {
    var text = doc.loadPage(p).toStructuredText("").asText();
    if (hit(text.replace(/\s+/g, " "))) out.pages.push(p + 1);
  } catch (e) {}
}
function has(o, depth) {
  if (depth > 32) return false;
  if (o.isString()) return hit(o.asString());
  if (o.isName()) return hit(o.asName());
  if (o.isArray()) {
    for (var i = 0; i < o.length; i++) if (has(o.get(i), depth + 1)) return true;
    return false;
  }
  if (o.isDictionary()) {
    var found = false;
    o.forEach(function (v, k) {
      if (!found && !v.isIndirect() && has(v, depth + 1)) found = true;
    });
    return found;
  }
  return false;
}
for (var i = 1; i < pdf.countObjects(); i++) {
  try {
    var obj = pdf.newIndirect(i, 0).resolve();
    var found_hit = has(obj, 0);
    if (!found_hit && obj.isStream()) {
      var st = obj.get("Subtype");
      var image = st && st.isName() && st.asName() == "Image";
      var font = obj.get("Length1") || obj.get("Length2") || obj.get("Length3") ||
        (st && st.isName() && (st.asName() == "Type1C" || st.asName() == "CIDFontType0C" || st.asName() == "OpenType"));
      if (!image && !font) {
        var data = obj.readStream().asString();
        if (hit(data)) found_hit = true;
      }
    }
    if (found_hit) out.objects.push(i);
  } catch (e) {}
}
out.found = Object.keys(found);
print(JSON.stringify(out));
