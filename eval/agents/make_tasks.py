"""Builds the inputs and ground truth for the harder tasks, written so that what is right is known by
construction, and adds them to DIR/fixtures and DIR/truth.json (made by setup.py first).

  python3 eval/agents/make_tasks.py --out DIR

Every file is written here with a small PDF writer, not by any of the tools under test. Each fixture is
checked with pdftotext where text should be found, so a task does not hinge on a file no reader can read.
"""
import argparse, json, os, random, re, subprocess, zlib

ap = argparse.ArgumentParser()
ap.add_argument("--out", required=True)
args = ap.parse_args()
out = os.path.abspath(args.out)
fx = os.path.join(out, "fixtures")
truth = json.load(open(os.path.join(out, "truth.json")))
norm = lambda s: re.sub(r"\s+", " ", s).strip()


def pdftotext(path, page=None, layout=False):
    cmd = ["pdftotext"] + (["-layout"] if layout else []) + (["-f", str(page), "-l", str(page)] if page else []) + [path, "-"]
    return subprocess.run(cmd, capture_output=True, timeout=60).stdout.decode("utf-8", "replace")


def esc(s):
    return s.replace("\\", "\\\\").replace("(", "\\(").replace(")", "\\)")


class Pdf:
    """Pages of text, lines and images; an outline; Info entries; text annotations."""

    def __init__(self):
        self.objs, self.pages, self.info, self.outline = [], [], {}, []

    def add(self, body):
        self.objs.append(body)
        return len(self.objs)

    def page(self, ops, annots=(), images=(), rotate=0, size=(612, 792)):
        self.pages.append((ops, list(annots), list(images), rotate, size))

    def save(self, path):
        self.objs = []
        cat, pages, font, bold = self.add(None), self.add(None), self.add(None), self.add(None)
        self.objs[font - 1] = b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
        self.objs[bold - 1] = b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold /Encoding /WinAnsiEncoding >>"
        kids = []
        for ops, annots, images, rotate, (w, h) in self.pages:
            xo = {}
            for i, (pw, ph, data) in enumerate(images):
                xo[f"Im{i}"] = self.add(b"<< /Type /XObject /Subtype /Image /Width %d /Height %d /ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /FlateDecode /Length %d >>\nstream\n" % (pw, ph, len(data)) + data + b"\nendstream")
            content = ("\n".join(ops)).encode("cp1252")
            c = self.add(b"<< /Length %d >>\nstream\n" % len(content) + content + b"\nendstream")
            a = [self.add(("<< /Type /Annot /Subtype /Text /Rect [%d %d %d %d] /Contents (%s) >>" % (x, y, x + 20, y + 20, esc(t))).encode("cp1252")) for x, y, t in annots]
            res = "/Font << /F1 %d 0 R /F2 %d 0 R >>" % (font, bold) + (" /XObject << %s >>" % " ".join(f"/{k} {v} 0 R" for k, v in xo.items()) if xo else "")
            kids.append(self.add(("<< /Type /Page /Parent %d 0 R /MediaBox [0 0 %d %d] /Rotate %d /Resources << %s >> /Contents %d 0 R%s >>" % (
                pages, w, h, rotate, res, c, (" /Annots [%s]" % " ".join(f"{x} 0 R" for x in a)) if a else "")).encode("cp1252")))
        self.objs[pages - 1] = ("<< /Type /Pages /Kids [%s] /Count %d >>" % (" ".join(f"{k} 0 R" for k in kids), len(kids))).encode()
        outlines = ""
        if self.outline:
            top = self.add(None)
            items = [self.add(None) for _ in self.outline]
            for i, (title, page, children) in enumerate(self.outline):
                kid_ids = [self.add(None) for _ in children]
                for j, (ct, cp) in enumerate(children):
                    links = (f" /Prev {kid_ids[j - 1]} 0 R" if j else "") + (f" /Next {kid_ids[j + 1]} 0 R" if j + 1 < len(kid_ids) else "")
                    self.objs[kid_ids[j] - 1] = f"<< /Title ({esc(ct)}) /Parent {items[i]} 0 R /Dest [{kids[cp]} 0 R /Fit]{links} >>".encode("cp1252")
                links = (f" /Prev {items[i - 1]} 0 R" if i else "") + (f" /Next {items[i + 1]} 0 R" if i + 1 < len(items) else "")
                sub = f" /First {kid_ids[0]} 0 R /Last {kid_ids[-1]} 0 R /Count {len(kid_ids)}" if kid_ids else ""
                self.objs[items[i] - 1] = f"<< /Title ({esc(title)}) /Parent {top} 0 R /Dest [{kids[page]} 0 R /Fit]{links}{sub} >>".encode("cp1252")
            self.objs[top - 1] = f"<< /Type /Outlines /First {items[0]} 0 R /Last {items[-1]} 0 R /Count {len(items)} >>".encode()
            outlines = f" /Outlines {top} 0 R /PageMode /UseOutlines"
        self.objs[cat - 1] = f"<< /Type /Catalog /Pages {pages} 0 R{outlines} >>".encode()
        info = self.add(("<< %s >>" % " ".join(f"/{k} ({esc(v)})" for k, v in self.info.items())).encode("cp1252")) if self.info else None
        pdf, offs = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n", []
        for i, o in enumerate(self.objs, 1):
            offs.append(len(pdf))
            pdf += b"%d 0 obj\n" % i + o + b"\nendobj\n"
        xref = len(pdf)
        n = len(self.objs) + 1
        pdf += b"xref\n0 %d\n0000000000 65535 f \n" % n + b"".join(b"%010d 00000 n \n" % o for o in offs)
        pdf += b"trailer\n<< /Size %d /Root 1 0 R%s >>\nstartxref\n%d\n%%%%EOF\n" % (n, (b" /Info %d 0 R" % info) if info else b"", xref)
        open(path, "wb").write(pdf)


def text(x, y, s, size=11, font="F1", mode=0):
    return f"BT /{font} {size} Tf {mode} Tr {x} {y} Td ({esc(s)}) Tj ET"


def noise(w, h, seed):
    r = random.Random(seed)
    return zlib.compress(bytes(r.choice((235, 240, 245, 250, 255, 30)) for _ in range(w * h)))


# ---- invoices: fields from six layouts --------------------------------------------------------------
os.makedirs(os.path.join(fx, "invoices"), exist_ok=True)
inv = {}


def invoice(name, vendor, number, date, total, build):
    p = Pdf()
    build(p, vendor, number, date, total)
    path = os.path.join(fx, "invoices", name)
    p.save(path)
    flat = norm(pdftotext(path))
    for v in (vendor, number):
        assert v in flat, (name, v)
    inv[name] = {"vendor": vendor, "invoice_number": number, "date": date, "total": total}


def labels(p, vendor, number, date, total):
    p.page([text(72, 720, vendor, 16, "F2"), text(72, 690, f"Invoice number: {number}"), text(72, 672, "Invoice date: 2026-03-04"),
            text(72, 654, f"Due date: {date}"), text(72, 600, "Consulting services, February"), text(400, 600, "1,200.00"),
            text(72, 560, f"Amount due: USD {total:,.2f}", 12, "F2")])
    inv_dates[number] = "2026-03-04"


def table(p, vendor, number, date, total):
    rows = [("Widget A", 3, 40.00), ("Widget B", 10, 12.50), ("Setup fee", 1, 75.00)]
    sub = sum(q * u for _, q, u in rows)
    ops = [text(72, 730, vendor, 16, "F2"), text(400, 730, "INVOICE", 16, "F2"), text(400, 710, f"No. {number}"), text(400, 694, f"Date {date}")]
    y = 640
    for h, x in (("Description", 72), ("Qty", 300), ("Unit", 360), ("Amount", 450)):
        ops.append(text(x, y, h, 10, "F2"))
    for d, q, u in rows:
        y -= 18
        ops += [text(72, y, d, 10), text(300, y, str(q), 10), text(360, y, f"{u:.2f}", 10), text(450, y, f"{q * u:.2f}", 10)]
    tax = round(total - sub, 2)
    ops += [text(360, y - 30, "Subtotal", 10), text(450, y - 30, f"{sub:.2f}", 10), text(360, y - 46, "Tax", 10), text(450, y - 46, f"{tax:.2f}", 10),
            text(360, y - 62, "Total", 10, "F2"), text(450, y - 62, f"{total:.2f}", 10, "F2")]
    p.page(ops)


def columns(p, vendor, number, date, total):
    # Bill-to on the left names another company; the vendor is at the bottom, as a remit-to line.
    p.page([text(72, 720, "BILL TO", 9, "F2"), text(72, 706, "Northwind Traders Ltd"), text(72, 692, "12 Harbour Road"),
            text(380, 720, "INVOICE #", 9, "F2"), text(460, 720, number), text(380, 704, "DATE", 9, "F2"), text(460, 704, date),
            text(72, 600, "Annual license renewal"), text(450, 600, f"{total:,.2f}"), text(380, 560, "TOTAL DUE", 10, "F2"), text(460, 560, f"${total:,.2f}", 10, "F2"),
            text(72, 120, f"Remit to: {vendor}, PO Box 77, Springfield", 9)])


def balance(p, vendor, number, date, total):
    p.page([text(72, 720, vendor, 16, "F2"), text(72, 700, f"Statement and invoice {number} issued {date}"), text(72, 660, "Previous balance"), text(420, 660, "980.00"),
            text(72, 642, "Payments received"), text(420, 642, "-980.00"), text(72, 624, "New charges"), text(420, 624, f"{total:,.2f}"),
            text(72, 590, "Please pay the new charges shown above; the previous balance is settled.", 9)])


def two_pages(p, vendor, number, date, total):
    ops = [text(72, 720, vendor, 16, "F2"), text(72, 700, f"Invoice {number}   Date: {date}")]
    for i in range(30):
        ops.append(text(72, 660 - i * 18, f"Line item {i + 1}: hosting, region {i % 4 + 1}", 10))
    p.page(ops)
    p.page([text(72, 720, f"Invoice {number} (continued)", 10), text(72, 680, "Grand total", 12, "F2"), text(420, 680, f"{total:,.2f}", 12, "F2"), text(72, 640, "Thank you for your business.")])


def scrambled(p, vendor, number, date, total):
    # The same layout as `labels`, but the content stream draws it bottom to top, right to left.
    ops = [text(72, 720, vendor, 16, "F2"), text(72, 690, "Invoice number:"), text(200, 690, number), text(72, 672, "Date:"), text(200, 672, date),
           text(72, 600, "Freight, 2 pallets"), text(400, 600, f"{total:,.2f}"), text(72, 560, "Total", 12, "F2"), text(400, 560, f"{total:,.2f}", 12, "F2")]
    p.page(list(reversed(ops)))


inv_dates = {}
invoice("inv1.pdf", "Acme Office Supply", "AOS-20417", "2026-04-03", 1200.00, labels)
invoice("inv2.pdf", "Bright Widgets GmbH", "BW-7781", "2026-02-11", 393.25, table)
invoice("inv3.pdf", "Cobalt Software Inc", "CS-0099-A", "2026-01-30", 4800.00, columns)
invoice("inv4.pdf", "Delta Utilities", "DU-55120", "2026-03-15", 212.40, balance)
invoice("inv5.pdf", "Echo Cloud Hosting", "ECH-3301", "2026-02-28", 9135.75, two_pages)
invoice("inv6.pdf", "Fjord Freight AS", "FF-2026-118", "2026-03-22", 640.00, scrambled)
# inv1's date is its invoice date, not the due date the builder passed.
inv["inv1.pdf"]["date"] = "2026-03-04"
truth["invoices"] = inv

# ---- pii: emails and phone numbers across a bundle, some outside the page text --------------------
os.makedirs(os.path.join(fx, "pii"), exist_ok=True)
pii = {}
people = [("Ana Ruiz", "ana.ruiz@example.com", "+1 415 555 0134"), ("Ben Okafor", "b.okafor@example.org", "(212) 555-0198"),
          ("Chen Wei", "chen.wei@mail.example.net", "+44 20 7946 0321"), ("Dana Kim", "dkim@example.com", "555-0177"),
          ("Eli Novak", "eli.novak@example.co.uk", "+49 30 901820 44")]
for i, (who, email, phone) in enumerate(people, 1):
    p = Pdf()
    keep = f"{who} joined the platform team in 2024 and leads the on-call rotation."
    ops = [text(72, 720, f"Employee record: {who}", 14, "F2"), text(72, 690, keep)]
    annots = []
    if i == 1:
        ops += [text(72, 660, f"Email: {email}"), text(72, 642, f"Phone: {phone}")]
    elif i == 2:
        ops += [text(72, 660, f"Reach {who.split()[0]} at {email} or on {phone} after 6pm.")]
    elif i == 3:
        ops += [text(72, 660, f"Contact: {email}")]
        p.info = {"Author": f"HR desk, {phone}", "Title": "Employee record"}
    elif i == 4:
        ops += [text(72, 660, "Contact details are in the note.")]
        annots = [(500, 700, f"Call {phone} or write to {email}")]
    else:
        # The address split over two lines, and the phone number in invisible text.
        user, host = email.split("@")
        ops += [text(72, 660, f"Email: {user}@"), text(72, 642, host), text(72, 600, f"Phone {phone}", 10, mode=3)]
    p.page(ops, annots=annots)
    name = f"record{i}.pdf"
    p.save(os.path.join(fx, "pii", name))
    pii[name] = {"remove": [email, phone], "keep": keep}
truth["pii"] = pii

# ---- bookmarks: split a report at its top-level bookmarks -----------------------------------------
p = Pdf()
chapters = [("Executive summary", 2, []), ("Market review", 3, [("Europe", 1), ("Asia", 2)]), ("Financials", 4, [("Revenue", 0), ("Costs", 2)]), ("Appendix", 3, [])]
start = 0
for title, n, subs in chapters:
    for k in range(n):
        p.page([text(72, 720, f"{title}", 14, "F2"), text(72, 690, f"Section page {k + 1} of {n}"), text(72, 100, f"[{title} / {k + 1}]", 8)])
    p.outline.append((title, start, [(st, start + off) for st, off in subs]))
    start += n
p.info = {"Title": "Annual report 2026"}
p.save(os.path.join(fx, "report.pdf"))
truth["bookmarks"] = {"parts": [{"title": t, "pages": [f"[{t} / {k + 1}]" for k in range(n)]} for t, n, _ in chapters]}

# ---- clause: a long contract whose amendment changes the answer -----------------------------------
r = random.Random(5)
p = Pdf()
words = "the parties agree that services shall be provided in accordance with schedules attached hereto and all fees are payable within thirty days of invoice unless otherwise stated".split()
clauses = {"14.1": "Either party may terminate this Agreement for material breach on 30 days' written notice if the breach is not cured.",
           "14.2": "The Customer may terminate this Agreement for convenience on 90 days' written notice to the Supplier.",
           "14.3": "Termination does not affect accrued rights or the survival of clauses 9, 11 and 16."}
section, para = 1, 0
for page in range(58):
    ops, y = [], 730
    while y > 90:
        if para % 7 == 0:
            ops.append(text(72, y, f"{section}. Section {section}", 11, "F2"))
            y -= 18
            sub = 1
        num = f"{section}.{sub}"
        line = clauses.get(num) if section == 14 else None
        line = line or (num + " " + " ".join(r.choice(words) for _ in range(14)).capitalize() + ".")
        if section == 14 and num in clauses:
            line = num + " " + clauses[num]
        ops.append(text(72, y, line[:110], 9))
        y -= 14
        para += 1
        sub += 1
        if para % 7 == 0:
            section += 1
    p.page(ops)
assert section > 15
p.page([text(72, 720, "Amendment No. 1", 14, "F2"), text(72, 690, "With effect from 1 June 2026, clause 14.2 is replaced as follows:", 10),
        text(72, 672, "14.2 The Customer may terminate this Agreement for convenience on 45 days' written notice.", 10),
        text(72, 640, "All other terms remain unchanged.", 10)])
p.page([text(72, 720, "Schedule 3 - Service levels", 14, "F2"), text(72, 690, "Response within 4 hours for priority 1 incidents.", 10)])
p.save(os.path.join(fx, "contract.pdf"))
assert "for convenience on 90 days" in norm(pdftotext(os.path.join(fx, "contract.pdf")))
truth["clause"] = {"section": "14.2", "days": 45}

# ---- scans: which pages are images with no text, and which are images with an OCR layer ----------
p = Pdf()
kinds = ["text", "text", "scan", "text", "ocr", "scan", "text", "text", "scan", "text"]
for i, k in enumerate(kinds, 1):
    if k == "text":
        p.page([text(72, 720, f"Page {i}: board minutes", 12, "F2"), text(72, 700, f"Item {i}. The board approved the budget for quarter {i % 4 + 1}.")])
    else:
        ops = ["q 612 0 0 792 0 0 cm /Im0 Do Q"]
        if k == "ocr":
            ops.append(text(72, 700, f"Item {i}. Scanned letter from the auditors, received in March.", 11, mode=3))
        p.page(ops, images=[(153, 198, noise(153, 198, i))])
p.save(os.path.join(fx, "scans.pdf"))
for i, k in enumerate(kinds, 1):
    has = bool(norm(pdftotext(os.path.join(fx, "scans.pdf"), i)))
    assert has == (k != "scan"), (i, k)
truth["scans"] = {"no_text": [i for i, k in enumerate(kinds, 1) if k == "scan"], "ocr": [i for i, k in enumerate(kinds, 1) if k == "ocr"]}

# ---- table: a ruled table to CSV, with a cell that wraps --------------------------------------------
cols = [("Region", 72), ("Product", 152), ("Q1", 312), ("Q2", 372), ("Q3", 432), ("Q4", 492)]
data = [("North", "Basic plan", 120, 135, 150, 160), ("North", "Pro plan", 80, 95, 101, 120), ("South", "Basic plan", 60, 64, 70, 71),
        ("South", "Pro plan with priority support", 40, 44, 52, 58), ("East", "Basic plan", 200, 210, 190, 230), ("East", "Pro plan", 150, 149, 160, 171),
        ("West", "Basic plan", 90, 91, 99, 105), ("West", "Pro plan", 70, 75, 78, 90)]
ops = [text(72, 740, "Subscriptions sold, 2026", 14, "F2")]
y = 700
ops.append("0.5 w")
for h, x in cols:
    ops.append(text(x + 4, y + 6, h, 10, "F2"))
rows_y = [y + 20, y]
for row in data:
    product = row[1]
    lines = [product] if len(product) < 22 else ["Pro plan with priority", "support"]
    hgt = 18 * len(lines)
    top = y
    for j, (v, (h, x)) in enumerate(zip(row, cols)):
        if j == 1:
            for li, l in enumerate(lines):
                ops.append(text(x + 4, top - 13 - li * 14, l, 10))
        else:
            ops.append(text(x + 4, top - 13, str(v), 10))
    y -= hgt
    rows_y.append(y)
for ry in rows_y:
    ops.append(f"72 {ry} m 552 {ry} l S")
for _, x in cols + [("", 552)]:
    ops.append(f"{x} {rows_y[0]} m {x} {rows_y[-1]} l S")
p = Pdf()
p.page(ops)
p.save(os.path.join(fx, "table.pdf"))
truth["table"] = {"header": [h for h, _ in cols], "rows": [[str(v) for v in r] for r in data]}

json.dump(truth, open(os.path.join(out, "truth.json"), "w"), indent=1, ensure_ascii=False)
print(json.dumps({k: truth[k] for k in ("clause", "scans")}))
