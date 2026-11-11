// Oracle for the `fonts`, `font-files`, `image-list` and `image-pixmaps`
// checks.
// Usage: mutool run oracle/js/inventory.js <file> <scratch-dir> <mode>
//
// fonts, images: what each page's own resources reach, through fonts,
// XObjects and patterns, found as `mutool info` walks them
// (gatherresourceinfo with everything shown): each object visited once
// per page, and each font or image dictionary listed once, at the first
// page it is found on (pdf_objcmp). Names are given as names, numbers as
// pdf_to_int gives them.
//
// font-files: the program each font descriptor embeds, written as
// `mutool extract` writes it (font-NNNN.ext), every object in order, after
// the walk.
//
// pixmaps: each image XObject's pixmap as fz_get_pixmap_from_image gives
// it at full size (Image.toPixmap()), every object in order after the
// walk: its size,
// components, alpha, colorspace and the MD5 of its samples.

var doc = Document.openDocument(scriptArgs[0]);
var dir = scriptArgs[1];
var mode = scriptArgs[2];

function message(e) {
  return String(e.message || e);
}

function nameOf(v) {
  if (v === null || v === undefined) return null;
  v = v.resolve();
  return v.isName() ? v.asName() : null;
}

// pdf_to_int.
function intOf(v) {
  if (v === null || v === undefined) return 0;
  v = v.resolve();
  if (!v.isNumber()) return 0;
  var n = v.asNumber();
  return n < 0 ? Math.ceil(n) : Math.floor(n);
}

function num(v) {
  return v.isIndirect() ? v.asIndirect() : 0;
}

// pdf_dict_get: the stored value; a reference stays a reference, even to
// an object that does not load.
function stored(dict, key) {
  var v = dict.get(key);
  if (v === null || v === undefined) return null;
  if (!v.isIndirect() && v.isNull()) return null;
  return v;
}

// A dictionary's values in order. The glue gives a null value as null,
// which MuPDF's walk passes over as it does any non-dictionary.
function values(dict) {
  var out = [];
  dict.forEach(function (v) { if (v !== null && v !== undefined) out.push(v); });
  return out;
}

var found = { fonts: [], images: [] };

function listed(list, obj) {
  for (var i = 0; i < list.length; i++)
    if (list[i].obj.compare(obj) === 0) return true;
  return false;
}

function walkPage(page, pageref) {
  var marks = [];
  var pageObject = num(pageref);
  // pdf_mark_list_push: true when already visited.
  function mark(obj) {
    var n = num(obj);
    if (n > 0 && marks.indexOf(n) >= 0) return true;
    marks.push(n);
    return false;
  }
  function fonts(dict) {
    values(dict).forEach(function (fontdict) {
      if (!fontdict.isDictionary()) return;
      var subtype = stored(fontdict, "Subtype");
      var basefont = stored(fontdict, "BaseFont");
      var name = basefont === null ? stored(fontdict, "Name") : basefont;
      var encoding = stored(fontdict, "Encoding");
      if (encoding !== null && encoding.isDictionary()) encoding = stored(encoding, "BaseEncoding");
      if (listed(found.fonts, fontdict)) return;
      found.fonts.push({
        obj: fontdict,
        entry: {
          page: page, page_object: pageObject, object: num(fontdict),
          subtype: nameOf(subtype), name: nameOf(name), encoding: nameOf(encoding),
        },
      });
    });
  }
  function images(dict) {
    values(dict).forEach(function (imagedict) {
      if (!imagedict.isDictionary()) return;
      var type = stored(imagedict, "Subtype");
      if (nameOf(type) !== "Image") return;
      var filter = stored(imagedict, "Filter");
      var filters = [];
      if (filter !== null) {
        if (filter.isArray()) filter.forEach(function (f) { filters.push(nameOf(f)); });
        else filters.push(nameOf(filter));
      }
      var cs = stored(imagedict, "ColorSpace");
      var alt = null;
      if (cs !== null && cs.isArray()) {
        var cses = cs.resolve();
        cs = cses.length > 0 ? cses.get(0) : null;
        var family = nameOf(cs);
        if (family === "DeviceN" || family === "Separation") {
          alt = cses.length > 2 ? cses.get(2) : null;
          if (alt !== null && alt.isArray()) {
            var a = alt.resolve();
            alt = a.length > 0 ? a.get(0) : null;
          }
        }
      }
      if (listed(found.images, imagedict)) return;
      var bpc = stored(imagedict, "BitsPerComponent");
      found.images.push({
        obj: imagedict,
        entry: {
          page: page, page_object: pageObject, object: num(imagedict),
          width: intOf(stored(imagedict, "Width")), height: intOf(stored(imagedict, "Height")),
          bpc: bpc === null ? null : intOf(bpc), filters: filters,
          colorspace: nameOf(cs), alternate: nameOf(alt),
        },
      });
    });
  }
  function resources(obj) {
    if (mark(obj)) return;
    var rsrc = obj.get("Resources");
    if (rsrc === null || rsrc === undefined || !rsrc.isDictionary()) return;
    var font = stored(rsrc, "Font");
    if (font !== null && !mark(font)) {
      fonts(font);
      values(font).forEach(resources);
    }
    var xobj = stored(rsrc, "XObject");
    if (xobj !== null && !mark(xobj)) {
      images(xobj);
      values(xobj).forEach(resources);
    }
    var shade = stored(rsrc, "Shading");
    if (shade !== null) mark(shade);
    var pattern = stored(rsrc, "Pattern");
    if (pattern !== null && !mark(pattern)) values(pattern).forEach(resources);
  }
  resources(pageref);
}

function walk() {
  var out = { page_count: null, page_errors: [] };
  try {
    out.page_count = doc.countPages();
  } catch (e) {
    return out;
  }
  for (var p = 0; p < out.page_count; p++) {
    try {
      walkPage(p + 1, doc.findPage(p));
    } catch (e) {
      out.page_errors.push({ page: p + 1, error: message(e) });
    }
  }
  return out;
}

function pad4(n) {
  var s = String(n);
  while (s.length < 4) s = "0" + s;
  return s;
}

// savefont.
function fontFile(desc) {
  var stream = null, ext = "";
  var f = stored(desc, "FontFile");
  if (f !== null) { stream = f; ext = "pfa"; }
  f = stored(desc, "FontFile2");
  if (f !== null) { stream = f; ext = "ttf"; }
  f = stored(desc, "FontFile3");
  if (f !== null) {
    stream = f;
    var sub = stored(f, "Subtype");
    if (sub !== null && !sub.resolve().isName()) throw new Error("invalid font descriptor subtype");
    var s = nameOf(sub);
    if (s === "Type1C") ext = "cff";
    else if (s === "CIDFontType0C") ext = "cid";
    else if (s === "OpenType") ext = "otf";
    else throw new Error("unhandled font type '" + (s === null ? "" : s) + "'");
  }
  if (stream === null) return null;
  return { stream: stream, ext: ext };
}

var out;
if (mode === "fonts") {
  out = walk();
  out.fonts = found.fonts.map(function (f) { return f.entry; });
} else if (mode === "images") {
  out = walk();
  out.images = found.images.map(function (i) { return i.entry; });
} else {
  // The command lists before it extracts or decodes, and finding pages can
  // repair the document.
  walk();
  var count = doc.countObjects();
  out = { object_count: count };
  var items = {};
  for (var i = 1; i < count; i++) {
    var ref = doc.newIndirect(i, 0);
    try {
      // pdf_dict_get on anything but a dictionary (or a stream) is null.
      if (!ref.isDictionary()) continue;
      if (mode === "font-files") {
        if (nameOf(stored(ref, "Type")) !== "FontDescriptor") continue;
        var ff = fontFile(ref);
        if (ff === null) continue;
        var name = "font-" + pad4(i) + "." + ff.ext;
        ff.stream.readStream().save(dir + "/" + name);
        items[i] = { name: name, file: name };
      } else {
        if (nameOf(stored(ref, "Subtype")) !== "Image") continue;
        var pix = doc.loadImage(ref).toPixmap();
        var cs = pix.getColorSpace();
        items[i] = {
          w: pix.getWidth(), h: pix.getHeight(), n: pix.getNumberOfComponents(),
          alpha: pix.getAlpha() ? true : false,
          colorspace: cs ? [cs.getType(), cs.getNumberOfComponents()] : null,
          md5: pix.computeMD5(),
        };
      }
    } catch (e) {
      items[i] = { error: message(e) };
    }
  }
  if (mode === "font-files") out.extracted = items;
  else out.pixmaps = items;
}
print(JSON.stringify(out));
