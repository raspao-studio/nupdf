import subprocess, sys
S = sys.argv[1]
PLACE = 8192
def build(contents_hex, br):
    objs = []
    objs.append(b"<< /Type /Catalog /Pages 2 0 R /AcroForm << /Fields [4 0 R] /SigFlags 3 >> >>")
    objs.append(b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")
    objs.append(b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Annots [4 0 R] >>")
    objs.append(b"<< /Type /Annot /Subtype /Widget /FT /Sig /T (Approval) /Rect [0 0 0 0] /F 132 /P 3 0 R /V 5 0 R >>")
    objs.append(b"<< /Type /Sig /Filter /Adobe.PPKLite /SubFilter /adbe.pkcs7.detached /Name (Test Signer) /Reason (Testing) /Location (Here) /M (D:20261112120000Z) /ByteRange " + br + b" /Contents <" + contents_hex + b"> >>")
    out = b"%PDF-1.7\n"
    offs = []
    for i, o in enumerate(objs, 1):
        offs.append(len(out))
        out += b"%d 0 obj\n" % i + o + b"\nendobj\n"
    x = len(out)
    out += b"xref\n0 %d\n0000000000 65535 f \n" % (len(objs) + 1)
    for o in offs:
        out += b"%010d 00000 n \n" % o
    out += b"trailer\n<< /Size %d /Root 1 0 R >>\nstartxref\n%d\n%%%%EOF\n" % (len(objs) + 1, x)
    return out
br_placeholder = b"[0 0000000000 0000000000 0000000000]"
pdf = build(b"0" * (PLACE * 2), br_placeholder)
start = pdf.index(b"/Contents <") + len(b"/Contents ")
end = start + PLACE * 2 + 2
br = b"[0 %010d %010d %010d]" % (start, end, len(pdf) - end)
assert len(br) == len(br_placeholder)
pdf = build(b"0" * (PLACE * 2), br)
signed = pdf[:start] + pdf[end:]
open(S + "/data.bin", "wb").write(signed)
subprocess.run(["openssl", "cms", "-sign", "-binary", "-in", S + "/data.bin", "-signer", S + "/cert.pem", "-inkey", S + "/key.pem", "-outform", "DER", "-out", S + "/sig.der", "-md", "sha256", "-nosmimecap"], check=True)
sig = open(S + "/sig.der", "rb").read().hex().encode()
assert len(sig) <= PLACE * 2
sig = sig + b"0" * (PLACE * 2 - len(sig))
out = pdf[:start + 1] + sig + pdf[end - 1:]
assert len(out) == len(pdf)
open(sys.argv[2], "wb").write(out)
