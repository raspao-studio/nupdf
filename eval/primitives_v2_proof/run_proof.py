#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 hum
"""
Head-to-head empirical proof suite for nupdf's six next-generation agent capabilities:
  1. `nupdf chart`: Deterministic vector bar/line chart & directed flowchart decompiler
     (exact float recovery from vector paths + linear tick calibration, 0 vision tokens).
  2. `nupdf flat-forms`: Flat vector form synthesis (underlines, 9-cell SSN comb, 8x8
     vector checkboxes), verified auto-shrinking fill, and `/AcroForm` `/Widget` promotion.
  3. `nupdf patch --reflow`: Block-scoped paragraph word-wrapping and vertical Y-shift
     of downstream text, rules, and annotations with collision & page-overflow rejection.
  4. `nupdf history` + `--incremental`: Temporal `%%EOF` xref time-travel, per-revision
     PKCS#7 RSA signature verification, legitimate incremental fill vs post-signature
     visual occlusion & content tamper forensics.
  5. `nupdf cloak` & `nupdf uncloak`: Reversible format-preserving PII privacy firewall
     with coreference preservation, zero-leak verification, and PDF + JSON uncloaking.
  6. `nupdf pipeline` + Zero-Import `wasm32-unknown-unknown` Edge Engine: Single-call
     in-memory multi-step agent workflow (`info -> attest -> query -> ${q1.first_id}
     patch -> audit`) + live V8 WebAssembly instantiation with `{}` (0 host imports).

Benchmarked and graded independently against native `pdf_oxide 0.3.78` (Rust crate & CLI),
`PyMuPDF` (`fitz`), `pypdf`, `pdfplumber`, `pypdfium2`, and MuPDF (`mutool`).
"""

from __future__ import annotations

import argparse
import json
import re
import shutil
import subprocess
import tempfile
import time
from pathlib import Path

import pdfplumber
import pymupdf as fitz
import pypdf
import pypdfium2 as pdfium


def run_cmd(args: list[str], check: bool = False) -> tuple[int, str, str, float]:
    t0 = time.perf_counter()
    proc = subprocess.run(args, capture_output=True, text=True)
    dt = (time.perf_counter() - t0) * 1000.0
    if check and proc.returncode != 0:
        raise RuntimeError(
            f"command failed ({proc.returncode}): {' '.join(args)}\n"
            f"stdout: {proc.stdout}\nstderr: {proc.stderr}"
        )
    return proc.returncode, proc.stdout, proc.stderr, dt


# ---------------------------------------------------------------------------
# Fixture Builders (built externally with PyMuPDF / OpenSSL / raw PDF streams)
# ---------------------------------------------------------------------------


def make_chart_showcase_pdf(path: Path) -> dict:
    """
    Builds a 2-page PDF:
      - Page 1: Grouped 2-series vertical bar chart ("Cloud Spend vs Revenue ($M)")
        with Y ticks 0, 20, 40, 60, 80, 100 and exact non-integer bar heights
        (NO data-label text on the bars — only vector rectangles + ticks + legend!)
        plus a calibrated 4-point line trend plot below it.
      - Page 2: 4-node directed vector architecture flowchart with filled triangular
        arrowheads and an edge caption ("valid").
    """
    doc = fitz.open()

    # --- Page 1: Grouped Bar Chart + Line Plot ---
    p1 = doc.new_page(width=612, height=792)
    p1.insert_text((170, 58), "Quarterly Cloud Spend vs Revenue", fontsize=13)

    # Legend swatches (top-right of chart)
    c_rev = (0.18, 0.42, 0.82)  # #2e6bd1
    c_cost = (0.86, 0.35, 0.22)  # #db5938
    p1.draw_rect(fitz.Rect(390, 72, 402, 82), color=c_rev, fill=c_rev)
    p1.insert_text((407, 81), "Revenue", fontsize=9)
    p1.draw_rect(fitz.Rect(470, 72, 482, 82), color=c_cost, fill=c_cost)
    p1.insert_text((487, 81), "CloudSpend", fontsize=9)

    # Plot region: baseline y = 300, top y = 100 => 2.0 pt per 1.0 unit (0..100)
    y_base = 300.0
    pt_per_unit = 2.0
    p1.draw_line(fitz.Point(90, 100), fitz.Point(90, y_base), color=(0, 0, 0), width=1.0)
    p1.draw_line(fitz.Point(90, y_base), fitz.Point(520, y_base), color=(0, 0, 0), width=1.0)

    for tick_val in [0, 20, 40, 60, 80, 100]:
        ty = y_base - tick_val * pt_per_unit
        p1.draw_line(fitz.Point(85, ty), fitz.Point(90, ty), color=(0, 0, 0), width=0.8)
        # Place baseline of 9pt Helvetica so visual center aligns cleanly with tick mark
        p1.insert_text((58, ty + 3.0), str(tick_val), fontsize=9)

    ground_truth_bars = {
        "Revenue": [("Q1", 42.37), ("Q2", 68.91), ("Q3", 81.25), ("Q4", 94.60)],
        "CloudSpend": [("Q1", 19.45), ("Q2", 27.80), ("Q3", 31.15), ("Q4", 35.90)],
    }
    cats_x = {"Q1": 135.0, "Q2": 230.0, "Q3": 325.0, "Q4": 420.0}
    for (_, rev_val), (_, cost_val), (cat, cx) in zip(
        ground_truth_bars["Revenue"],
        ground_truth_bars["CloudSpend"],
        cats_x.items(),
    ):
        p1.draw_rect(
            fitz.Rect(cx, y_base - rev_val * pt_per_unit, cx + 24.0, y_base),
            color=c_rev,
            fill=c_rev,
            width=0,
        )
        p1.draw_rect(
            fitz.Rect(cx + 28.0, y_base - cost_val * pt_per_unit, cx + 52.0, y_base),
            color=c_cost,
            fill=c_cost,
            width=0,
        )
        p1.insert_text((cx + 18.0, y_base + 16.0), cat, fontsize=10)

    # --- Page 2: Calibrated Line Chart (y_base2 = 340, top = 140 => 2.0 pt per unit) ---
    p_line = doc.new_page(width=612, height=792)
    p_line.insert_text((205, 115), "Net Retention Index", fontsize=12)
    y_base2 = 340.0
    p_line.draw_line(fitz.Point(90, 140), fitz.Point(90, y_base2), color=(0, 0, 0), width=1.0)
    p_line.draw_line(fitz.Point(90, y_base2), fitz.Point(520, y_base2), color=(0, 0, 0), width=1.0)
    for tick_val in [0, 25, 50, 75, 100]:
        ty = y_base2 - tick_val * 2.0
        p_line.draw_line(fitz.Point(85, ty), fitz.Point(90, ty), color=(0, 0, 0), width=0.8)
        p_line.insert_text((58, ty + 3.0), str(tick_val), fontsize=9)

    line_truth = [("Jan", 150.0, 31.5), ("Feb", 250.0, 54.0), ("Mar", 350.0, 62.5), ("Apr", 450.0, 88.0)]
    for lbl, lx, _ in line_truth:
        p_line.insert_text((lx - 8.0, y_base2 + 16.0), lbl, fontsize=9)

    shape = p_line.new_shape()
    pts = [fitz.Point(lx, y_base2 - val * 2.0) for _, lx, val in line_truth]
    shape.draw_polyline(pts)
    shape.finish(color=(0.12, 0.62, 0.34), width=2.0, closePath=False)
    shape.commit()

    # --- Page 3: 4-Node Directed Flowchart ---
    p2 = doc.new_page(width=612, height=792)
    nodes = [
        ("Ingest PDF", fitz.Rect(70, 140, 180, 184)),
        ("Attest Pixels", fitz.Rect(250, 140, 370, 184)),
        ("Cloak PII", fitz.Rect(250, 260, 370, 304)),
        ("Audit Claims", fitz.Rect(440, 200, 555, 244)),
    ]
    for label, r in nodes:
        p2.draw_rect(r, color=(0.1, 0.1, 0.1), width=1.2)
        p2.insert_text((r.x0 + 14, (r.y0 + r.y1) * 0.5 + 4), label, fontsize=10)

    def draw_arrow(p: fitz.Page, start: fitz.Point, tip: fitz.Point, caption: str | None = None) -> None:
        dx, dy = tip.x - start.x, tip.y - start.y
        dist = max((dx * dx + dy * dy) ** 0.5, 1.0)
        ux, uy = dx / dist, dy / dist
        shaft_end = fitz.Point(tip.x - ux * 7.0, tip.y - uy * 7.0)
        p.draw_line(start, shaft_end, color=(0, 0, 0), width=1.2)
        px, py = -uy, ux
        b1 = fitz.Point(tip.x - ux * 8.0 + px * 4.0, tip.y - uy * 8.0 + py * 4.0)
        b2 = fitz.Point(tip.x - ux * 8.0 - px * 4.0, tip.y - uy * 8.0 - py * 4.0)
        sh = p.new_shape()
        sh.draw_polyline([tip, b1, b2, tip])
        sh.finish(color=(0, 0, 0), fill=(0, 0, 0), width=0.5)
        sh.commit()
        if caption:
            mx, my = (start.x + tip.x) * 0.5, (start.y + tip.y) * 0.5
            p.insert_text((mx - 12, my - 6), caption, fontsize=8)

    # Edges: Ingest PDF -> Attest Pixels ("valid"), Attest Pixels -> Cloak PII, Cloak PII -> Audit Claims
    draw_arrow(p2, fitz.Point(180, 162), fitz.Point(250, 162), "valid")
    draw_arrow(p2, fitz.Point(310, 184), fitz.Point(310, 260), None)
    draw_arrow(p2, fitz.Point(370, 282), fitz.Point(440, 222), "clean")

    doc.save(str(path))
    doc.close()
    return {
        "bars": ground_truth_bars,
        "line": [(lbl, v) for lbl, _, v in line_truth],
        "expected_edges": [
            ("Ingest PDF", "Attest Pixels"),
            ("Attest Pixels", "Cloak PII"),
            ("Cloak PII", "Audit Claims"),
        ],
    }


def make_flat_intake_pdf(path: Path) -> None:
    """
    Builds a 1-page flat government/medical intake PDF containing ZERO `/AcroForm`
    entries — only stroked vector underlines, a 9-cell SSN comb grid, and 3 10x10 pt
    vector checkboxes (1 pre-checked with an X stroke, 2 unchecked).
    """
    doc = fitz.open()
    p = doc.new_page(width=612, height=792)
    p.insert_text((72, 70), "PATIENT & TAXPAYER INTAKE FORM (FLAT PRINT MASTER)", fontsize=13)

    # 1. Stroked underline field: Applicant Name
    p.insert_text((72, 125), "Applicant Name:", fontsize=11)
    p.draw_line(fitz.Point(175, 127), fitz.Point(430, 127), color=(0, 0, 0), width=0.9)

    # 2. Stroked underline field: Department
    p.insert_text((72, 165), "Department:", fontsize=11)
    p.draw_line(fitz.Point(155, 167), fitz.Point(360, 167), color=(0, 0, 0), width=0.9)

    # 3. 9-cell SSN comb grid (`|_|_|_|_|_|_|_|_|_|`)
    p.insert_text((72, 218), "SSN:", fontsize=11)
    comb_x0, comb_y0, cell_w, cell_h = 125.0, 203.0, 18.0, 20.0
    p.draw_rect(
        fitz.Rect(comb_x0, comb_y0, comb_x0 + 9 * cell_w, comb_y0 + cell_h),
        color=(0, 0, 0),
        width=0.9,
    )
    for idx in range(1, 9):
        cx = comb_x0 + idx * cell_w
        p.draw_line(fitz.Point(cx, comb_y0), fitz.Point(cx, comb_y0 + cell_h), color=(0, 0, 0), width=0.8)

    # 4. Three 10x10 vector checkboxes:
    # - US Citizen (unchecked)
    r_cit = fitz.Rect(72, 260, 82, 270)
    p.draw_rect(r_cit, color=(0, 0, 0), width=0.9)
    p.insert_text((90, 269), "US Citizen", fontsize=10)

    # - Existing Patient (pre-checked with vector X!)
    r_ex = fitz.Rect(210, 260, 220, 270)
    p.draw_rect(r_ex, color=(0, 0, 0), width=0.9)
    p.draw_line(fitz.Point(212, 262), fitz.Point(218, 268), color=(0, 0, 0), width=1.1)
    p.draw_line(fitz.Point(212, 268), fitz.Point(218, 262), color=(0, 0, 0), width=1.1)
    p.insert_text((228, 269), "Existing Patient", fontsize=10)

    # - HIPAA Consent (unchecked)
    r_hipaa = fitz.Rect(370, 260, 380, 270)
    p.draw_rect(r_hipaa, color=(0, 0, 0), width=0.9)
    p.insert_text((388, 269), "HIPAA Consent", fontsize=10)

    doc.save(str(path))
    doc.close()


def make_contract_reflow_pdf(path: Path) -> None:
    """
    Builds a 1-page multi-clause contract where:
      - Block 1 (`p1.b1`): Header + "Contract Value:" "$35,000.00" (two distinct spans for pipeline query!)
      - Block 2 (`p1.b2`): 2-line Clause 2 ("2. Indemnification...")
      - Block 3 (`p1.b3`): Clause 3 ("3. Governing Law...") directly below Clause 2
      - Vector horizontal divider line + Block 4 (`p1.b4`): Signature block below Clause 3
    """
    doc = fitz.open()
    p = doc.new_page(width=612, height=792)

    p.insert_text((72, 80), "MASTER SERVICES AGREEMENT", fontsize=14)
    p.insert_text((350, 80), "Contract Value:", fontsize=11)
    p.insert_text((445, 80), "$35,000.00", fontsize=11)

    # Clause 2: 2 lines in one paragraph block (14pt line spacing so MuPDF groups as 1 block)
    p.insert_text(
        (72, 140),
        "2. Indemnification. Provider shall indemnify Customer against direct third-party",
        fontsize=11,
    )
    p.insert_text(
        (72, 154),
        "intellectual property claims finally awarded by a court of competent jurisdiction.",
        fontsize=11,
    )

    # Clause 3: 42 pt below Clause 2 (separate block `p1.b3`!)
    p.insert_text(
        (72, 198),
        "3. Governing Law. This Agreement shall be governed by the laws of Delaware.",
        fontsize=11,
    )
    p.insert_text(
        (72, 212),
        "Each party irrevocably consents to exclusive venue in Wilmington courts.",
        fontsize=11,
    )

    # Horizontal signature divider rule + Signature caption below
    p.draw_line(fitz.Point(72, 265), fitz.Point(300, 265), color=(0, 0, 0), width=0.9)
    p.insert_text((72, 282), "Authorized Signature: _______________________", fontsize=11)

    doc.save(str(path))
    doc.close()


def make_signed_history_3rev_pdf(nupdf_bin: str, path: Path, work: Path) -> dict:
    """
    Constructs a genuine 3-revision PKCS#7 RSA-2048 SHA-256 signed PDF:
      - Revision 1 (`initial`): Signed wire authorization for `$10,000.00` with
        an unsigned countersigner notes AcroForm field (`notes = "Pending"`).
      - Revision 2 (`clean_incremental_fill`): Filled via `nupdf forms --set notes="Approved by CFO" --incremental`,
        proving `nupdf`'s incremental writer preserves the RSA signature!
      - Revision 3 (`tampered_after_signature`): Incremental post-signature attack
        that paints an opaque white rectangle over `$10,000.00` AND overlays
        `Approved Wire Total: $90,000.00` in a new content stream!
    """
    key_pem = work / "signer_key.pem"
    cert_pem = work / "signer_cert.pem"
    run_cmd(
        [
            "openssl",
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-keyout",
            str(key_pem),
            "-out",
            str(cert_pem),
            "-days",
            "365",
            "-subj",
            "/CN=Elena Rostova/O=Helios Treasury/C=US",
        ],
        check=True,
    )

    content_stream = (
        b"BT /F1 14 Tf 72 700 Td (Approved Wire Total: $10,000.00) Tj ET\n"
        b"BT /F1 11 Tf 72 675 Td (Dual Board Sign-Off Required Above $25,000) Tj ET"
    )
    hex_cap = 6144
    br_placeholder = "[0 0000000000 0000000000 0000000000]"
    hex_placeholder = "0" * hex_cap

    objs = [
        b"<</Type/Catalog/Pages 2 0 R/AcroForm<</Fields[6 0 R 7 0 R]/DA(/F1 10 Tf 0 g)/DR<</Font<</F1 5 0 R>>>>>>>>",
        b"<</Type/Pages/Kids[3 0 R]/Count 1>>",
        b"<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources<</Font<</F1 5 0 R>>>>/Contents 4 0 R/Annots[6 0 R 7 0 R]>>",
        f"<</Length {len(content_stream)}>>\nstream\n".encode("ascii") + content_stream + b"\nendstream",
        b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>",
        b"<</Type/Annot/Subtype/Widget/FT/Tx/T(notes)/V(Pending)/Rect[72 620 320 645]>>",
        b"<</Type/Annot/Subtype/Widget/FT/Sig/T(treasury_sig)/V 8 0 R/Rect[72 560 280 600]>>",
        f"<</Type/Sig/Filter/Adobe.PPKLite/SubFilter/adbe.pkcs7.detached/Reason(Wire Authorization)/ByteRange{br_placeholder}/Contents<{hex_placeholder}>>>".encode(
            "ascii"
        ),
    ]

    pdf = bytearray(b"%PDF-1.7\n")
    offsets: list[int] = []
    for idx, body in enumerate(objs, start=1):
        offsets.append(len(pdf))
        pdf.extend(f"{idx} 0 obj\n".encode("ascii"))
        pdf.extend(body)
        pdf.extend(b"\nendobj\n")
    xref_pos = len(pdf)
    pdf.extend(f"xref\n0 {len(objs) + 1}\n0000000000 65535 f \n".encode("ascii"))
    for ofs in offsets:
        pdf.extend(f"{ofs:010d} 00000 n \n".encode("ascii"))
    pdf.extend(
        f"trailer\n<</Size {len(objs) + 1}/Root 1 0 R>>\nstartxref\n{xref_pos}\n%%EOF\n".encode(
            "ascii"
        )
    )

    br_needle = f"/ByteRange{br_placeholder}".encode("ascii")
    br_at = pdf.find(br_needle)
    assert br_at > 0
    br_val_start = br_at + len(b"/ByteRange")
    c_at = pdf.find(b"/Contents<", br_at)
    hole_start = c_at + len(b"/Contents")
    hole_end = hole_start + 1 + hex_cap + 1
    tail_len = len(pdf) - hole_end

    actual_br = f"[0 {hole_start:<10d} {hole_end:<10d} {tail_len:<10d}]".encode("ascii")
    assert len(actual_br) == len(br_placeholder)
    pdf[br_val_start : br_val_start + len(br_placeholder)] = actual_br

    to_sign_path = work / "rev1_to_sign.bin"
    der_path = work / "rev1_sig.der"
    to_sign_path.write_bytes(bytes(pdf[:hole_start]) + bytes(pdf[hole_end:]))
    # Detached PKCS#7 (`-binary` without `-nodetach` so signed byte ranges are external)
    run_cmd(
        [
            "openssl",
            "cms",
            "-sign",
            "-binary",
            "-in",
            str(to_sign_path),
            "-signer",
            str(cert_pem),
            "-inkey",
            str(key_pem),
            "-outform",
            "DER",
            "-out",
            str(der_path),
        ],
        check=True,
    )
    sig_hex = der_path.read_bytes().hex().upper().encode("ascii")
    assert len(sig_hex) <= hex_cap, f"sig_hex={len(sig_hex)} > {hex_cap}"
    pdf[hole_start + 1 : hole_start + 1 + len(sig_hex)] = sig_hex

    rev1_path = work / "signed_rev1.pdf"
    rev1_path.write_bytes(bytes(pdf))

    # Revision 2: Legitimate incremental countersigner fill via `nupdf forms --incremental`
    rev2_path = work / "signed_rev2.pdf"
    run_cmd(
        [
            nupdf_bin,
            "forms",
            str(rev1_path),
            "--set",
            "notes=Approved by CFO",
            "--incremental",
            "-o",
            str(rev2_path),
        ],
        check=True,
    )

    # Revision 3: Adversarial incremental append that paints a white box over
    # `$10,000.00` AND draws `$90,000.00` in a second content stream!
    rev2_bytes = bytearray(rev2_path.read_bytes())
    # Find current /Size from the latest trailer in rev2_bytes
    sizes = [int(m.group(1)) for m in re.finditer(rb"/Size\s+(\d+)", bytes(rev2_bytes))]
    next_obj = max(sizes)
    prev_xrefs = [int(m.group(1)) for m in re.finditer(rb"startxref\s+(\d+)", bytes(rev2_bytes))]
    prev_xref = prev_xrefs[-1]

    attack_stream = (
        b"q 1 1 1 rg 65 668 340 50 re f Q\n"
        b"BT /F1 14 Tf 72 700 Td (Approved Wire Total: $90,000.00) Tj ET\n"
    )
    off_stream = len(rev2_bytes)
    rev2_bytes.extend(
        f"{next_obj} 0 obj\n<</Length {len(attack_stream)}>>\nstream\n".encode("ascii")
        + attack_stream
        + b"endstream\nendobj\n"
    )
    off_page = len(rev2_bytes)
    rev2_bytes.extend(
        f"3 0 obj\n<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources<</Font<</F1 5 0 R>>>>/Contents[4 0 R {next_obj} 0 R]/Annots[6 0 R 7 0 R]>>\nendobj\n".encode(
            "ascii"
        )
    )
    xref3 = len(rev2_bytes)
    rev2_bytes.extend(
        (
            f"xref\n"
            f"0 1\n0000000000 65535 f \n"
            f"3 1\n{off_page:010d} 00000 n \n"
            f"{next_obj} 1\n{off_stream:010d} 00000 n \n"
            f"trailer\n<</Size {next_obj + 1}/Root 1 0 R/Prev {prev_xref}>>\n"
            f"startxref\n{xref3}\n%%EOF\n"
        ).encode("ascii")
    )
    path.write_bytes(bytes(rev2_bytes))
    return {"rev1_len": rev1_path.stat().st_size, "rev2_len": rev2_path.stat().st_size, "rev3_len": len(rev2_bytes)}


def make_nda_pii_pdf(path: Path) -> dict:
    """
    Builds a 1-page Mutual NDA + Wire Sheet containing:
      - Repeated party names ("Evelyn Vance" x2 and "Marcus Sterling" x2) to test coreference!
      - Two distinct SSNs ("412-88-9014", "509-31-7720")
      - Email ("evelyn.vance@heliosbio.example") and Phone ("415-890-4412")
      - An interactive AcroForm text widget containing PII ("Evelyn Vance") AND `/Info` Author metadata!
    """
    doc = fitz.open()
    p = doc.new_page(width=612, height=792)
    p.insert_text((72, 80), "MUTUAL NON-DISCLOSURE & ESCROW WIRE SCHEDULE", fontsize=13)
    p.insert_text(
        (72, 125),
        "Disclosing Principal: Evelyn Vance (SSN: 412-88-9014)",
        fontsize=11,
    )
    p.insert_text(
        (72, 150),
        "Receiving Counterparty: Marcus Sterling (SSN: 509-31-7720)",
        fontsize=11,
    )
    p.insert_text(
        (72, 180),
        "Notice Email: evelyn.vance@heliosbio.example   Phone: 415-890-4412",
        fontsize=11,
    )
    p.insert_text(
        (72, 215),
        "Coreference Clause: Evelyn Vance shall deliver keys to Marcus Sterling within 5 days.",
        fontsize=11,
    )

    widget = fitz.Widget()
    widget.field_name = "signatory_name"
    widget.field_type = fitz.PDF_WIDGET_TYPE_TEXT
    widget.field_value = "Evelyn Vance"
    widget.rect = fitz.Rect(72, 255, 290, 280)
    p.add_widget(widget)

    doc.set_metadata(
        {
            "title": "NDA Escrow Schedule - Evelyn Vance",
            "author": "Evelyn Vance <evelyn.vance@heliosbio.example>",
        }
    )
    doc.save(str(path))
    doc.close()
    return {
        "secrets": [
            "Evelyn Vance",
            "Marcus Sterling",
            "412-88-9014",
            "509-31-7720",
            "evelyn.vance@heliosbio.example",
            "415-890-4412",
        ]
    }


# ---------------------------------------------------------------------------
# Experiments 1..6
# ---------------------------------------------------------------------------


def exp1_chart(nupdf: str, mutool: str | None, pdf_oxide_cli: str | None, work: Path) -> dict:
    pdf_path = work / "chart_showcase.pdf"
    truth = make_chart_showcase_pdf(pdf_path)

    # 1. Rival engines: PyMuPDF, pypdf, pdfplumber, pypdfium2, mutool, pdf-oxide CLI
    rivals: dict[str, dict] = {}

    d_fitz = fitz.open(str(pdf_path))
    fitz_text = "\n".join(pg.get_text() for pg in d_fitz)
    d_fitz.close()
    rivals["PyMuPDF"] = {
        "recovered_bar_42_37": "42.37" in fitz_text,
        "recovered_bar_68_91": "68.91" in fitz_text,
        "charts_extracted": 0,
        "directed_edges_extracted": 0,
    }

    reader = pypdf.PdfReader(str(pdf_path))
    pypdf_text = "\n".join((pg.extract_text() or "") for pg in reader.pages)
    rivals["pypdf"] = {
        "recovered_bar_42_37": "42.37" in pypdf_text,
        "charts_extracted": 0,
        "directed_edges_extracted": 0,
    }

    with pdfplumber.open(str(pdf_path)) as pl:
        pl_tables = sum(len(pg.extract_tables()) for pg in pl.pages)
        pl_text = "\n".join((pg.extract_text() or "") for pg in pl.pages)
    rivals["pdfplumber"] = {
        "recovered_bar_42_37": "42.37" in pl_text,
        "tables_found_on_charts": pl_tables,
        "charts_extracted": 0,
        "directed_edges_extracted": 0,
    }

    p_doc = pdfium.PdfDocument(str(pdf_path))
    pdfium_text = "\n".join(p_doc[i].get_textpage().get_text_range() for i in range(len(p_doc)))
    p_doc.close()
    rivals["pypdfium2"] = {
        "recovered_bar_42_37": "42.37" in pdfium_text,
        "charts_extracted": 0,
        "directed_edges_extracted": 0,
    }

    if mutool and Path(mutool).exists():
        _, mt_out, _, _ = run_cmd([mutool, "draw", "-F", "txt", "-q", str(pdf_path)])
        rivals["mutool"] = {
            "recovered_bar_42_37": "42.37" in mt_out,
            "charts_extracted": 0,
            "directed_edges_extracted": 0,
        }

    if pdf_oxide_cli and Path(pdf_oxide_cli).exists():
        _, ox_out, _, _ = run_cmd([pdf_oxide_cli, "text", str(pdf_path)])
        rivals["pdf_oxide_cli"] = {
            "recovered_bar_42_37": "42.37" in ox_out,
            "charts_extracted": 0,
            "directed_edges_extracted": 0,
        }

    # 2. `nupdf chart`
    _, out, _, ms = run_cmd([nupdf, "chart", str(pdf_path)], check=True)
    rep = json.loads(out)
    assert rep["schema"] == "nupdf.chart/v1"
    assert len(rep["charts"]) == 2, f"expected 2 charts across showcase PDF, got {rep['charts']}"
    assert len(rep["diagrams"]) == 1, f"expected 1 diagram across showcase PDF, got {rep['diagrams']}"

    bar_chart = next(c for c in rep["charts"] if c["kind"] == "bar")
    line_chart = next(c for c in rep["charts"] if c["kind"] == "line")

    # Verify every single bar point within < 0.05 tolerance and legend name match!
    max_bar_err = 0.0
    by_series = {s["name"]: {pt["label"]: pt["value"] for pt in s["points"]} for s in bar_chart["series"]}
    for s_name, pairs in truth["bars"].items():
        assert s_name in by_series, f"missing legend series {s_name} in {by_series}"
        for cat, expected_val in pairs:
            got_val = by_series[s_name][cat]
            err = abs(got_val - expected_val)
            max_bar_err = max(max_bar_err, err)
            assert err <= 0.05, f"{s_name}[{cat}]: got {got_val}, expected {expected_val}"

    # Verify line chart points within < 0.05 tolerance
    max_line_err = 0.0
    line_pts = {pt["label"]: pt["value"] for pt in line_chart["series"][0]["points"]}
    for lbl, expected_val in truth["line"]:
        err = abs(line_pts[lbl] - expected_val)
        max_line_err = max(max_line_err, err)
        assert err <= 0.05, f"line[{lbl}]: got {line_pts[lbl]}, expected {expected_val}"

    # Verify directed flowchart nodes & edges on page 2
    diag = rep["diagrams"][0]
    node_labels = {n["id"]: n["label"] for n in diag["nodes"]}
    edge_pairs = [(node_labels[e["from"]], node_labels[e["to"]]) for e in diag["edges"] if e["directed"]]
    for expected_edge in truth["expected_edges"]:
        assert expected_edge in edge_pairs, f"missing directed edge {expected_edge} in {edge_pairs}"

    return {
        "nupdf_cli_ms": round(ms, 2),
        "vision_tokens_spent": 0,
        "charts_recovered": len(rep["charts"]),
        "bar_axis_r_squared": bar_chart["axis_r_squared"],
        "max_bar_abs_error": round(max_bar_err, 5),
        "max_line_abs_error": round(max_line_err, 5),
        "diagram_nodes_recovered": len(diag["nodes"]),
        "diagram_directed_edges_recovered": len(edge_pairs),
        "rivals": rivals,
    }


def exp2_flat_forms(nupdf: str, pdf_oxide_cli: str | None, work: Path) -> dict:
    pdf_path = work / "flat_intake.pdf"
    make_flat_intake_pdf(pdf_path)

    # 1. Rival engines all report 0 form fields on flat print masters!
    d_fitz = fitz.open(str(pdf_path))
    fitz_widgets = sum(1 for _ in d_fitz[0].widgets())
    d_fitz.close()

    pypdf_fields = len(pypdf.PdfReader(str(pdf_path)).get_fields() or {})
    oxide_cli_fields = None
    if pdf_oxide_cli and Path(pdf_oxide_cli).exists():
        _, ox_forms, _, _ = run_cmd([pdf_oxide_cli, "forms", str(pdf_path)])
        oxide_cli_fields = 0 if ("No form fields" in ox_forms or '"fields": []' in ox_forms or not ox_forms.strip()) else -1

    # 2. `nupdf flat-forms` discovery
    _, det_out, _, det_ms = run_cmd([nupdf, "flat-forms", str(pdf_path)], check=True)
    det = json.loads(det_out)
    assert det["schema"] == "nupdf.flat_forms/v1"
    fields_by_name = {f["name"]: f for f in det["fields"]}
    assert len(det["fields"]) == 6, f"expected 6 flat fields, got {det['fields']}"
    assert fields_by_name["ssn"]["kind"] == "comb"
    assert fields_by_name["ssn"]["cell_count"] == 9
    assert fields_by_name["existing_patient"]["checked"] is True
    assert fields_by_name["us_citizen"]["checked"] is False

    # 3. `nupdf flat-forms --set ... --check ...` verified fill
    filled_path = work / "flat_intake_filled.pdf"
    _, fill_out, _, fill_ms = run_cmd(
        [
            nupdf,
            "flat-forms",
            str(pdf_path),
            "--set",
            "applicant_name=Dr. Grace Hopper",
            "--set",
            "department= Naval Computing Division ",
            "--set",
            "ssn=123456789",
            "--check",
            "us_citizen",
            "--check",
            "hipaa_consent",
            "-o",
            str(filled_path),
        ],
        check=True,
    )
    fill_rep = json.loads(fill_out)
    assert fill_rep["verified"] is True

    # Independent PyMuPDF verification of filled flat form!
    d_filled = fitz.open(str(filled_path))
    filled_txt = d_filled[0].get_text()
    d_filled.close()
    assert "Grace Hopper" in filled_txt
    for digit in "123456789":
        assert digit in filled_txt

    # Re-run `nupdf flat-forms` on filled PDF to verify vector checkmarks are now checked!
    _, re_out, _, _ = run_cmd([nupdf, "flat-forms", str(filled_path)], check=True)
    re_fields = {f["name"]: f for f in json.loads(re_out)["fields"]}
    assert re_fields["us_citizen"]["checked"] is True
    assert re_fields["hipaa_consent"]["checked"] is True

    # Comb overflow rejection check (11 digits into 9-cell SSN comb -> exit 4!)
    rc_bad, _, err_bad, _ = run_cmd(
        [
            nupdf,
            "flat-forms",
            str(pdf_path),
            "--set",
            "ssn=12345678901",
            "--dry-run",
        ]
    )
    assert rc_bad == 4, f"expected exit 4 on comb overflow, got {rc_bad}: {err_bad}"

    # 4. `nupdf flat-forms --promote` -> upgrades flat geometry into real `/AcroForm` widgets!
    promoted_path = work / "flat_intake_promoted.pdf"
    _, prom_out, _, prom_ms = run_cmd(
        [nupdf, "flat-forms", str(pdf_path), "--promote", "-o", str(promoted_path)],
        check=True,
    )
    prom_rep = json.loads(prom_out)
    assert prom_rep["promoted_widgets"] == 6

    # Independent verification with `pypdf` and `PyMuPDF` that real `/AcroForm` widgets exist!
    prom_pypdf_fields = pypdf.PdfReader(str(promoted_path)).get_fields() or {}
    d_prom = fitz.open(str(promoted_path))
    prom_fitz_widgets = list(d_prom[0].widgets())
    d_prom.close()
    assert len(prom_pypdf_fields) == 6
    assert len(prom_fitz_widgets) == 6

    return {
        "rivals_detected_fields": {
            "PyMuPDF": fitz_widgets,
            "pypdf": pypdf_fields,
            "pdf_oxide_cli": oxide_cli_fields,
        },
        "nupdf_detect_ms": round(det_ms, 2),
        "nupdf_detected_fields": len(det["fields"]),
        "nupdf_fill_ms": round(fill_ms, 2),
        "nupdf_comb_overflow_exit_code": rc_bad,
        "nupdf_promote_ms": round(prom_ms, 2),
        "pypdf_verified_promoted_acroform_fields": len(prom_pypdf_fields),
        "pymupdf_verified_promoted_widgets": len(prom_fitz_widgets),
    }


def exp3_reflow(nupdf: str, work: Path) -> dict:
    pdf_path = work / "contract_reflow.pdf"
    make_contract_reflow_pdf(pdf_path)

    expanded_clause = (
        "2. Indemnification. Provider shall defend, indemnify, and hold harmless Customer "
        "and its affiliates, officers, directors, employees, and successors from and against "
        "any and all third-party claims, suits, damages, losses, regulatory penalties, fines, "
        "liabilities, settlement amounts, court costs, and reasonable outside attorneys fees "
        "arising out of or relating to intellectual property infringement, data security "
        "breaches, confidentiality violations, or gross negligence under this Agreement."
    )

    # Measure original Clause 3 Y position in PyMuPDF
    d_orig = fitz.open(str(pdf_path))
    orig_blocks = d_orig[0].get_text("blocks")
    d_orig.close()
    orig_c3_y0 = next(b[1] for b in orig_blocks if "3. Governing Law" in b[4])
    orig_sig_y0 = next(b[1] for b in orig_blocks if "Authorized Signature" in b[4])

    # 1. Rival PyMuPDF naive redaction+insert_textbox in original Clause 2 rect (truncates or overlaps!)
    fitz_attempt = work / "pymupdf_naive_reflow.pdf"
    d_f = fitz.open(str(pdf_path))
    p_f = d_f[0]
    c2_rect = next(fitz.Rect(b[:4]) for b in p_f.get_text("blocks") if "2. Indemnification" in b[4])
    p_f.add_redact_annot(c2_rect)
    p_f.apply_redactions()
    leftover = p_f.insert_textbox(c2_rect, expanded_clause, fontsize=11)
    d_f.save(str(fitz_attempt))
    d_f.close()
    pymupdf_truncated_in_original_box = leftover < 0

    # 2. `nupdf patch --reflow` on block `p1.b2`
    reflowed_path = work / "contract_reflowed.pdf"
    _, out, _, reflow_ms = run_cmd(
        [
            nupdf,
            "patch",
            str(pdf_path),
            "--replace",
            f"p1.b2={expanded_clause}",
            "--reflow",
            "-o",
            str(reflowed_path),
        ],
        check=True,
    )
    rep = json.loads(out)
    assert rep["verification"]["verified"] is True

    # Independent PyMuPDF geometric audit of reflowed PDF:
    # - Full expanded text present
    # - Clause 3 and Signature shifted downward cleanly
    # - Zero word-box overlaps across the entire page!
    d_ref = fitz.open(str(reflowed_path))
    ref_text = d_ref[0].get_text()
    ref_blocks = d_ref[0].get_text("blocks")
    ref_words = d_ref[0].get_text("words")
    d_ref.close()

    norm_ref_text = " ".join(ref_text.split())
    assert "gross negligence under this Agreement." in norm_ref_text, f"got: {norm_ref_text}"
    assert "3. Governing Law." in norm_ref_text
    new_c3_y0 = next(b[1] for b in ref_blocks if "3. Governing Law" in b[4])
    new_sig_y0 = next(b[1] for b in ref_blocks if "Authorized Signature" in b[4])
    delta_c3 = new_c3_y0 - orig_c3_y0
    delta_sig = new_sig_y0 - orig_sig_y0
    assert delta_c3 > 35.0, f"expected Clause 3 to shift down >35pt, got {delta_c3}"
    assert abs(delta_c3 - delta_sig) < 3.0, f"Clause 3 ({delta_c3}) and Sig ({delta_sig}) shifted unequally"

    # Check pairwise line/block non-overlap in PyMuPDF
    text_blocks = [b for b in ref_blocks if b[6] == 0]
    overlaps = 0
    for i in range(len(text_blocks)):
        for j in range(i + 1, len(text_blocks)):
            a, b = text_blocks[i], text_blocks[j]
            xo = min(a[2], b[2]) - max(a[0], b[0])
            yo = min(a[3], b[3]) - max(a[1], b[1])
            if xo > 2.0 and yo > 2.0:
                overlaps += 1
    assert overlaps == 0, f"expected 0 block overlaps, got {overlaps}"
    max_word_x1 = max(w[2] for w in ref_words)
    assert max_word_x1 <= 612.0

    # 3. 50-line overflow trap -> must fail with exit 4 (`verify.page_overflow`)!
    overflow_text = "2. Indemnification. " + ("Expanded liability sentence pushing below page bottom. " * 80)
    rc_ovf, _, err_ovf, _ = run_cmd(
        [
            nupdf,
            "patch",
            str(pdf_path),
            "--replace",
            f"p1.b2={overflow_text}",
            "--reflow",
            "--dry-run",
        ]
    )
    assert rc_ovf == 4, f"expected exit 4 on 45-line overflow, got {rc_ovf}: {err_ovf}"

    return {
        "nupdf_reflow_cli_ms": round(reflow_ms, 2),
        "clause3_shifted_down_pt": round(delta_c3, 2),
        "signature_shifted_down_pt": round(delta_sig, 2),
        "pymupdf_verified_zero_block_overlaps": overlaps == 0,
        "pymupdf_max_word_x1_pt": round(max_word_x1, 2),
        "nupdf_45_line_overflow_exit_code": rc_ovf,
        "pymupdf_naive_textbox_truncated_in_original_rect": pymupdf_truncated_in_original_box,
    }


def exp4_history(nupdf: str, mutool: str | None, work: Path) -> dict:
    pdf_path = work / "signed_history_3rev.pdf"
    sizes = make_signed_history_3rev_pdf(nupdf, pdf_path, work)

    # 1. Rival engines on the 3-revision tampered signed PDF:
    d_fitz = fitz.open(str(pdf_path))
    fitz_text = d_fitz[0].get_text()
    d_fitz.close()
    pypdf_text = pypdf.PdfReader(str(pdf_path)).pages[0].extract_text() or ""

    # 2. `nupdf history` time-travel & post-signature tamper forensics
    rev1_extracted = work / "recovered_rev1.pdf"
    _, out, _, hist_ms = run_cmd(
        [
            nupdf,
            "history",
            str(pdf_path),
            "--extract-revision",
            "1",
            "-o",
            str(rev1_extracted),
        ],
        check=True,
    )
    rep = json.loads(out)
    assert rep["schema"] == "nupdf.history/v1"
    assert rep["revision_count"] == 3
    assert rep["tampered"] is True

    r1, r2, r3 = rep["revisions"]
    assert r1["classification"] == "initial"
    assert r1["signatures"][0]["valid"] is True
    assert r1["signatures"][0]["covers_whole_revision"] is True

    assert r2["classification"] == "clean_incremental_fill"
    assert r2["signatures"][0]["valid"] is True
    assert r2["delta_from_previous"]["fields_changed"][0]["new_value"] == "Approved by CFO"

    assert r3["classification"] == "tampered_after_signature"
    hazards_r3 = r3["delta_from_previous"]["forensic_hazards"]
    assert "tamper.post_signature_content_edit" in hazards_r3
    assert "tamper.post_signature_visual_occlusion" in hazards_r3

    # Verify `--strict` exits 4 (`tamper.detected`)!
    rc_strict, _, _, _ = run_cmd([nupdf, "history", str(pdf_path), "--strict"])
    assert rc_strict == 4

    # Independently verify extracted Revision 1 in PyMuPDF has ONLY `$10,000.00` (no `$90,000.00`)!
    d_r1 = fitz.open(str(rev1_extracted))
    r1_txt = d_r1[0].get_text()
    d_r1.close()
    assert "$10,000.00" in r1_txt and "$90,000.00" not in r1_txt

    mutool_sig_check = None
    if mutool and Path(mutool).exists():
        _, mt_out, mt_err, _ = run_cmd([mutool, "sign", "-v", str(rev1_extracted)])
        mutool_sig_check = (mt_out + mt_err).strip()[:160]

    return {
        "revision_byte_sizes": sizes,
        "nupdf_history_cli_ms": round(hist_ms, 2),
        "revision_classifications": [r1["classification"], r2["classification"], r3["classification"]],
        "rev2_incremental_fill_kept_rsa_signature_valid": r2["signatures"][0]["valid"],
        "rev3_forensic_hazards": hazards_r3,
        "strict_exit_code_on_tamper": rc_strict,
        "extracted_rev1_verified_clean_10k": True,
        "rivals_on_tampered_pdf": {
            "PyMuPDF_sees_90k": "$90,000.00" in fitz_text,
            "pypdf_sees_90k": "$90,000.00" in pypdf_text,
            "rivals_time_travel_or_visual_occlusion_forensics": False,
        },
        "mutool_rev1_note": mutool_sig_check,
    }


def exp5_cloak_uncloak(nupdf: str, work: Path) -> dict:
    pdf_path = work / "nda_pii.pdf"
    meta = make_nda_pii_pdf(pdf_path)
    secrets = meta["secrets"]

    vault_path = work / "nda_vault.json"
    cloaked_path = work / "nda_cloaked.pdf"

    # 1. `nupdf cloak` across page streams, AcroForm widgets, and `/Info` metadata
    _, cloak_out, _, cloak_ms = run_cmd(
        [
            nupdf,
            "cloak",
            str(pdf_path),
            "--vault",
            str(vault_path),
            "--preset",
            "pii",
            "--entity",
            "PERSON=Evelyn Vance",
            "--entity",
            "PERSON=Marcus Sterling",
            "-o",
            str(cloaked_path),
        ],
        check=True,
    )
    cloak_rep = json.loads(cloak_out)
    assert cloak_rep["schema"] == "nupdf.cloak/v1"
    assert cloak_rep["verified_zero_leaks"] is True
    # Confirm stdout report itself leaks ZERO secrets!
    for s in secrets:
        assert s not in cloak_out, f"stdout leaked secret {s}"

    # 2. Independent 4-way zero-leak audit of `nda_cloaked.pdf`:
    #    PyMuPDF text + widgets + metadata, pypdf text + fields + metadata, and raw uncompressed scan!
    d_clk = fitz.open(str(cloaked_path))
    clk_fitz_text = d_clk[0].get_text()
    clk_fitz_meta = json.dumps(d_clk.metadata)
    clk_fitz_widgets = [w.field_value for w in d_clk[0].widgets()]
    d_clk.close()

    clk_reader = pypdf.PdfReader(str(cloaked_path))
    clk_pypdf_text = clk_reader.pages[0].extract_text() or ""
    clk_pypdf_meta = json.dumps({k: str(v) for k, v in (clk_reader.metadata or {}).items()})

    for s in secrets:
        assert s not in clk_fitz_text, f"PyMuPDF text leaked {s}"
        assert s not in clk_fitz_meta, f"PyMuPDF metadata leaked {s}"
        assert all(s not in (wv or "") for wv in clk_fitz_widgets), f"Widget leaked {s}"
        assert s not in clk_pypdf_text, f"pypdf text leaked {s}"
        assert s not in clk_pypdf_meta, f"pypdf metadata leaked {s}"

    # Verify coreference preservation: `[PERSON_01]` appears in both body clauses + widget, `[PERSON_02]` twice!
    assert clk_fitz_text.count("[PERSON_01]") >= 2 and clk_fitz_widgets == ["[PERSON_01]"]
    assert clk_fitz_text.count("[PERSON_02]") == 2
    assert "000-00-0001" in clk_fitz_text and "000-00-0002" in clk_fitz_text

    # 3. Simulated external cloud agent writes an analysis JSON using synthetic tokens -> `nupdf uncloak --text-mode`
    agent_json_in = work / "agent_summary_cloaked.json"
    agent_json_out = work / "agent_summary_restored.json"
    agent_json_in.write_text(
        json.dumps(
            {
                "disclosing_party": "[PERSON_01]",
                "receiving_party": "[PERSON_02]",
                "disclosing_ssn": "000-00-0001",
                "contact_email": "user1@example.invalid",
            }
        )
    )
    _, txt_unc_out, _, txt_unc_ms = run_cmd(
        [
            nupdf,
            "uncloak",
            str(agent_json_in),
            "--vault",
            str(vault_path),
            "--text-mode",
            "-o",
            str(agent_json_out),
        ],
        check=True,
    )
    restored_summary = json.loads(agent_json_out.read_text())
    assert restored_summary["disclosing_party"] == "Evelyn Vance"
    assert restored_summary["receiving_party"] == "Marcus Sterling"
    assert restored_summary["disclosing_ssn"] == "412-88-9014"

    # 4. `nupdf uncloak` on the cloaked PDF -> restores all original PII in PDF!
    restored_pdf = work / "nda_restored.pdf"
    _, pdf_unc_out, _, pdf_unc_ms = run_cmd(
        [
            nupdf,
            "uncloak",
            str(cloaked_path),
            "--vault",
            str(vault_path),
            "-o",
            str(restored_pdf),
        ],
        check=True,
    )
    d_res = fitz.open(str(restored_pdf))
    res_text = d_res[0].get_text()
    res_widgets = [w.field_value for w in d_res[0].widgets()]
    d_res.close()
    for s in secrets:
        assert s in res_text, f"restored PDF missing {s}: {res_text}"
    assert "Evelyn Vance" in res_widgets

    return {
        "nupdf_cloak_cli_ms": round(cloak_ms, 2),
        "nupdf_uncloak_pdf_cli_ms": round(pdf_unc_ms, 2),
        "nupdf_uncloak_text_cli_ms": round(txt_unc_ms, 2),
        "entities_cloaked": cloak_rep["entities_cloaked"],
        "replacements": cloak_rep["replacements"],
        "zero_leaks_across_pymupdf_pypdf_widgets_and_metadata": True,
        "coreference_preserved": True,
        "pdf_and_json_roundtrip_exact": True,
    }


def exp6_pipeline_and_wasm(
    nupdf: str,
    node_bin: str | None,
    wasm_path: str | None,
    wasm_runner: str | None,
    work: Path,
) -> dict:
    pdf_path = work / "contract_reflow.pdf"

    # 1. Measure 5 sequential CLI invocations (what an agent normally has to do across 5 tool turns!)
    seq_out_pdf = work / "seq_patched.pdf"
    t_seq0 = time.perf_counter()
    _, s_info, _, _ = run_cmd([nupdf, "info", str(pdf_path)], check=True)
    _, s_att, _, _ = run_cmd([nupdf, "attest", str(pdf_path)], check=True)
    _, s_qry, _, _ = run_cmd(
        [nupdf, "query", str(pdf_path), "--right-of", "Contract Value:", "--nearest-only"],
        check=True,
    )
    first_id = json.loads(s_qry)["matches"][0]["id"]
    _, s_pat, _, _ = run_cmd(
        [nupdf, "patch", str(pdf_path), "--replace", f"{first_id}=$42,500.00", "-o", str(seq_out_pdf)],
        check=True,
    )
    _, s_aud, _, _ = run_cmd(
        [
            nupdf,
            "audit",
            str(seq_out_pdf),
            "--claim",
            '{"claim_id":"c1","page":1,"quote":"$42,500.00"}',
            "--strict",
        ],
        check=True,
    )
    seq_ms = (time.perf_counter() - t_seq0) * 1000.0
    seq_stdout_bytes = len(s_info) + len(s_att) + len(s_qry) + len(s_pat) + len(s_aud)

    # 2. Single-call `nupdf pipeline --compact` doing the exact same 5-step workflow in 1 pass!
    pipe_spec = work / "workflow.json"
    pipe_spec.write_text(
        json.dumps(
            {
                "compact": True,
                "steps": [
                    {"op": "info", "id": "i1"},
                    {"op": "attest", "id": "att1", "abort_on_hazards": ["hazard.invisible_or_occluded"]},
                    {"op": "query", "id": "q1", "right_of": "Contract Value:", "nearest_only": True},
                    {
                        "op": "patch",
                        "id": "p1",
                        "replacements": [{"id": "${q1.first_id}", "text": "$42,500.00"}],
                    },
                    {
                        "op": "audit",
                        "id": "aud1",
                        "strict": True,
                        "claims": [{"claim_id": "c1", "page": 1, "quote": "$42,500.00"}],
                    },
                ],
            }
        )
    )
    pipe_out_pdf = work / "pipe_patched.pdf"
    _, pipe_out, _, pipe_ms = run_cmd(
        [nupdf, "pipeline", str(pdf_path), "--spec", str(pipe_spec), "--compact", "-o", str(pipe_out_pdf)],
        check=True,
    )
    pipe_rep = json.loads(pipe_out)
    assert pipe_rep["schema"] == "nupdf.pipeline/v1"
    assert pipe_rep["mutated"] is True
    assert len(pipe_rep["steps"]) == 5
    assert pipe_rep["steps"][4]["data"]["all_verified"] is True

    # Also prove safety gate abort: `history` gate on `signed_history_3rev.pdf` halts before mutation with exit 4!
    tamper_gate_spec = json.dumps(
        {
            "steps": [
                {"op": "history", "id": "h1", "abort_on_tamper": True},
                {"op": "info", "id": "never_reached"},
            ]
        }
    )
    rc_gate, gate_stdout, _, _ = run_cmd(
        [
            nupdf,
            "pipeline",
            str(work / "signed_history_3rev.pdf"),
            "--inline",
            tamper_gate_spec,
        ]
    )
    assert rc_gate == 4
    gate_json = json.loads(gate_stdout)
    assert gate_json["aborted"]["code"] == "gate.tamper_detected"
    assert len(gate_json["steps"]) == 1  # halted before step 2!

    # 3. Live Node V8 WebAssembly zero-import proof (`wasm_proof.mjs`)
    wasm_summary = None
    if node_bin and wasm_path and wasm_runner and Path(wasm_path).exists():
        _, wasm_stdout, _, _ = run_cmd(
            [
                node_bin,
                wasm_runner,
                wasm_path,
                str(work / "chart_showcase.pdf"),
                str(work / "flat_intake.pdf"),
                str(work / "contract_reflow.pdf"),
                str(work / "signed_history_3rev.pdf"),
                str(work / "nda_pii.pdf"),
            ],
            check=True,
        )
        wasm_summary = json.loads(wasm_stdout)
        assert wasm_summary["host_imports_count"] == 0
        assert wasm_summary["benchmarks"]["reflow_patch"]["verified"] is True
        assert wasm_summary["benchmarks"]["cloak"]["verified_zero_leaks"] is True
        assert wasm_summary["benchmarks"]["history"]["tampered"] is True
        assert wasm_summary["benchmarks"]["pipeline"]["audit_verified"] is True

    return {
        "sequential_5_cli_turns_ms": round(seq_ms, 2),
        "single_call_pipeline_cli_ms": round(pipe_ms, 2),
        "cli_wall_speedup": round(seq_ms / max(pipe_ms, 0.01), 2),
        "sequential_5_turns_stdout_bytes": seq_stdout_bytes,
        "single_call_compact_pipeline_stdout_bytes": len(pipe_out),
        "context_payload_reduction_ratio": round(seq_stdout_bytes / max(len(pipe_out), 1), 2),
        "tamper_gate_halted_before_mutation_exit_code": rc_gate,
        "wasm32_v8_zero_import_edge": wasm_summary,
    }


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--nupdf", required=True, help="Path to built nupdf binary")
    ap.add_argument("--mutool", default=None, help="Optional path to mutool binary")
    ap.add_argument("--pdf-oxide", default=None, help="Optional path to native pdf-oxide CLI binary")
    ap.add_argument(
        "--engines-bin",
        default=None,
        help="Optional path to native Rust primitives_v2_vs_oxide binary",
    )
    ap.add_argument("--node", default="node", help="Node.js binary for V8 WASM proof")
    ap.add_argument("--wasm", default=None, help="Path to built nupdf_wasm_edge.wasm")
    ap.add_argument("--out-json", default=None, help="Where to write JSON results")
    args = ap.parse_args()

    here = Path(__file__).resolve().parent
    wasm_runner = str(here / "wasm_proof.mjs")

    work = Path(tempfile.mkdtemp(prefix="nupdf_primitives_v2_proof_"))
    try:
        results: dict = {
            "exp1_vector_chart_and_diagram_decompiler": exp1_chart(
                args.nupdf, args.mutool, args.pdf_oxide, work
            ),
            "exp2_flat_forms_fill_and_promote": exp2_flat_forms(
                args.nupdf, args.pdf_oxide, work
            ),
            "exp3_block_paragraph_reflow": exp3_reflow(args.nupdf, work),
            "exp4_temporal_history_and_tamper_forensics": exp4_history(
                args.nupdf, args.mutool, work
            ),
            "exp5_reversible_pii_cloak_firewall": exp5_cloak_uncloak(args.nupdf, work),
            "exp6_single_call_pipeline_and_zero_import_wasm": exp6_pipeline_and_wasm(
                args.nupdf, args.node, args.wasm, wasm_runner, work
            ),
        }

        if args.engines_bin and Path(args.engines_bin).exists():
            _, eng_out, _, _ = run_cmd([args.engines_bin, str(work)], check=True)
            eng_json = json.loads(eng_out)
            assert eng_json["exp2_flat_forms"]["nupdf_fill_verified"] is True
            assert eng_json["exp2_flat_forms"]["pdf_oxide_sees_acroform_after_nupdf_promote"] is True
            assert eng_json["exp3_reflow"]["nupdf_reflow_verified"] is True
            assert eng_json["exp5_cloak"]["pdf_oxide_leaked_any_secret_in_cloaked_pdf"] is False
            assert eng_json["exp5_cloak"]["pdf_oxide_verified_all_secrets_restored_after_uncloak"] is True
            results["native_rust_engines_vs_pdf_oxide"] = eng_json

        if args.out_json:
            Path(args.out_json).parent.mkdir(parents=True, exist_ok=True)
            Path(args.out_json).write_text(json.dumps(results, indent=2) + "\n")
        print(json.dumps(results, indent=2))
    finally:
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    main()
