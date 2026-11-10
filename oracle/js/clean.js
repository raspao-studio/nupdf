// Oracle for the `clean` checks: the document as `mutool clean` writes it,
// in reproducible mode (no version in the comment line) and keeping /ID,
// as `mutool clean` does. Further write options, such as "garbage=2", may
// follow the scratch directory.
// Usage: mutool run oracle/js/clean.js <file> <scratch-dir> [options]
// The output is written to <scratch-dir>/clean.pdf; the harness replaces
// the "file" field with the BLAKE3 hash of what was written, after naming
// nupdf in the comment line (both lines are the same length).

// A failed save is an error, so the harness sees MuPDF fail.

var doc = Document.openDocument(scriptArgs[0]);
var pdf = doc.asPDF ? doc.asPDF() : doc;
var options = "reproducible,regenerate-id=no";
if (scriptArgs[2]) options += "," + scriptArgs[2];
pdf.save(scriptArgs[1] + "/clean.pdf", options);
print(JSON.stringify({ output: { file: "clean.pdf" } }));
