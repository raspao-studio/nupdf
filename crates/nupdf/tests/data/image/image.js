// mutool run image.js file.pdf: object 2 loaded as an image
// (pdf_load_image), one line summarizing what MuPDF made of it, in the
// form the tests in src/image.rs compare against.
var doc = Document.openDocument(scriptArgs[0]);

function list(a) {
  if (a === null || a === undefined) return "-";
  // Four decimals, as the tests print them.
  return "[" + Array.prototype.map.call(a, function (v) { return Math.round(v * 1e4) / 1e4; }).join(" ") + "]";
}

function summary(img) {
  var cs = img.getColorSpace();
  var mask = img.getMask();
  return (
    img.getWidth() + "x" + img.getHeight() +
    " bpc " + img.getBitsPerComponent() +
    " n " + img.getNumberOfComponents() +
    // This build registers no wrapper for the Lab space: it reads as
    // undefined.
    " cs " + (cs === null ? "-" : cs === undefined ? "Lab" : cs.getType()) +
    " im " + img.getImageMask() +
    " interp " + img.getInterpolate() +
    " decode " + list(img.getDecode()) +
    " key " + list(img.getColorKey()) +
    (mask === null ? "" : " mask (" + summary(mask) + ")")
  );
}

try {
  print(summary(doc.loadImage(doc.newIndirect(2, 0))));
} catch (e) {
  print("error " + String(e.message || e));
}
