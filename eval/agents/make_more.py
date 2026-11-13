"""Builds the inputs and ground truth for the third set of tasks (forms, annotations, links, versions,
encryption, attachments, signatures, actions, page geometry, layout and a large bundle), and adds them to
DIR/fixtures and DIR/truth.json (made by setup.py and make_tasks.py first).

  python3 eval/agents/make_more.py --out DIR --mutool MUTOOL

As in make_tasks.py, every file is written here object by object, not by any of the tools under test, so
what is right is known by construction. qpdf encrypts one file and openssl signs three; each fixture is
checked with pdftotext, pdfinfo, qpdf or MuPDF, so no task hinges on a file the reference readers misread.
"""
import argparse, hashlib, json, os, random, re, subprocess, tempfile, textwrap, zlib

ap = argparse.ArgumentParser()
ap.add_argument("--out", required=True)
ap.add_argument("--mutool", required=True)
args = ap.parse_args()
out = os.path.abspath(args.out)
fx = os.path.join(out, "fixtures")
truth = json.load(open(os.path.join(out, "truth.json")))
norm = lambda s: re.sub(r"\s+", " ", s).strip()
run = lambda *a: subprocess.run(a, capture_output=True, timeout=120)


def pdftotext(path, page=None, raw=False):
    cmd = ["pdftotext"] + (["-raw"] if raw else []) + (["-f", str(page), "-l", str(page)] if page else []) + [path, "-"]
    return run(*cmd).stdout.decode("utf-8", "replace")


def esc(s):
    return s.replace("\\", "\\\\").replace("(", "\\(").replace(")", "\\)")


def text(x, y, s, size=11, font="F1"):
    return f"BT /{font} {size} Tf {x} {y} Td ({esc(s)}) Tj ET"


def enc(o):
    return o.encode("cp1252") if isinstance(o, str) else o


ID = b"<6e757064662d6167656e742d6576616c>"


class Doc:
    """Objects written one by one. Object 1 is Helvetica (/F1), 2 Helvetica-Bold (/F2)."""

    def __init__(self):
        self.objs = ["<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>",
                     "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold /Encoding /WinAnsiEncoding >>"]

    def new(self, body=None):
        self.objs.append(body)
        return len(self.objs)

    def set(self, n, body):
        self.objs[n - 1] = body

    def get(self, n):
        return self.objs[n - 1]

    @staticmethod
    def stream(data, extra=""):
        data = enc(data)
        return b"<< %s /Length %d >>\nstream\n" % (enc(extra), len(data)) + data + b"\nendstream"

    def form(self, ops, w, h):
        return self.new(self.stream(ops, f"/Type /XObject /Subtype /Form /BBox [0 0 {w} {h}] /Resources << /Font << /F1 1 0 R /Helv 1 0 R >> >>"))

    def pages(self, pages, catalog="", pages_extra=""):
        """pages: dicts with ops, size ((w, h); None inherits), extra (page dict entries) and annots
        (object numbers, or a function of the list of page numbers that returns one). Returns the
        catalog's number and the pages'."""
        cat, root = self.new(), self.new()
        refs = [self.new() for _ in pages]
        for ref, pg in zip(refs, pages):
            annots = [a(refs) if callable(a) else a for a in pg.get("annots", [])]
            c = self.new(self.stream("\n".join(pg["ops"])))
            size = pg.get("size", (612, 792))
            box = f" /MediaBox [0 0 {size[0]} {size[1]}]" if size else ""
            ann = f" /Annots [{' '.join(f'{a} 0 R' for a in annots)}]" if annots else ""
            self.set(ref, f"<< /Type /Page /Parent {root} 0 R{box} /Resources << /Font << /F1 1 0 R /F2 2 0 R >> >> /Contents {c} 0 R{ann}{pg.get('extra', '')} >>")
        self.set(root, f"<< /Type /Pages /Kids [{' '.join(f'{r} 0 R' for r in refs)}] /Count {len(refs)}{pages_extra} >>")
        self.set(cat, f"<< /Type /Catalog /Pages {root} 0 R{catalog} >>")
        self.root = cat
        return cat, refs

    def bytes(self, info=None):
        pdf, offs = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n", []
        for i, o in enumerate(self.objs, 1):
            offs.append(len(pdf))
            pdf += b"%d 0 obj\n" % i + enc(o) + b"\nendobj\n"
        xref, n = len(pdf), len(self.objs) + 1
        pdf += b"xref\n0 %d\n0000000000 65535 f \n" % n + b"".join(b"%010d 00000 n \n" % o for o in offs)
        return pdf + b"trailer\n<< /Size %d /Root %d 0 R%s /ID [%s %s] >>\nstartxref\n%d\n%%%%EOF\n" % (
            n, self.root, b" /Info %d 0 R" % info if info else b"", ID, ID, xref)

    def save(self, path, info=None):
        open(path, "wb").write(self.bytes(info))
        return path


def update(pdf, objs, root):
    """An incremental update: objs ({number: body}) appended with their own xref section and trailer."""
    prev = int(re.findall(rb"startxref\s+(\d+)", pdf)[-1])
    size = max(max(int(m) for m in re.findall(rb"/Size (\d+)", pdf)), max(objs) + 1)
    pdf += b"" if pdf.endswith(b"\n") else b"\n"
    offs = {}
    for n, body in sorted(objs.items()):
        offs[n] = len(pdf)
        pdf += b"%d 0 obj\n" % n + enc(body) + b"\nendobj\n"
    xref = len(pdf)
    pdf += b"xref\n" + b"".join(b"%d 1\n%010d 00000 n \n" % (n, o) for n, o in sorted(offs.items()))
    return pdf + b"trailer\n<< /Size %d /Root %d 0 R /Prev %d /ID [%s %s] >>\nstartxref\n%d\n%%%%EOF\n" % (size, root, prev, ID, ID, xref)


def write(path, data):
    open(path, "wb").write(data)
    return path


def lines(path, page):
    """Each text line on a page as MuPDF's stext device sees it: (text, (dx, dy))."""
    x = run(args.mutool, "draw", "-q", "-F", "stext", "-o", "-", path, str(page)).stdout.decode("utf-8", "replace")
    return [(t, tuple(float(v) for v in d.split())) for d, t in re.findall(r'<line [^>]*dir="([^"]+)"[^>]*text="([^"]*)"', x)]


def widget(extra, rect):
    return f"<< /Type /Annot /Subtype /Widget /F 4 /Rect [{rect}] /MK << /BC [0 0 0] >>{extra} >>"


TX = " /FT /Tx /DA (/Helv 10 Tf 0 g)"

# ---- fill: write values into a form's fields --------------------------------------------------------
d = Doc()
tick = d.form("0 G 1.2 w 3 3 m 11 11 l S 3 11 m 11 3 l S", 14, 14)
dot = d.form("0 g 4 4 6 6 re f", 14, 14)
blank = d.form("", 14, 14)
box = lambda on: f" /V /Off /AS /Off /AP << /N << /{on} {tick} 0 R /Off {blank} 0 R >> >>"
applicant = d.new()
name = d.new(widget(f" /Parent {applicant} 0 R{TX} /T (name) /TU (Full name)", "200 646 520 664"))
email = d.new(widget(f" /Parent {applicant} 0 R{TX} /T (email) /TU (Email)", "200 616 520 634"))
d.set(applicant, f"<< /T (applicant) /Kids [{name} 0 R {email} 0 R] >>")
start = d.new(widget(TX + " /T (start_date) /TU (Start date)", "200 586 320 604"))
plan = d.new(widget(" /FT /Ch /Ff 131072 /T (plan) /TU (Plan) /Opt [(Basic) (Pro) (Team)] /V (Basic) /DA (/Helv 10 Tf 0 g)", "200 556 320 574"))
contact = d.new()
radios = [d.new(widget(f" /Parent {contact} 0 R /AS /Off /AP << /N << /{v} {dot} 0 R /Off {blank} 0 R >> >>", f"{x} 526 {x + 14} 540")) for v, x in (("Email", 200), ("Phone", 280), ("Post", 360))]
d.set(contact, f"<< /FT /Btn /Ff 49152 /T (contact_by) /TU (Contact me by) /V /Off /Kids [{' '.join(f'{r} 0 R' for r in radios)}] >>")
news = d.new(widget(" /FT /Btn /T (newsletter) /TU (Send me the newsletter)" + box("Yes"), "200 496 214 510"))
terms = d.new(widget(" /FT /Btn /T (terms) /TU (I agree to the terms)" + box("Agreed"), "200 466 214 480"))
acro = d.new(f"<< /Fields [{applicant} 0 R {start} 0 R {plan} 0 R {contact} 0 R {news} 0 R {terms} 0 R] /DA (/Helv 10 Tf 0 g) /DR << /Font << /Helv 1 0 R >> >> >>")
ops = [text(72, 720, "Membership application", 16, "F2"), text(72, 650, "Full name"), text(72, 620, "Email"), text(72, 590, "Start date (YYYY-MM-DD)"),
       text(72, 560, "Plan"), text(72, 530, "Contact me by"), text(218, 530, "Email", 10), text(298, 530, "Phone", 10), text(378, 530, "Post", 10),
       text(72, 500, "Newsletter"), text(220, 500, "Send me the newsletter", 10), text(72, 470, "Terms"), text(220, 470, "I agree to the terms", 10)]
d.pages([{"ops": ops, "annots": [name, email, start, plan] + radios + [news, terms]}], catalog=f" /AcroForm {acro} 0 R")
d.save(os.path.join(fx, "form.pdf"))
truth["fill"] = {"fields": {"applicant.name": ["text", "Maria Lopez-Garcia"], "applicant.email": ["text", "maria.lopez@example.org"], "start_date": ["text", "2026-12-01"],
                            "plan": ["text", "Team"], "contact_by": ["button", "Phone"], "newsletter": ["button", "Yes"], "terms": ["button", "Agreed"]}}

# ---- formread: read the values of a filled form -----------------------------------------------------
d = Doc()
tick, dot, blank = d.form("0 G 1.2 w 3 3 m 11 11 l S 3 11 m 11 3 l S", 14, 14), d.form("0 g 4 4 6 6 re f", 14, 14), d.form("", 14, 14)
vals = {"holder": ("Policyholder", "Sofia Brandt"), "policy": ("Policy number", "HB-220-5813"), "address": ("New address", "14 Linden Close, Harrogate HG2 8QP"),
        "effective": ("Effective date", "2026-11-30"), "notes": ("Notes for the agent", "")}
fields, ops, y = [], [text(72, 730, "Change of address", 16, "F2")], 680
for key, (label, v) in vals.items():
    ap_ = d.form(f"/Tx BMC q BT /Helv 10 Tf 0 g 2 5 Td ({esc(v)}) Tj ET Q EMC" if v else "/Tx BMC EMC", 320, 18)
    fields.append(d.new(widget(f"{TX} /T ({key}) /TU ({label}) /V ({esc(v)}) /AP << /N {ap_} 0 R >>", f"200 {y - 4} 520 {y + 14}")))
    ops.append(text(72, y, label))
    y -= 32
billing = d.new(widget(f" /FT /Btn /T (billing) /TU (Also update the billing address) /V /On /AS /On /AP << /N << /On {tick} 0 R /Off {blank} 0 R >> >>", f"200 {y - 2} 214 {y + 12}"))
ops += [text(72, y, "Billing"), text(220, y, "Also update the billing address", 10)]
y -= 32
reason = d.new()
kids = [d.new(widget(f" /Parent {reason} 0 R /AS /{'Correction' if v == 'Correction' else 'Off'} /AP << /N << /{v} {dot} 0 R /Off {blank} 0 R >> >>", f"{x} {y - 2} {x + 14} {y + 12}"))
        for v, x in (("Moving", 200), ("Correction", 290), ("Other", 400))]
d.set(reason, f"<< /FT /Btn /Ff 49152 /T (reason) /TU (Reason for the change) /V /Correction /Kids [{' '.join(f'{k} 0 R' for k in kids)}] >>")
ops += [text(72, y, "Reason"), text(218, y, "Moving", 10), text(308, y, "Correction", 10), text(418, y, "Other", 10)]
acro = d.new(f"<< /Fields [{' '.join(f'{f} 0 R' for f in fields + [billing, reason])}] /DA (/Helv 10 Tf 0 g) /DR << /Font << /Helv 1 0 R >> >> >>")
d.pages([{"ops": ops, "annots": fields + [billing] + kids}], catalog=f" /AcroForm {acro} 0 R")
d.save(os.path.join(fx, "filled.pdf"))
truth["formread"] = {"policyholder": "Sofia Brandt", "policy_number": "HB-220-5813", "new_address": "14 Linden Close, Harrogate HG2 8QP",
                     "effective_date": "2026-11-30", "update_billing": True, "reason": "Correction", "notes": ""}

# ---- comments: what the annotations say and who wrote them ------------------------------------------
W = lambda s, size: len(s) * size * 0.5  # a rough Helvetica width, enough for a markup box


def markup(kind, x, y, s, size, who, says, color):
    x2, q = x + W(s, size), [x, y + size, x + W(s, size), y + size, x, y - 2, x + W(s, size), y - 2]
    return f"<< /Type /Annot /Subtype /{kind} /Rect [{x} {y - 2} {x2} {y + size}] /QuadPoints [{' '.join(map(str, q))}] /C [{color}] /T ({esc(who)}) /Contents ({esc(says)}) /M (D:20261102093000Z) >>"


d = Doc()
para = ["Lumen Labs today announced Beacon, a battery monitor for small solar installations.", "Beacon reports charge, temperature and faults to a phone app every minute.",
        "\"We cut our site visits in half,\" said Ines Duarte, who runs a solar co-operative in Evora.", "Beacon costs EUR 89 and ships to twelve countries."]
note = d.new()
popup = d.new(f"<< /Type /Annot /Subtype /Popup /Rect [420 600 580 700] /Parent {note} 0 R /Open false >>")
d.set(note, f"<< /Type /Annot /Subtype /Text /Rect [540 700 560 720] /Name /Comment /T (Sam Ortiz) /Contents (Lead with the customer quote, not the product name.) /Popup {popup} 0 R /M (D:20261102090000Z) >>")
reply = d.new(f"<< /Type /Annot /Subtype /Text /Rect [540 676 560 696] /IRT {note} 0 R /RT /R /T (Lee Park) /Contents (Agreed, I will move the quote up.) /M (D:20261102091500Z) >>")
hl_phrase = "We cut our site visits in half"
hl = d.new(markup("Highlight", 72, 660, hl_phrase, 11, "Sam Ortiz", "Is this figure confirmed by the co-operative?", "1 1 0"))
fta = d.form("BT /Helv 10 Tf 0 0 1 rg 2 6 Td (Add the launch date here) Tj ET", 180, 20)
ft = d.new(f"<< /Type /Annot /Subtype /FreeText /Rect [360 560 540 580] /DA (/Helv 10 Tf 0 0 1 rg) /T (Ravi Menon) /Contents (Add the launch date here) /AP << /N {fta} 0 R >> >>")
so_phrase = "the best monitor in its class"
so = d.new(markup("StrikeOut", 72, 640, so_phrase, 11, "Lee Park", "Remove this: legal says we cannot claim 'best in class'.", "1 0 0"))
link = d.new("<< /Type /Annot /Subtype /Link /Rect [72 96 300 110] /Border [0 0 0] /A << /S /URI /URI (https://example.org/press) >> >>")
p1 = [text(72, 730, "Press release: draft 3", 16, "F2")] + [text(72, 700 - 18 * i, s) for i, s in enumerate(para[:2])]
p2 = [text(72, 730, "Quote and pricing", 14, "F2"), text(72, 660, hl_phrase + ",\" said Ines Duarte, who runs a co-operative in Evora."), text(72, 600, para[3])]
p3 = [text(72, 730, "About Lumen Labs", 14, "F2"), text(72, 680, "Lumen Labs makes monitoring hardware for renewable energy."), text(72, 640, so_phrase + " is now shipping."),
      text(72, 100, "Press kit: example.org/press", 10)]
d.pages([{"ops": p1, "annots": [note, popup, reply]}, {"ops": p2, "annots": [hl, ft]}, {"ops": p3, "annots": [so, link]}])
d.save(os.path.join(fx, "review.pdf"))
truth["comments"] = {"comments": [{"page": 1, "author": "Sam Ortiz", "text": "Lead with the customer quote, not the product name."},
                                  {"page": 1, "author": "Lee Park", "text": "Agreed, I will move the quote up."},
                                  {"page": 2, "author": "Sam Ortiz", "text": "Is this figure confirmed by the co-operative?"},
                                  {"page": 2, "author": "Ravi Menon", "text": "Add the launch date here"},
                                  {"page": 3, "author": "Lee Park", "text": "Remove this: legal says we cannot claim 'best in class'."}]}

# ---- links: clickable links that leave the document, and where they really go -----------------------
d = Doc()
uri = lambda rect, u: d.new(f"<< /Type /Annot /Subtype /Link /Rect [{rect}] /Border [0 0 0] /A << /S /URI /URI ({esc(u)}) >> >>")
pages = [
    {"ops": [text(72, 730, "Partner handbook", 16, "F2"), text(72, 690, "Read the nupdf documentation for details."), text(72, 660, "Pricing: https://example.com/pricing"),
             text(72, 100, "Status page", 9)],
     "annots": [uri("100 686 230 702", "https://docs.example.org/guide/start"), uri("120 656 300 672", "https://example.net/offer?id=7&src=pdf"), uri("72 96 130 108", "https://status.example.com/")]},
    {"ops": [text(72, 730, "Contacts", 14, "F2"), text(72, 690, "See page 3 for the contact list."), text(72, 660, "Visit www.example.org/blog for news.")],
     "annots": [lambda refs: d.new(f"<< /Type /Annot /Subtype /Link /Rect [72 686 200 702] /Border [0 0 0] /Dest [{refs[2]} 0 R /Fit] >>")]},
    {"ops": [text(72, 730, "Contact list", 14, "F2"), text(72, 690, "Download the order form"), text(72, 100, "Status page", 9)],
     "annots": [uri("72 686 210 702", "https://forms.example.org/f/1234"), uri("72 96 130 108", "https://status.example.com/")]},
]
d.pages(pages)
d.save(os.path.join(fx, "handbook.pdf"))
truth["links"] = {"links": [[1, "https://docs.example.org/guide/start"], [1, "https://example.net/offer?id=7&src=pdf"], [1, "https://status.example.com/"],
                            [3, "https://forms.example.org/f/1234"], [3, "https://status.example.com/"]]}

# ---- diff: what changed between two versions --------------------------------------------------------
filler = ("Staff should read this section together with the code of conduct and the information security handbook, which take precedence where they differ. "
          "Exceptions to anything in this section need written approval from a director and are recorded by the People team.")
sections = [("1. Purpose", ["This policy sets out how employees of Lumen Labs may work away from the office. It applies to all permanent staff and to contractors engaged for more than three months.", filler]),
            ("2. Equipment and security", ["The company provides a laptop and a headset to each remote worker. Employees must use the company VPN on public networks. Lost or stolen equipment must be reported to the IT desk within 24 hours. Personal devices may not be used to store customer data.", filler]),
            ("3. Schedule", ["Employees may work remotely up to 3 days per week, subject to their manager's approval. Core hours are 10:00 to 15:00 in the employee's home time zone. Team meetings take priority over remote days.", filler]),
            ("4. Review", ["This policy is reviewed by the People team each year. Questions should be sent to the People team mailbox.", filler])]


def policy(path, secs):
    d = Doc()
    pages = []
    for title, paras in secs:
        ops, y = [text(72, 730, "Remote work policy", 9), text(72, 690, title, 14, "F2")], 660
        for p_ in paras:
            for ln in textwrap.wrap(p_, 92):
                ops.append(text(72, y, ln, 10))
                y -= 14
            y -= 10
        pages.append({"ops": ops})
    d.pages(pages)
    return d.save(path)


v2 = json.loads(json.dumps(sections))
v2[1][1][0] = v2[1][1][0].replace(" Employees must use the company VPN on public networks.", "")
v2[2][1][0] = v2[2][1][0].replace("up to 3 days per week", "up to 2 days per week")
v2[3][1][0] = v2[3][1][0].replace("each year.", "each year. Managers review remote arrangements every six months.")
a_, b_ = policy(os.path.join(fx, "policy-v1.pdf"), sections), policy(os.path.join(fx, "policy-v2.pdf"), v2)
assert "company VPN" in norm(pdftotext(a_)) and "company VPN" not in norm(pdftotext(b_)) and "every six months" in norm(pdftotext(b_))
truth["diff"] = {"changes": [{"kind": "removed", "before": ["company VPN on public networks"]},
                             {"kind": "changed", "before": ["3 days per week"], "after": ["2 days per week"]},
                             {"kind": "added", "after": ["every six months"]}]}

# ---- password: open an encrypted file with its password ----------------------------------------------
d = Doc()
d.pages([{"ops": [text(72, 730, "Treasury note", 16, "F2"), text(72, 700, "Confidential. The details of this month's supplier payment follow on page 2.")]},
         {"ops": [text(72, 730, "Supplier payment", 14, "F2"), text(72, 700, "Wire transfer reference WT-88213-K for EUR 14,250.00 was sent on 2026-10-02"),
                  text(72, 684, "to Vantaa Components Oy.")]},
         {"ops": [text(72, 730, "Earlier payments", 14, "F2"), text(72, 700, "The previous transfer, WT-77102-B for EUR 9,800.00, was cancelled.")]}])
with tempfile.TemporaryDirectory() as tmp:
    plain = d.save(os.path.join(tmp, "plain.pdf"))
    locked = os.path.join(fx, "locked.pdf")
    r = run("qpdf", "--encrypt", "--user-password=harbor-lights-42", "--owner-password=owner-7f3k-91", "--bits=256", "--", plain, locked)
    assert r.returncode == 0, r.stderr
assert run("qpdf", "--requires-password", locked).returncode == 0
assert "WT-88213-K" in run("pdftotext", "-upw", "harbor-lights-42", "-f", "2", "-l", "2", locked, "-").stdout.decode()
truth["password"] = {"reference": "WT-88213-K", "amount": 14250.00}

# ---- attachments: list the embedded files, wherever they are attached, and extract one ---------------
d = Doc()
budget = b"item,q1,q2\nTravel,1200,950\nSoftware,3400,3400\nTraining,800,1250\n"
files = {"budget.csv": (budget, "text/csv", "Budget, first half"), "minutes.txt": (b"Minutes of the 2026-10-14 meeting.\nPresent: A. Ruiz, B. Okafor.\nThe budget was approved.\n", "text/plain", "Minutes"),
         "site-photos.json": (b'{"photos": ["north-wall.jpg", "roof.jpg"], "taken": "2026-10-20"}\n', "application/json", "Photo index")}
spec = {}
for fname, (data, mime, desc) in files.items():
    ef = d.new(d.stream(zlib.compress(data), f"/Type /EmbeddedFile /Subtype /{mime.replace('/', '#2F')} /Filter /FlateDecode /Params << /Size {len(data)} /CheckSum <{hashlib.md5(data).hexdigest()}> /ModDate (D:20261020100000Z) >>"))
    spec[fname] = d.new(f"<< /Type /Filespec /F ({fname}) /UF ({fname}) /Desc ({desc}) /EF << /F {ef} 0 R >> >>")
att = d.new(f"<< /Type /Annot /Subtype /FileAttachment /Rect [500 690 520 710] /Name /PushPin /FS {spec['site-photos.json']} 0 R /Contents (Photo index for the site visit) /T (B. Okafor) >>")
tree = " ".join(f"({n}) {spec[n]} 0 R" for n in sorted(("budget.csv", "minutes.txt")))
d.pages([{"ops": [text(72, 730, "Project package", 16, "F2"), text(72, 700, "The budget and the meeting minutes are attached to this document.")]},
         {"ops": [text(72, 730, "Site visit", 14, "F2"), text(72, 700, "Photos were taken on 20 October; their index is attached here.")], "annots": [att]}],
        catalog=f" /Names << /EmbeddedFiles << /Names [{tree}] >> >>")
d.save(os.path.join(fx, "package.pdf"))
j = json.loads(run("qpdf", "--json=2", os.path.join(fx, "package.pdf")).stdout)
assert sorted(j["attachments"]) == ["budget.csv", "minutes.txt"], j["attachments"]
truth["attachments"] = {"files": sorted(files), "extract": "budget.csv", "data": budget.decode()}

# ---- signature: which signed files changed after their last signature -------------------------------
PH = b"/ByteRange [0 0000000000 0000000000 0000000000]"
HOLE = 16384


def sigdict(who, when):
    return f"<< /Type /Sig /Filter /Adobe.PPKLite /SubFilter /adbe.pkcs7.detached /Name ({who}) /Reason (Approved) /M (D:{when}Z) ".encode() + PH + b" /Contents <" + b"0" * HOLE + b"> >>"


def sign(pdf, key, crt, tmp):
    """Fill the last signature hole: the byte range around it, then a detached CMS signature of those bytes."""
    a = pdf.rindex(b"/Contents <" + b"0" * HOLE) + len(b"/Contents ")
    b = a + HOLE + 2
    j = pdf.rindex(PH)
    pdf = pdf[:j] + (b"/ByteRange [0 %d %d %d]" % (a, b, len(pdf) - b)).ljust(len(PH)) + pdf[j + len(PH):]
    data = os.path.join(tmp, "signed-bytes")
    write(data, pdf[:a] + pdf[b:])
    r = run("openssl", "cms", "-sign", "-binary", "-in", data, "-signer", crt, "-inkey", key, "-outform", "DER", "-md", "sha256", "-nosmimecap")
    assert r.returncode == 0, r.stderr
    h = r.stdout.hex().encode()
    assert len(h) <= HOLE
    return pdf[:a + 1] + h.ljust(HOLE, b"0") + pdf[b - 1:]


os.makedirs(os.path.join(fx, "signed"), exist_ok=True)
with tempfile.TemporaryDirectory() as tmp:
    keys = {}
    for who in ("Alex Morgan", "Blake Chen"):
        k, c = os.path.join(tmp, who[0] + ".key"), os.path.join(tmp, who[0] + ".crt")
        r = run("openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", k, "-out", c, "-days", "3650", "-sha256", "-subj", f"/CN={who}/O=Example Holdings")
        assert r.returncode == 0, r.stderr
        keys[who] = (k, c)

    def contract(title, fee):
        d = Doc()
        sig = d.new(sigdict("Alex Morgan", "20261101120000"))
        w = d.new()
        acro = d.new(f"<< /Fields [{w} 0 R] /SigFlags 3 >>")
        ops = [text(72, 730, title, 16, "F2"), text(72, 700, "Between Example Holdings and Northwind Traders Ltd."), text(72, 670, f"Total fee: USD {fee}"),
               text(72, 640, "Signed for Example Holdings by Alex Morgan.")]
        _, (page,) = d.pages([{"ops": ops, "annots": [w]}], catalog=f" /AcroForm {acro} 0 R")
        d.set(w, f"<< /Type /Annot /Subtype /Widget /FT /Sig /T (Signature1) /V {sig} 0 R /Rect [0 0 0 0] /F 132 /P {page} 0 R >>")
        return d, sign(d.bytes(), *keys["Alex Morgan"], tmp), page, acro, w

    # nda.pdf: signed once, untouched since.
    _, pdf, *_ = contract("Mutual non-disclosure agreement", "0")
    write(os.path.join(fx, "signed", "nda.pdf"), pdf)
    # supply.pdf: signed, then the fee in the page's text changed in an incremental update.
    d, pdf, page, acro, w = contract("Supply agreement", "12,000")
    content = int(re.search(r"/Contents (\d+) 0 R", d.get(page)).group(1))
    changed = d.get(content).replace(b"USD 12,000", b"USD 21,000")
    write(os.path.join(fx, "signed", "supply.pdf"), update(pdf, {content: changed}, d.root))
    # lease.pdf: signed by one party, then countersigned by the other; the countersignature covers it all.
    d, pdf, page, acro, w = contract("Office lease", "48,000")
    n = len(d.objs)
    sig2, w2 = n + 1, n + 2
    objs = {sig2: sigdict("Blake Chen", "20261103150000"),
            w2: f"<< /Type /Annot /Subtype /Widget /FT /Sig /T (Signature2) /V {sig2} 0 R /Rect [0 0 0 0] /F 132 /P {page} 0 R >>",
            acro: f"<< /Fields [{w} 0 R {w2} 0 R] /SigFlags 3 >>",
            page: d.get(page).replace(f"/Annots [{w} 0 R]", f"/Annots [{w} 0 R {w2} 0 R]")}
    write(os.path.join(fx, "signed", "lease.pdf"), sign(update(pdf, objs, d.root), *keys["Blake Chen"], tmp))
expect = {"nda.pdf": False, "supply.pdf": True, "lease.pdf": False}
for name, changed in expect.items():
    p = os.path.join(fx, "signed", name)
    raw = open(p, "rb").read()
    last = [int(x) for x in re.findall(rb"/ByteRange \[([\d ]+)\]", raw)[-1].split()]
    assert (last[2] + last[3] != len(raw)) == changed, name
    v = run(args.mutool, "sign", "-v", p).stdout.decode("utf-8", "replace")
    assert ("unchanged since signing" in v.split("Verifying signature")[-1]) == (not changed), v
    assert pdftotext(p).count("Total fee") == 1
truth["signature"] = {"changed": expect}

# ---- sanitize: remove JavaScript and launch actions, keep the document --------------------------------
d = Doc()
js = d.new(d.stream("var t = new Date(); app.alert('Opened at ' + t);"))
named = d.new(f"<< /S /JavaScript /JS {js} 0 R >>")
field = d.new(widget(TX + " /T (amount) /TU (Amount) /AA << /K << /S /JavaScript /JS (AFNumber_Keystroke\\(2, 0, 0, 0, \"\", true\\);) >> >>", "200 596 320 614"))
acro = d.new(f"<< /Fields [{field} 0 R] /DA (/Helv 10 Tf 0 g) /DR << /Font << /Helv 1 0 R >> >> >>")
launch = d.new("<< /Type /Annot /Subtype /Link /Rect [72 656 260 672] /Border [0 0 0] /A << /S /Launch /F (setup.exe) /Win << /F (cmd.exe) /P (/c setup.exe) >> >> >>")
web = d.new("<< /Type /Annot /Subtype /Link /Rect [72 686 260 702] /Border [0 0 0] /A << /S /URI /URI (https://example.org/help) >> >>")
keep = ["Expense claim form", "Claims over EUR 500 need a receipt for every item.", "Approval", "Claims are approved by the budget holder within ten working days."]
d.pages([{"ops": [text(72, 730, keep[0], 16, "F2"), text(72, 700, keep[1]), text(72, 690 - 30, "Open the setup assistant"), text(72, 600, "Amount")],
          "annots": [web, launch, field], "extra": " /AA << /O << /S /JavaScript /JS (this.print\\({bUI: false}\\);) >> >>"},
         {"ops": [text(72, 730, keep[2], 14, "F2"), text(72, 700, keep[3])]}],
        catalog=f" /AcroForm {acro} 0 R /OpenAction << /S /JavaScript /JS (app.alert\\('Welcome'\\);) >> /Names << /JavaScript << /Names [(init) {named} 0 R] >> >>")
d.save(os.path.join(fx, "script.pdf"))
truth["sanitize"] = {"actions": 5, "pages": 2, "keep": keep}

# ---- rotate: put pages in order and upright ----------------------------------------------------------
body = lambda n: [text(72, 730, f"Page {n} of 6", 14, "F2"), text(72, 700, f"Inspection log, section {n}: all readings within limits.")]
up_down = lambda ops: ["q -1 0 0 -1 612 792 cm"] + ops + ["Q"]
logical = {1: {"ops": body(1)}, 2: {"ops": body(2), "extra": " /Rotate 90"}, 3: {"ops": body(3)},
           5: {"ops": up_down(body(5))}, 6: {"ops": [text(72, 550, "Page 6 of 6", 14, "F2"), text(72, 520, "Inspection log, section 6: the site plan, drawn landscape.")], "size": (792, 612)}}
for m in ("0 -1 1 0 0 612", "0 1 -1 0 792 0"):
    # Page 4 is stored sideways and turned upright by /Rotate 270: correct as it is.
    logical[4] = {"ops": [f"q {m} cm"] + body(4) + ["Q"], "size": (792, 612), "extra": " /Rotate 270"}
    order = [3, 1, 6, 2, 5, 4]
    d = Doc()
    d.pages([logical[n] for n in order])
    path = d.save(os.path.join(fx, "pages.pdf"))
    if all(dx > 0.99 for _, (dx, _) in lines(path, order.index(4) + 1)):
        break
for i, n in enumerate(order, 1):
    upright = all(dx > 0.99 for _, (dx, _) in lines(path, i))
    assert upright == (n not in (2, 5)) and f"Page {n} of 6" in pdftotext(path, i), (i, n, lines(path, i))
truth["rotate"] = {"pages": 6}

# ---- columns: two columns drawn line by line across the page ------------------------------------------
steps = ["Check that the main breaker on the control panel is switched off before touching any equipment.",
         "Open the suction valve slowly until the pressure gauge reads between two and three bar.",
         "Vent trapped air through the bleed screw on top of the pump casing until water flows steadily.",
         "Confirm that the discharge valve is fully closed so that the motor starts without load.",
         "Switch the main breaker on and press the green start button once, holding it down for two seconds.",
         "Open the discharge valve a quarter turn at a time while watching the flow meter.",
         "Listen for grinding or rattling noises, and stop the pump at once if you hear any.",
         "Record the start time, pressure and flow rate in the station logbook before leaving."]
keys = [["main breaker on the control panel"], ["suction valve"], ["bleed screw"], ["discharge valve is fully closed"],
        ["switch the main breaker on", "holding it down for two seconds"], ["quarter turn"], ["grinding or rattling"], ["station logbook"]]
intro = "Follow these steps in order whenever the station has been shut down for maintenance. Do not skip a step, even when the pump was running a few minutes ago."
col, wrapped, step_at = [], [], []
for ln in textwrap.wrap(intro, 44):
    col.append(ln)
col.append("")
for i, s in enumerate(steps):
    step_at.append(len(col))
    col += textwrap.wrap(s, 44) + [""]
col += textwrap.wrap("Notes. Never leave the station unattended during its first hour of running. Report any fault to the duty engineer by phone.", 44)
first5 = step_at[4]
split = first5 + 1  # the left column ends after step 5's first line
left, right = col[:split], col[split:]
ops = [text(72, 740, "Pump station: start-up procedure", 15, "F2")]
for i in range(max(len(left), len(right))):
    for x, c in ((72, left), (320, right)):
        if i < len(c) and c[i]:
            ops.append(text(x, 710 - 14 * i, c[i], 10))
d = Doc()
d.pages([{"ops": ops}])
path = d.save(os.path.join(fx, "procedure.pdf"))
flat_ = norm(pdftotext(path)).lower()
pos = [flat_.find(k[0]) for k in keys]
assert all(p >= 0 for p in pos) and pos == sorted(pos), pos
assert all(k in flat_ for k in keys[4]) and norm(steps[4]).lower() in flat_
# In the order the page draws it, step 5 is broken up by the other column's lines.
assert norm(steps[4]).lower() not in norm(pdftotext(path, raw=True)).lower()
truth["columns"] = {"keys": keys}

# ---- figure: a caption and a table value on a given page ----------------------------------------------
capt = {2: "Figure 1. Energy use by site, 2023 to 2025, in megawatt hours.", 4: "Figure 2. Share of renewable electricity purchased, by quarter.",
        7: "Figure 3. Water withdrawn at the Ostrava plant fell after the closed-loop cooling retrofit in 2024.", 9: "Figure 4. Waste sent to landfill against the 2030 target."}
tables = {7: ("Table 2. Resource use at the Ostrava plant", [("Energy use (MWh)", "41,200", "39,850", "38,120"), ("Water use (ML)", "182", "151", "97"), ("Waste (t)", "640", "602", "575")]),
          8: ("Table 3. Resource use at the Brno plant", [("Energy use (MWh)", "22,310", "21,900", "21,480"), ("Water use (ML)", "95", "91", "88"), ("Waste (t)", "310", "298", "301")])}
prose = "The figures in this report cover the three plants owned for the whole period. Numbers are rounded; totals may not add up exactly."
pages = []
for n in range(1, 11):
    ops = [text(72, 760, f"Sustainability report 2026, page {n}", 8)]
    if n == 1:
        ops += [text(72, 720, "Sustainability report 2026", 18, "F2"), text(72, 680, "List of figures", 12, "F2"), text(72, 660, "Figure 1  Energy use by site ........ 2", 10),
                text(72, 646, "Figure 2  Renewable share ........ 4", 10), text(72, 632, "Figure 3  Water withdrawal ........ 7", 10), text(72, 618, "Figure 4  Landfill waste ........ 9", 10)]
    else:
        ops += [text(72, 720, f"Section {n}", 14, "F2")] + [text(72, 700 - 14 * i, ln, 10) for i, ln in enumerate(textwrap.wrap(prose, 95))]
        if n == 6:
            ops.append(text(72, 650, "Water use at Ostrava fell to 151 ML in 2024, as Figure 3 on page 7 shows.", 10))
        if n in capt:
            ops += ["0.85 g 72 420 468 200 re f 0 g", text(72, 404, capt[n], 9)]
        if n in tables:
            title, rows = tables[n]
            y = 360 if n in capt else 620
            ops.append(text(72, y, title, 10, "F2"))
            for x, h in ((300, "2023"), (380, "2024"), (460, "2025")):
                ops.append(text(x, y - 18, h, 10, "F2"))
            for i, row in enumerate(rows):
                for x, v in zip((72, 300, 380, 460), row):
                    ops.append(text(x, y - 36 - 16 * i, v, 10))
    pages.append({"ops": ops})
d = Doc()
d.pages(pages)
path = d.save(os.path.join(fx, "sustainability.pdf"))
assert norm(capt[7]) in norm(pdftotext(path, 7))
truth["figure"] = {"caption": capt[7], "water_2025": 97}

# ---- sizes: page sizes and orientation, after inheritance, rotation and cropping ----------------------
A4, A4L = (595.28, 841.89), (841.89, 595.28)
spec_ = [(None, ""), (A4, ""), ((792, 612), ""), (A4, " /Rotate 90"), (None, " /Rotate 180"), ((612, 1008), ""), ((612, 792), " /CropBox [36 200 576 560]"), (A4L, " /Rotate 270")]
d = Doc()
d.pages([{"ops": [text(72, 300, f"Sheet {i}", 24, "F2")], "size": s, "extra": e} for i, (s, e) in enumerate(spec_, 1)], pages_extra=" /MediaBox [0 0 612 792]")
path = d.save(os.path.join(fx, "sizes.pdf"))
info = run("pdfinfo", "-f", "1", "-l", "8", path).stdout.decode()
land, a4 = [], []
for i in range(1, 9):
    w, h = map(float, re.search(rf"Page\s+{i} size:\s+([\d.]+) x ([\d.]+)", info).groups())
    rot = int(re.search(rf"Page\s+{i} rot:\s+(\d+)", info).group(1))
    if (w > h) != (rot % 180 == 90):
        land.append(i)
    if sorted((round(w), round(h))) == [595, 842]:
        a4.append(i)
assert land == [3, 4, 7] and a4 == [2, 4, 8], (land, a4)
truth["sizes"] = {"landscape": land, "a4": a4}

# ---- bundle: one question over 150 files ---------------------------------------------------------------
r = random.Random(23)
os.makedirs(os.path.join(fx, "receipts"), exist_ok=True)
vendors = ["Harbor Coffee", "Pier Bakery", "Northline Taxi", "Copy Corner", "Green Grocer", "Metro Parking"]
months = {2: "Feb", 3: "Mar", 4: "Apr"}
hc, biggest = [], None
for i in range(1, 151):
    vendor = vendors[r.randrange(len(vendors))] if i != 97 else "Copy Corner"
    m, day = r.choice((2, 3, 3, 4)), r.randint(1, 28)
    date = f"{months[m]} {day}, 2026" if vendor == "Harbor Coffee" else f"2026-{m:02d}-{day:02d}"
    items = [(f"Item {k + 1}", round(r.uniform(2, 40), 2)) for k in range(r.randint(1, 4))]
    if i == 97:
        items = [("Poster printing, 400 copies", 1188.00)]
    sub = round(sum(v for _, v in items), 2)
    tax = round(sub * 0.08, 2)
    total = round(sub + tax, 2)
    ops = [text(72, 720, vendor, 16, "F2"), text(72, 700, f"Receipt {i:04d}   {date}", 10)]
    ops += [text(72, 670 - 16 * k, n_, 10) for k, (n_, _) in enumerate(items)] + [text(400, 670 - 16 * k, f"{v:.2f}", 10) for k, (_, v) in enumerate(items)]
    y = 670 - 16 * len(items) - 10
    ops += [text(300, y, "Subtotal", 10), text(400, y, f"{sub:.2f}", 10), text(300, y - 16, "Tax 8%", 10), text(400, y - 16, f"{tax:.2f}", 10),
            text(300, y - 34, "TOTAL", 11, "F2"), text(400, y - 34, f"${total:,.2f}", 11, "F2")]
    if vendor == "Pier Bakery" and i % 3 == 0:
        ops.append(text(72, 120, "Pairs well with a cup from Harbor Coffee, next door.", 9))
    d = Doc()
    d.pages([{"ops": ops}])
    name = f"r{i:03d}.pdf"
    d.save(os.path.join(fx, "receipts", name))
    if vendor == "Harbor Coffee" and m == 3:
        hc.append(total)
    if biggest is None or total > biggest[1]:
        biggest = (name, total)
assert biggest[0] == "r097.pdf" and "1,283.04" in pdftotext(os.path.join(fx, "receipts", "r097.pdf"))
truth["bundle"] = {"count": len(hc), "total": round(sum(hc), 2), "largest": biggest[0]}

# ---- claim: fields of a printed form, read by position ---------------------------------------------------
cells = [("Policyholder", "Jordan Ellis"), ("Policy number", "PX-4471-0932"), ("Branch", "Leeds North"),
         ("Date of birth", "1984-07-19"), ("Date of loss", "2026-09-03"), ("Claim type", "Water damage"),
         ("Claim amount", "8,450.00"), ("Deductible", "500.00"), ("Adjuster", "Priya Natarajan"),
         ("Vehicle reg.", "n/a"), ("Phone", "0113 496 0721"), ("Postcode", "LS7 3QH")]
ops, vals_ = [text(72, 740, "Property claim form", 16, "F2"), "0.6 w"], []
for i, (label, v) in enumerate(cells):
    x, top = 72 + 156 * (i % 3), 700 - 56 * (i // 3)
    ops += [f"{x} {top - 56} 156 56 re S", text(x + 4, top - 11, label.upper(), 7)]
    vals_.append(text(x + 9 + (i * 7) % 5, top - 38 - (i * 3) % 4, v, 11))
random.Random(31).shuffle(vals_)
d = Doc()
d.pages([{"ops": ops + vals_}])
path = d.save(os.path.join(fx, "claim.pdf"))
assert all(v in norm(pdftotext(path)) for _, v in cells)
truth["claim"] = {"policy_number": "PX-4471-0932", "date_of_loss": "2026-09-03", "claim_amount": 8450.00, "deductible": 500.00, "adjuster": "Priya Natarajan"}

json.dump(truth, open(os.path.join(out, "truth.json"), "w"), indent=1, ensure_ascii=False)
print(json.dumps({k: truth[k] for k in ("bundle", "sizes", "signature")}))
