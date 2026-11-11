// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! `cargo xtask reading-order`: the reading order of nupdf's text, by
//! default (MuPDF's) and with the `reading-order` option, scored against
//! the hand-labeled pages in `corpus/reading-order/truth.json`. A page's
//! score is the share of pairs of its labeled fragments that come in the
//! labeled order.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::{corpus, proc};

#[derive(Debug, Deserialize)]
struct Truth {
    pages: Vec<LabeledPage>,
}

#[derive(Debug, Deserialize)]
struct LabeledPage {
    file: String,
    page: usize,
    fragments: Vec<String>,
}

/// The share of pairs of `fragments` found in `text` in their order; a
/// fragment not found counts against every pair it is in.
pub fn score(text: &str, fragments: &[String]) -> f64 {
    let text = collapse(text);
    let pos: Vec<Option<usize>> = fragments.iter().map(|f| text.find(f.as_str())).collect();
    let n = pos.len();
    if n < 2 {
        return 1.0;
    }
    let mut ok = 0usize;
    for i in 0..n {
        for j in i + 1..n {
            if let (Some(a), Some(b)) = (pos[i], pos[j])
                && a < b
            {
                ok += 1;
            }
        }
    }
    ok as f64 / (n * (n - 1) / 2) as f64
}

/// Whitespace runs as one space.
fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn run(root: &Path) -> Result<()> {
    let nupdf = crate::build_nupdf(root)?;
    let path = root.join("corpus/reading-order/truth.json");
    let truth: Truth = serde_json::from_slice(&std::fs::read(&path)?)
        .with_context(|| format!("reading {}", path.display()))?;
    let dir = corpus::corpus_dir();
    let text = |file: &Path, page: usize, options: Option<&str>| -> Result<String> {
        let mut args = vec![
            "extract".to_string(),
            file.to_string_lossy().into_owned(),
            "--pages".into(),
            page.to_string(),
            "--format".into(),
            "text".into(),
        ];
        if let Some(o) = options {
            args.extend(["--options".into(), o.into()]);
        }
        let run = proc::run(&nupdf, &args, Duration::from_secs(60))?;
        Ok(String::from_utf8_lossy(&run.stdout).into_owned())
    };
    println!("| Page | MuPDF's order | reading-order |\n|---|---|---|");
    let (mut base_sum, mut mode_sum) = (0.0, 0.0);
    let mut stale = Vec::new();
    for p in &truth.pages {
        let file = dir.join(&p.file);
        if !file.exists() {
            bail!("{} is not in the corpus; fetch it first", p.file);
        }
        let base = text(&file, p.page, None)?;
        let collapsed = collapse(&base);
        for f in &p.fragments {
            if collapsed.matches(f.as_str()).count() != 1 {
                stale.push(format!("{} page {}: {f:?}", p.file, p.page));
            }
        }
        let mode = text(&file, p.page, Some("reading-order"))?;
        let (b, m) = (score(&base, &p.fragments), score(&mode, &p.fragments));
        base_sum += b;
        mode_sum += m;
        println!("| {} p{} | {b:.3} | {m:.3} |", p.file, p.page);
    }
    let n = truth.pages.len().max(1) as f64;
    println!("| mean | {:.3} | {:.3} |", base_sum / n, mode_sum / n);
    if !stale.is_empty() {
        bail!(
            "fragments that do not occur exactly once:\n{}",
            stale.join("\n")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::score;

    #[test]
    fn scores_count_pairs_in_order() {
        let f = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(score("a b  c\nd", &f(&["a", "b", "c d"])), 1.0);
        assert_eq!(score("c b a", &f(&["a", "b", "c"])), 0.0);
        // One of three pairs out of order.
        assert!((score("b a c", &f(&["a", "b", "c"])) - 2.0 / 3.0).abs() < 1e-9);
        // A fragment not found loses its pairs.
        assert!((score("a c", &f(&["a", "b", "c"])) - 1.0 / 3.0).abs() < 1e-9);
    }
}
