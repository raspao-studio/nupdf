// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! `cargo xtask triage`: one page rendered by MuPDF (the oracle), by nupdf
//! and by hayro, a second reference, with how far each pair is apart. When
//! nupdf and MuPDF disagree, hayro's side says which of them is the odd
//! one out. Color management is off on both sides, as `mutool draw -N`.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::harness::{PIXEL_TOLERANCE, read_ppm_file};
use crate::proc;

type Rgb = (usize, usize, Vec<u8>);

/// hayro's rendering of page `index` at `dpi`, on white.
fn hayro_page(data: Vec<u8>, index: usize, dpi: f32) -> Result<Rgb> {
    use hayro::hayro_interpret::InterpreterSettings;
    use hayro::hayro_syntax::Pdf;
    use hayro::vello_cpu::color::palette::css::WHITE;
    use hayro::{PixmapSettings, RenderCache, RenderSettings, render};
    let pdf = Pdf::new(data).map_err(|e| anyhow::anyhow!("hayro cannot open the file: {e:?}"))?;
    let pages = pdf.pages();
    let page = pages.get(index).context("hayro has no such page")?;
    let cache = RenderCache::new();
    let scale = dpi / 72.0;
    let pix = render(
        page,
        &cache,
        &InterpreterSettings::default(),
        &RenderSettings::default(),
        &PixmapSettings {
            x_scale: scale,
            y_scale: scale,
            bg_color: WHITE,
        },
    );
    let (w, h) = (usize::from(pix.width()), usize::from(pix.height()));
    let rgb = pix
        .data_as_u8_slice()
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|p| [p[0], p[1], p[2]])
        .collect();
    Ok((w, h, rgb))
}

/// Pixels of the overlap of `a` and `b` (from the top left) off by more
/// than the tolerance, and the share of the overlap they are.
fn compare(a: &Rgb, b: &Rgb) -> Value {
    let (w, h) = (a.0.min(b.0), a.1.min(b.1));
    let mut different = 0u64;
    let mut max = 0u8;
    for y in 0..h {
        for x in 0..w {
            let pa = &a.2[(y * a.0 + x) * 3..][..3];
            let pb = &b.2[(y * b.0 + x) * 3..][..3];
            let d = pa
                .iter()
                .zip(pb)
                .map(|(u, v)| u.abs_diff(*v))
                .max()
                .unwrap_or(0);
            max = max.max(d);
            if d > PIXEL_TOLERANCE {
                different += 1;
            }
        }
    }
    let total = (w * h).max(1) as f64;
    json!({
        "sizes": [[a.0, a.1], [b.0, b.1]],
        "different": different,
        "share": different as f64 / total,
        "max": max,
    })
}

fn write_ppm(path: &Path, img: &Rgb) -> Result<()> {
    let mut out = format!("P6\n{} {}\n255\n", img.0, img.1).into_bytes();
    out.extend_from_slice(&img.2);
    std::fs::write(path, out)?;
    Ok(())
}

pub fn run(root: &Path, nupdf: &Path, file: &Path, page: usize, dpi: f32) -> Result<()> {
    if page == 0 {
        bail!("pages count from 1");
    }
    let out = root.join("target/triage");
    let _ = std::fs::remove_dir_all(&out);
    let (mu_dir, nu_dir) = (out.join("mupdf"), out.join("nupdf"));
    std::fs::create_dir_all(&mu_dir)?;
    std::fs::create_dir_all(&nu_dir)?;
    let timeout = std::time::Duration::from_secs(120);
    let mutool = crate::oracle::mutool(root)?;
    let script = root.join("oracle/js/render.js");
    let dpi_s = dpi.to_string();
    let args = [
        "run".as_ref(),
        script.as_os_str(),
        file.as_os_str(),
        mu_dir.as_os_str(),
        dpi_s.as_ref(),
    ];
    proc::run(&mutool, &args, timeout)?;
    let name = format!("page-{page:04}.ppm");
    let mupdf = read_ppm_file(&mu_dir.join(&name)).context("MuPDF did not render the page")?;
    let page_s = page.to_string();
    let args = [
        "render".as_ref(),
        "--no-icc".as_ref(),
        file.as_os_str(),
        "-o".as_ref(),
        nu_dir.as_os_str(),
        "--format".as_ref(),
        "ppm".as_ref(),
        "--dpi".as_ref(),
        dpi_s.as_ref(),
        "--pages".as_ref(),
        page_s.as_ref(),
    ];
    proc::run(nupdf, &args, timeout)?;
    let ours = read_ppm_file(&nu_dir.join(&name)).context("nupdf did not render the page")?;
    let hayro = hayro_page(std::fs::read(file)?, page - 1, dpi);
    let mut report = json!({
        "file": file.display().to_string(),
        "page": page,
        "dpi": dpi,
        "nupdf_vs_mupdf": compare(&ours, &mupdf),
    });
    match &hayro {
        Ok(h) => {
            write_ppm(&out.join("hayro.ppm"), h)?;
            report["hayro_vs_mupdf"] = compare(h, &mupdf);
            report["nupdf_vs_hayro"] = compare(&ours, h);
        }
        Err(e) => report["hayro_error"] = e.to_string().into(),
    }
    report["images"] = json!({
        "mupdf": mu_dir.join(&name).display().to_string(),
        "nupdf": nu_dir.join(&name).display().to_string(),
        "hayro": hayro.is_ok().then(|| out.join("hayro.ppm").display().to_string()),
    });
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
