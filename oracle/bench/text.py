# Every page's text with one library, as its own docs extract it, written
# to stdout: what `cargo xtask bench --pair text-context` times.
# Usage: text.py <pymupdf|pypdf|pdfplumber|pdfminer|pdf_oxide> <file>

import sys


def pymupdf(path):
    import pymupdf

    for page in pymupdf.open(path):
        yield page.get_text()


def pypdf(path):
    from pypdf import PdfReader

    for page in PdfReader(path).pages:
        yield page.extract_text()


def pdfplumber(path):
    import pdfplumber

    with pdfplumber.open(path) as pdf:
        for page in pdf.pages:
            yield page.extract_text() or ""


def pdfminer(path):
    from pdfminer.high_level import extract_text

    yield extract_text(path)


def pdf_oxide(path):
    from pdf_oxide import PdfDocument

    doc = PdfDocument(path)
    for i in range(doc.page_count()):
        yield doc.extract_text(i)


TOOLS = {f.__name__: f for f in (pymupdf, pypdf, pdfplumber, pdfminer, pdf_oxide)}

if __name__ == "__main__":
    out = sys.stdout
    for text in TOOLS[sys.argv[1]](sys.argv[2]):
        out.write(text)
        out.write("\f\n")
