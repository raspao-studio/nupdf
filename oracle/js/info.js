// Oracle for the `info` check: what MuPDF reports about a file, as JSON.
// Usage: mutool run oracle/js/info.js <file>
// Field names match `nupdf info` so the harness can compare them directly.

var doc = Document.openDocument(scriptArgs[0]);
var format = doc.getMetaData("format") || "";
var report = {
  format: format,
  pdf_version: format.indexOf("PDF ") === 0 ? format.slice(4) : null,
  needs_password: doc.needsPassword(),
  encryption: doc.getMetaData("encryption") || null,
};
if (!report.needs_password) {
  report.page_count = doc.countPages();
}
print(JSON.stringify(report));
