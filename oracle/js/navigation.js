// Oracle for the navigation checks: a document's outline, each page's links
// and label, and its embedded files, as MuPDF reads them, in the JSON that
// `nupdf outline`, `links`, `labels` and `attachments` give.
// Usage: mutool run oracle/js/navigation.js <file> outline|links|labels|attachments

var doc = Document.openDocument(scriptArgs[0]);
var what = scriptArgs[1];
var report = {};

// fz_resolve_link: the page an internal link leads to, -1 for none.
function pageOf(uri) {
  if (uri === undefined || uri === null) return -1;
  try {
    return doc.resolveLink(uri);
  } catch (e) {
    return -1;
  }
}

function page1(n) {
  return n >= 0 ? n + 1 : null;
}

// load_outline_sub, through the outline iterator, which also gives `open`.
function outline(it) {
  var items = [];
  do {
    var item = it.item();
    if (!item) break;
    var node = {
      title: item.title === undefined ? null : item.title,
      uri: item.uri === undefined ? null : item.uri,
      open: !!item.open,
      page: page1(pageOf(item.uri)),
    };
    var res = it.down();
    if (res === 0) node.down = outline(it);
    if (res >= 0) it.up();
    items.push(node);
  } while (it.next() === 0);
  return items;
}

if (what === "outline") {
  var it = doc.outlineIterator();
  report.items = it ? outline(it) : [];
} else if (what === "links" || what === "labels") {
  var n = doc.countPages();
  report.page_count = n;
  report.pages = [];
  for (var i = 0; i < n; i++) {
    var entry = { page: i + 1 };
    try {
      // As the stext oracle loads pages: the page object first.
      var obj = doc.findPage(i);
      if (!obj.isDictionary()) throw new Error("page object is not a dictionary");
      var page = doc.loadPage(i);
      if (what === "labels") {
        entry.label = page.getLabel();
      } else {
        var links = page.getLinks();
        entry.links = [];
        for (var k = 0; k < links.length; k++) {
          var uri = links[k].getURI();
          entry.links.push({
            bbox: links[k].getBounds(),
            uri: uri === undefined ? null : uri,
            page: page1(pageOf(uri)),
          });
        }
      }
    } catch (e) {
      entry.error = String(e.message || e);
    }
    report.pages.push(entry);
  }
} else if (what === "attachments") {
  var pdf = doc.asPDF();
  // The script glue's getEmbeddedFiles throws on a document without an
  // /EmbeddedFiles tree; MuPDF's own functions see no files.
  var files = {};
  try {
    if (pdf) files = pdf.getEmbeddedFiles();
  } catch (e) {
    files = {};
  }
  var time = function (d) {
    return d === undefined ? null : Math.round(d.getTime() / 1000);
  };
  report.attachments = [];
  for (var name in files) {
    var fs = files[name];
    var p = pdf.getFilespecParams(fs);
    var entry = {
      name: name,
      filename: p.filename === undefined ? null : p.filename,
      embedded: pdf.isEmbeddedFile(fs),
      mimetype: p.mimetype === undefined ? null : p.mimetype,
      size: p.size === undefined ? null : p.size,
      created: time(p.creationDate),
      modified: time(p.modificationDate),
    };
    if (entry.embedded) {
      try {
        var data = pdf.getEmbeddedFileContents(fs);
        entry.length = data ? data.getLength() : null;
        entry.checksum_ok = pdf.verifyEmbeddedFileChecksum(fs);
      } catch (e) {
        entry.error = String(e.message || e);
      }
    }
    report.attachments.push(entry);
  }
}
print(JSON.stringify(report));
