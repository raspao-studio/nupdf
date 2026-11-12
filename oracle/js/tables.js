// Oracle for the `tables` check: each page's tables as MuPDF's table
// hunting finds them (Page.toStructuredText with "vectors,table-hunt" and
// any options given), as rows of cells, each cell's text its lines joined
// by newlines. The script walker gives no structure boxes, so these are
// left out on both sides.
// Usage: mutool run oracle/js/tables.js <file> [options]

var doc = Document.openDocument(scriptArgs[0]);
var options = "vectors,table-hunt" + (scriptArgs.length > 1 && scriptArgs[1] ? "," + scriptArgs[1] : "");

function list(v) {
  var a = [];
  for (var i = 0; i < v.length; i++) a.push(v[i]);
  return a;
}

function walk(text) {
  // Structure blocks hold the blocks within them; `blocks` is the list
  // being filled, and `outer` the lists around it.
  var blocks = [];
  var outer = [];
  var block = null;
  var line = null;
  text.walk({
    beginStruct: function (standard, raw, index) {
      var s = { type: "struct", standard: standard, raw: raw, index: index, blocks: [] };
      blocks.push(s);
      outer.push(blocks);
      blocks = s.blocks;
    },
    endStruct: function () {
      blocks = outer.pop();
    },
    onImageBlock: function (bbox, transform) {
      blocks.push({ type: "image", bbox: list(bbox), transform: list(transform) });
    },
    onVector: function (bbox, flags) {
      var f = (flags.isStroked ? 1 : 0) | (flags.isRectangle ? 2 : 0) | (flags.continues ? 4 : 0);
      blocks.push({ type: "vector", bbox: list(bbox), flags: f });
    },
    beginTextBlock: function (bbox, flags) {
      block = { type: "text", bbox: list(bbox), flags: flags, lines: [] };
    },
    beginLine: function (bbox, wmode, dir) {
      line = { bbox: list(bbox), wmode: wmode ? 1 : 0, dir: list(dir), chars: [] };
    },
    onChar: function (c, origin, font, size, quad, rgb, flags, bidi) {
      line.chars.push([c, list(origin), font.getName(), size, list(quad), flags, bidi]);
    },
    endLine: function () {
      block.lines.push(line);
    },
    endTextBlock: function () {
      blocks.push(block);
    },
  });
  return blocks;
}


function lineText(l) {
  var s = "";
  for (var i = 0; i < l.chars.length; i++) s += l.chars[i][0];
  return s.replace(/^[ \t\n\r\v\f]+|[ \t\n\r\v\f]+$/g, "");
}

function cellText(blocks, out) {
  for (var i = 0; i < blocks.length; i++) {
    var b = blocks[i];
    if (b.type === "text") {
      for (var j = 0; j < b.lines.length; j++) {
        var t = lineText(b.lines[j]);
        if (t.length) out.push(t);
      }
    } else if (b.type === "struct") cellText(b.blocks, out);
  }
}

function kids(blocks, standard) {
  var out = [];
  for (var i = 0; i < blocks.length; i++)
    if (blocks[i].type === "struct" && blocks[i].standard === standard) out.push(blocks[i]);
  return out;
}

function findTables(blocks, out) {
  for (var i = 0; i < blocks.length; i++) {
    var s = blocks[i];
    if (s.type !== "struct") continue;
    if (s.standard !== "Table") {
      findTables(s.blocks, out);
      continue;
    }
    var rows = [], columns = 0;
    var trs = kids(s.blocks, "TR");
    for (var r = 0; r < trs.length; r++) {
      var tds = kids(trs[r].blocks, "TD"), cells = [];
      for (var c = 0; c < tds.length; c++) {
        var lines = [];
        cellText(tds[c].blocks, lines);
        cells.push({ text: lines.join("\n") });
      }
      columns = Math.max(columns, cells.length);
      rows.push({ cells: cells });
    }
    out.push({ columns: columns, rows: rows });
  }
}

var out = { page_count: null, pages: [] };
try {
  out.page_count = doc.countPages();
} catch (e) {
  out.page_count_error = String(e.message || e);
}
for (var p = 0; p < (out.page_count || 0); p++) {
  var entry = { page: p + 1 };
  try {
    var obj = doc.findPage(p);
    if (!obj.isDictionary()) throw new Error("page object is not a dictionary");
    obj.delete("Annots");
    var page = doc.loadPage(p);
    var found = [];
    findTables(walk(page.toStructuredText(options)), found);
    entry.tables = found;
  } catch (e) {
    entry.error = String(e.message || e);
  }
  out.pages.push(entry);
}
print(JSON.stringify(out));
