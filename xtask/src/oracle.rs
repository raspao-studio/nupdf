// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! The reference oracle: MuPDF built from source at a pinned version, used
//! out of process so its crashes and hangs never take the harness down.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

const REPO: &str = "https://github.com/ArtifexSoftware/mupdf.git";

pub fn version(root: &Path) -> Result<String> {
    let path = root.join("oracle/MUPDF_VERSION");
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    Ok(text.trim().to_string())
}

pub fn source_dir(root: &Path) -> Result<PathBuf> {
    Ok(root.join(format!("target/oracle/mupdf-{}", version(root)?)))
}

/// Path to the pinned `mutool`. `NUPDF_MUTOOL` overrides it, but must report
/// the pinned version so results stay comparable to the baselines.
pub fn mutool(root: &Path) -> Result<PathBuf> {
    let pinned = version(root)?;
    let path = match std::env::var_os("NUPDF_MUTOOL") {
        Some(path) => PathBuf::from(path),
        None => source_dir(root)?.join("build/release/mutool"),
    };
    if !path.exists() {
        bail!(
            "oracle not built at {}; run `cargo xtask oracle build`",
            path.display()
        );
    }
    let reported = reported_version(&path)?;
    if reported != pinned {
        bail!(
            "{} is MuPDF {reported}, but oracle/MUPDF_VERSION pins {pinned}",
            path.display()
        );
    }
    Ok(path)
}

fn reported_version(mutool: &Path) -> Result<String> {
    let out = Command::new(mutool)
        .arg("-v")
        .output()
        .with_context(|| format!("running {} -v", mutool.display()))?;
    // `mutool -v` prints "mutool version 1.28.6" (to stderr on some builds).
    let text =
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    text.split_whitespace()
        .skip_while(|w| *w != "version")
        .nth(1)
        .map(str::to_string)
        .context("could not parse `mutool -v` output")
}

pub fn build(root: &Path) -> Result<()> {
    let version = version(root)?;
    let dir = source_dir(root)?;
    let mutool = dir.join("build/release/mutool");
    if mutool.exists() {
        println!("oracle MuPDF {version} already built: {}", mutool.display());
        return Ok(());
    }
    if !dir.exists() {
        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent)?;
        }
        run(Command::new("git")
            .args([
                "clone",
                "--quiet",
                "--depth",
                "1",
                "--branch",
                &version,
                "--recurse-submodules",
                "--shallow-submodules",
                REPO,
            ])
            .arg(&dir))?;
    }
    let jobs = std::thread::available_parallelism().map_or(4, |n| n.get());
    run(Command::new("make")
        .current_dir(&dir)
        .arg(format!("-j{jobs}"))
        // No viewer, no network: only the command-line tools.
        .args([
            "-s",
            "build=release",
            "HAVE_X11=no",
            "HAVE_GLUT=no",
            "HAVE_CURL=no",
            "tools",
        ]))?;
    let reported = reported_version(&mutool)?;
    if reported != version {
        bail!("built mutool reports {reported}, expected {version}");
    }
    println!("built oracle MuPDF {version}: {}", mutool.display());
    Ok(())
}

fn run(cmd: &mut Command) -> Result<()> {
    let status = cmd.status().with_context(|| format!("running {cmd:?}"))?;
    if !status.success() {
        bail!("{cmd:?} failed with {status}");
    }
    Ok(())
}
