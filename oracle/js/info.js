// Oracle for the `info` check: what MuPDF reports about a file, as JSON.
// Usage: mutool run oracle/js/info.js <file>
// Field names match `nupdf info` so the harness can compare them directly.
// The order of calls matters: a lazy repair triggered by one call changes
// what later calls see, and nupdf makes the same calls in the same order.

var doc = Document.openDocument(scriptArgs[0]);
var format = doc.getMetaData("format") || "";
var report = {
  format: format,
  pdf_version: format.indexOf("PDF ") === 0 ? format.slice(4) : null,
  needs_password: doc.needsPassword(),
  encryption: doc.getMetaData("encryption") || null,
  page_count: null,
};
if (!report.needs_password) {
  try {
    report.page_count = doc.countPages();
  } catch (e) {
    report.page_count_error = String(e.message || e);
  }
}
if (doc.isPDF()) {
  report.object_count = doc.countObjects();
  report.repaired = doc.wasRepaired();
  report.revisions = doc.countVersions();
}
var keys = ["Title", "Author", "Subject", "Keywords", "Creator", "Producer", "CreationDate", "ModDate"];
report.metadata = {};
for (var i = 0; i < keys.length; i++) {
  var value = doc.getMetaData("info:" + keys[i]);
  if (value) report.metadata[keys[i]] = value;
}
print(JSON.stringify(report));
