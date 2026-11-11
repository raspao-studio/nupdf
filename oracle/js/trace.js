// Oracle for the `trace` check: each page's device calls, as MuPDF's run
// processor makes them for the page's contents (PDFPage.runPageContents
// with a script device and the identity matrix).
// Usage: mutool run oracle/js/trace.js <file>
//
// Each call is an array: its name, then its arguments. Paths are their
// walked segments, colorspaces their type and number of components,
// shadings their bounds under the call's matrix, images their shape.
// Each page loses its /Annots before it is loaded. The tile callbacks are
// left out: the glue's beginTile returns from inside its exception frame,
// which breaks the script engine after a few calls, so MuPDF draws each
// pattern cell's content instead.

var doc = Document.openDocument(scriptArgs[0]);

function cs(c) {
  if (c === null || c === undefined) return null;
  return [c.getType(), c.getNumberOfComponents()];
}

function color(c, v) {
  if (c === null || c === undefined) return null;
  var a = [];
  for (var i = 0; i < v.length; i++) a.push(v[i]);
  return a;
}

function params(p) {
  return [p.renderingIntent, p.blackPointCompensation, p.overPrinting, p.overPrintMode];
}

function path(p) {
  var out = [];
  p.walk({
    moveTo: function (x, y) { out.push(["m", x, y]); },
    lineTo: function (x, y) { out.push(["l", x, y]); },
    curveTo: function (a, b, c, d, e, f) { out.push(["c", a, b, c, d, e, f]); },
    closePath: function () { out.push(["h"]); },
  });
  return out;
}

function stroke(s) {
  return [s.getLineCap(), s.getLineJoin(), s.getLineWidth(), s.getMiterLimit(),
    s.getDashPhase(), s.getDashPattern()];
}

function list(v) {
  if (v === null || v === undefined) return null;
  var a = [];
  for (var i = 0; i < v.length; i++) a.push(v[i]);
  return a;
}

function image(img) {
  if (img === null || img === undefined) return null;
  return {
    w: img.getWidth(),
    h: img.getHeight(),
    bpc: img.getBitsPerComponent(),
    n: img.getNumberOfComponents(),
    imagemask: img.getImageMask(),
    cs: cs(img.getColorSpace()),
    interpolate: img.getInterpolate(),
    decode: list(img.getDecode()),
    colorkey: list(img.getColorKey()),
    mask: image(img.getMask()),
  };
}

function mat(m) {
  return [m[0], m[1], m[2], m[3], m[4], m[5]];
}

var events;

function rec(a) { events.push(a); }

var device = {
  fillPath: function (p, eo, ctm, c, v, alpha, cp) {
    rec(["fill_path", path(p), eo, mat(ctm), cs(c), color(c, v), alpha, params(cp)]);
  },
  strokePath: function (p, s, ctm, c, v, alpha, cp) {
    rec(["stroke_path", path(p), stroke(s), mat(ctm), cs(c), color(c, v), alpha, params(cp)]);
  },
  clipPath: function (p, eo, ctm) { rec(["clip_path", path(p), eo, mat(ctm)]); },
  clipStrokePath: function (p, s, ctm) { rec(["clip_stroke_path", path(p), stroke(s), mat(ctm)]); },
  fillText: function () { rec(["fill_text"]); },
  strokeText: function () { rec(["stroke_text"]); },
  clipText: function () { rec(["clip_text"]); },
  clipStrokeText: function () { rec(["clip_stroke_text"]); },
  ignoreText: function () { rec(["ignore_text"]); },
  fillShade: function (sh, ctm, alpha, cp) {
    var b = sh.getBounds(ctm);
    rec(["fill_shade", [b[0], b[1], b[2], b[3]], mat(ctm), alpha, params(cp)]);
  },
  fillImage: function (img, ctm, alpha, cp) {
    rec(["fill_image", image(img), mat(ctm), alpha, params(cp)]);
  },
  fillImageMask: function (img, ctm, c, v, alpha, cp) {
    rec(["fill_image_mask", image(img), mat(ctm), cs(c), color(c, v), alpha, params(cp)]);
  },
  clipImageMask: function (img, ctm) { rec(["clip_image_mask", image(img), mat(ctm)]); },
  popClip: function () { rec(["pop_clip"]); },
  beginMask: function (r, lum, c, v, cp) {
    rec(["begin_mask", [r[0], r[1], r[2], r[3]], lum, cs(c), color(c, v), params(cp)]);
  },
  endMask: function () { rec(["end_mask"]); },
  beginGroup: function (r, c, isolated, knockout, bm, alpha) {
    rec(["begin_group", [r[0], r[1], r[2], r[3]], cs(c), isolated, knockout, bm, alpha]);
  },
  endGroup: function () { rec(["end_group"]); },
  setDefaultColorSpaces: function (d) {
    rec(["set_default_colorspaces", cs(d.getDefaultGray()), cs(d.getDefaultRGB()),
      cs(d.getDefaultCMYK()), cs(d.getOutputIntent())]);
  },
  beginLayer: function (name) { rec(["begin_layer", name]); },
  endLayer: function () { rec(["end_layer"]); },
  beginStructure: function (standard, raw, idx) { rec(["begin_structure", standard, raw, idx]); },
  endStructure: function () { rec(["end_structure"]); },
  beginMetatext: function (meta, text) { rec(["begin_metatext", meta, text]); },
  endMetatext: function () { rec(["end_metatext"]); },
};

var out = { page_count: null, pages: [] };
try {
  out.page_count = doc.countPages();
} catch (e) {
  out.page_count_error = String(e.message || e);
}
for (var p = 0; p < (out.page_count || 0); p++) {
  events = [];
  var entry = { events: events };
  try {
    var obj = doc.findPage(p);
    if (!obj.isDictionary()) throw new Error("page object is not a dictionary");
    obj.delete("Annots");
    var page = doc.loadPage(p);
    try {
      page.runPageContents(device, [1, 0, 0, 1, 0, 0]);
    } catch (e) {
      entry.error = String(e.message || e);
    }
  } catch (e) {
    entry = { error: String(e.message || e) };
  }
  out.pages.push(entry);
}
print(JSON.stringify(out));
