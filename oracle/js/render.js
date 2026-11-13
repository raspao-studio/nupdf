// Oracle for the `render` checks: each page drawn into a white DeviceRGB
// pixmap, written as PPM into the scratch directory. By default, the
// page's contents as Page.runPageContents draws them (no annotations: each
// page loses its /Annots before it is loaded, as for the stext checks);
// with "annots", the whole page as Page.run (fz_run_page, what
// `mutool draw` draws) draws it: contents, annotations, then widgets.
// Usage: mutool run oracle/js/render.js <file> <dir> <dpi> [icc] [annots]
//
// Color management is off unless "icc" is given, so colors convert by
// MuPDF's device formulas.

var doc = Document.openDocument(scriptArgs[0]);
var dir = scriptArgs[1];
var dpi = Number(scriptArgs[2] || "72");
var flags = scriptArgs.slice(3);
var icc = flags.indexOf("icc") >= 0;
var annots = flags.indexOf("annots") >= 0;
if (!icc) disableICC();
var zoom = dpi / 72;
var ctm = [zoom, 0, 0, zoom, 0, 0];

function pad(n) {
  var s = String(n);
  while (s.length < 4) s = "0" + s;
  return s;
}

var out = { page_count: null, pages: [] };
try {
  out.page_count = doc.countPages();
} catch (e) {
  out.page_count_error = String(e.message || e);
}
for (var p = 0; p < (out.page_count || 0); p++) {
  var entry;
  try {
    var obj = doc.findPage(p);
    if (!obj.isDictionary()) throw new Error("page object is not a dictionary");
    if (!annots) obj.delete("Annots");
    var page = doc.loadPage(p);
    var b = page.getBounds();
    var r = [b[0] * zoom, b[1] * zoom, b[2] * zoom, b[3] * zoom];
    var bbox = [
      Math.floor(r[0] + 0.001), Math.floor(r[1] + 0.001),
      Math.ceil(r[2] - 0.001), Math.ceil(r[3] - 0.001),
    ];
    var w = bbox[2] - bbox[0], h = bbox[3] - bbox[1];
    if (w * h > 100000000) throw new Error("page too large");
    var pix = new Pixmap(ColorSpace.DeviceRGB, bbox, false);
    pix.clear(255);
    var dev = new DrawDevice(Matrix.identity, pix);
    if (annots) page.run(dev, ctm);
    else page.runPageContents(dev, ctm);
    dev.close();
    var name = "page-" + pad(p + 1) + ".ppm";
    pix.saveAsPNM(dir + "/" + name);
    entry = { page: p + 1, width: w, height: h, file: name };
  } catch (e) {
    entry = { page: p + 1, error: String(e.message || e) };
  }
  out.pages.push(entry);
}
print(JSON.stringify(out));
