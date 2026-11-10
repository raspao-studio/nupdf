// Oracle for the `clean` check: the document as `mutool clean` writes it
// with no options, in reproducible mode (no version in the comment line)
// and keeping /ID, as `mutool clean` does.
// Usage: mutool run oracle/js/clean.js <file> <scratch-dir>
// The output is written to <scratch-dir>/clean.pdf; the harness replaces
// the "file" field with the BLAKE3 hash of what was written, after naming
// nupdf in the comment line (both lines are the same length).

// A failed save is an error, so the harness sees MuPDF fail.

var doc = Document.openDocument(scriptArgs[0]);
var pdf = doc.asPDF ? doc.asPDF() : doc;
pdf.save(scriptArgs[1] + "/clean.pdf", "reproducible,regenerate-id=no");
print(JSON.stringify({ output: { file: "clean.pdf" } }));
