// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! Corpus tiers. `smoke` is committed (generated fixtures). Every other tier
//! is assembled from the public sources in corpus/manifest.toml, fetched into
//! $NUPDF_CORPUS_DIR (default ~/.cache/nupdf/corpus) and never committed.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Manifest {
    source: Vec<Source>,
}

#[derive(Debug, Deserialize)]
pub struct Source {
    pub name: String,
    pub tier: String,
    pub about: String,
    pub license: String,
    #[serde(flatten)]
    pub fetch: Fetch,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Fetch {
    /// Sparse, shallow clone of `paths` from a git repository.
    Git { url: String, paths: Vec<String> },
    /// Not automated yet: download by hand into the source's directory.
    Manual { url: String },
}

#[derive(Debug, Clone)]
pub struct CorpusFile {
    /// Stable name used in reports and baselines: `<source>/<relative path>`.
    pub name: String,
    pub path: PathBuf,
}

fn manifest(root: &Path) -> Result<Vec<Source>> {
    let path = root.join("corpus/manifest.toml");
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let manifest: Manifest =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(manifest.source)
}

pub fn corpus_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("NUPDF_CORPUS_DIR") {
        return PathBuf::from(dir);
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".cache/nupdf/corpus")
}

/// Every PDF in a tier, sorted by name so results are deterministic.
pub fn files(root: &Path, tier: &str) -> Result<Vec<CorpusFile>> {
    let mut out = Vec::new();
    if tier == "smoke" {
        collect(&root.join("corpus/smoke"), "smoke", &mut out)?;
    } else {
        let sources: Vec<Source> = manifest(root)?
            .into_iter()
            .filter(|s| s.tier == tier)
            .collect();
        if sources.is_empty() {
            bail!("no sources for tier {tier} in corpus/manifest.toml");
        }
        for source in sources {
            let dir = corpus_dir().join(&source.name);
            if dir.exists() {
                collect(&dir, &source.name, &mut out)?;
            } else {
                eprintln!(
                    "skipping {}: not fetched (cargo xtask corpus fetch {})",
                    source.name, source.name
                );
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

fn collect(dir: &Path, prefix: &str, out: &mut Vec<CorpusFile>) -> Result<()> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in
            std::fs::read_dir(&current).with_context(|| format!("reading {}", current.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            if kind.is_dir() {
                if entry.file_name() != ".git" {
                    stack.push(path);
                }
            } else if kind.is_file()
                && path
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("pdf"))
            {
                let relative = path.strip_prefix(dir).unwrap_or(&path);
                out.push(CorpusFile {
                    name: format!("{prefix}/{}", relative.display()),
                    path,
                });
            }
        }
    }
    Ok(())
}

pub fn list(root: &Path) -> Result<()> {
    let smoke = files(root, "smoke")?.len();
    println!("smoke     {smoke:>6} files  committed in corpus/smoke");
    for source in manifest(root)? {
        let dir = corpus_dir().join(&source.name);
        let status = if dir.exists() {
            let mut found = Vec::new();
            collect(&dir, &source.name, &mut found)?;
            format!("{:>6} files", found.len())
        } else {
            "not fetched".to_string()
        };
        println!(
            "{:<9} {status}  [{}] {}",
            source.name, source.tier, source.about
        );
        println!("{:<9} license: {}", "", source.license);
    }
    println!("\ncorpus dir: {}", corpus_dir().display());
    Ok(())
}

pub fn fetch(root: &Path, name: &str) -> Result<()> {
    let source = manifest(root)?
        .into_iter()
        .find(|s| s.name == name)
        .with_context(|| format!("no source named {name} in corpus/manifest.toml"))?;
    let dir = corpus_dir().join(&source.name);
    match &source.fetch {
        Fetch::Manual { url } => bail!(
            "{name} is fetched by hand: download from {url} and unpack into {}",
            dir.display()
        ),
        Fetch::Git { url, paths } => {
            if dir.exists() {
                println!("{name} already fetched at {}", dir.display());
                return Ok(());
            }
            std::fs::create_dir_all(corpus_dir())?;
            git(Command::new("git")
                .args([
                    "clone",
                    "--quiet",
                    "--depth",
                    "1",
                    "--filter=blob:none",
                    "--sparse",
                    url,
                ])
                .arg(&dir))?;
            git(Command::new("git")
                .current_dir(&dir)
                .args(["sparse-checkout", "set", "--no-cone"])
                .args(paths))?;
            println!("fetched {name} into {}", dir.display());
            Ok(())
        }
    }
}

fn git(cmd: &mut Command) -> Result<()> {
    let status = cmd.status().with_context(|| format!("running {cmd:?}"))?;
    if !status.success() {
        bail!("{cmd:?} failed with {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_parses_and_names_are_unique() {
        let sources = manifest(&crate::repo_root()).unwrap();
        assert!(!sources.is_empty());
        let mut names: Vec<_> = sources.iter().map(|s| s.name.as_str()).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before);
        assert!(sources.iter().all(|s| s.name != "smoke"));
    }

    #[test]
    fn smoke_tier_lists_committed_fixtures() {
        let files = files(&crate::repo_root(), "smoke").unwrap();
        assert!(files.iter().any(|f| f.name == "smoke/minimal.pdf"));
        let mut sorted = files.clone();
        sorted.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(
            files.iter().map(|f| &f.name).collect::<Vec<_>>(),
            sorted.iter().map(|f| &f.name).collect::<Vec<_>>()
        );
    }
}
