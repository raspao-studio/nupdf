# Security

nupdf reads PDF files from anywhere, so a file that crashes it, hangs it, makes it use unbounded memory, or makes it say a file is clean when it is not, is a security bug. What nupdf promises and what it does not are in [docs/THREAT-MODEL.md](docs/THREAT-MODEL.md).

## Reporting a vulnerability

Report it privately through GitHub: the repository's **Security** tab, **Report a vulnerability**. Please do not open a public issue for it.

A useful report has the file that shows it (or how to make one), the command and options, nupdf's version (`nupdf --version`), and what happened. If the file is someone's real document, say so and do not attach it; a minimized file is better anyway (`cargo +nightly fuzz tmin` makes one).

You will hear back within a week. A fix goes into a release, with an advisory that credits you unless you would rather it did not.

## What counts

- A crash: a panic, an abort, a stack overflow, or any signal, on any input. nupdf has no `unsafe` code in its product crates, so memory corruption would be a bug in a dependency or the compiler; report it all the same.
- A hang, or memory or time far beyond what the file's size warrants with the default limits (`--max-decoded-bytes`, `--max-xref-entries`, `--timeout`).
- **`redact` saying content is gone when it is not**, or `verify` saying a file has none of a text when it does. Redaction is the command where a silent failure does the most harm (docs/AGENT-INTERFACE.md, the redaction contract).
- **`sanitize` leaving something it says it removed**: JavaScript, a launch or submit action, XFA.
- **`signatures` saying "OK"** for a signature whose signed bytes or signed digest do not hold.
- A file written by nupdf that another reader takes for something else than nupdf reported.
- Anything that reaches outside the input: reading or writing files other than those named, running programs (`ocr` runs Tesseract, and only the program `--tesseract` names), network access.

## What does not

- A file that MuPDF also reads differently from Acrobat. nupdf follows MuPDF by default (DECISIONS D5).
- Text, colours or pixels that differ from MuPDF's. That is a parity bug: open an ordinary issue.
- Certificate trust. `signatures` checks digests and RSA signatures, not whether a certificate is trusted.

## Supported versions

Only the latest release gets fixes until 1.0.
