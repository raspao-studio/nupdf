// Oracle for the signatures check: each page's signature widgets, with
// what MuPDF's signature functions say of each, in the JSON
// `nupdf signatures` gives.
// Usage: mutool run oracle/js/signatures.js <file>

var doc = Document.openDocument(scriptArgs[0]);
var n = doc.countPages();
var report = { page_count: n, pages: [] };

function message(e) {
  return String(e.message || e);
}

// pdf_dict_get_inheritable along /Parent.
function inherited(o, k) {
  for (var d = 0; o && !o.isNull() && d < 64; d++) {
    var v = o.get(k);
    if (v && !v.isNull()) return v;
    o = o.get("Parent");
  }
  return null;
}

function text(v) {
  if (!v || v.isNull()) return null;
  if (v.isName()) return v.asName();
  if (v.isString()) return v.asString();
  return null;
}

for (var i = 0; i < n; i++) {
  var entry = { page: i + 1 };
  try {
    var obj = doc.findPage(i);
    if (!obj.isDictionary()) throw new Error("page object is not a dictionary");
    var widgets = doc.loadPage(i).getWidgets();
    entry.signatures = [];
    for (var k = 0; k < widgets.length; k++) {
      var w = widgets[k];
      if (w.getFieldType() !== "signature") continue;
      var signed = !!w.isSigned();
      var s = { id: "p" + (i + 1) + ".w" + (k + 1), signed: signed };
      try {
        s.name = w.getName();
      } catch (e) {
        s.name_error = message(e);
      }
      if (signed) {
        var v = inherited(w.getObject(), "V");
        s.filter = text(v.get("Filter"));
        s.sub_filter = text(v.get("SubFilter"));
        s.reason = text(v.get("Reason"));
        s.location = text(v.get("Location"));
        s.signer_name = text(v.get("Name"));
        var c = v.get("Contents");
        s.contents_length = c && c.isString() ? c.asByteString().length : 0;
        s.signatory = String(w.getSignatory());
        var br = v.get("ByteRange");
        s.byte_range = [];
        if (br && br.isArray())
          for (var j = 0; j + 1 < br.length; j += 2) s.byte_range.push([br.get(j).asNumber(), br.get(j + 1).asNumber()]);
        try {
          s.changed_since_signing = !!w.incrementalChangesSinceSigning();
        } catch (e) {
          s.changed_error = message(e);
        }
        try {
          s.digest = w.checkDigest();
        } catch (e) {
          s.digest_error = message(e);
        }
      }
      entry.signatures.push(s);
    }
  } catch (e) {
    entry.error = message(e);
  }
  report.pages.push(entry);
}
print(JSON.stringify(report));
