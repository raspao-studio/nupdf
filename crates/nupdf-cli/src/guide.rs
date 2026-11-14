// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! `nupdf guide`: how to use nupdf, written for models, short enough to
//! read whole; and the Claude Code skill made of it (DECISIONS D45).

use std::fmt::Write as _;

use clap::CommandFactory;

use crate::Cli;

/// What the guide says before its list of commands.
const INTRO: &str = r#"# nupdf

PDF toolkit for agents. `nupdf <command> <file> [options]` prints one JSON report (`schema`, `doc_id`, fields, `diagnostics`); errors are JSON on stderr. Exit: 0 ok, 2 usage, 3 bad input, 4 verification failed, 5 limit. `nupdf <command> --help` lists options.

- Triage & Attest: `info <file>`; `attest <file> [--crops-dir dir]` checks pixel contrast vs stream text (occluded text, vector strikethroughs, `ActualText` spoofs, blind spots).
- Read & Query: `extract <file> --format markdown` (or `text`; `--options reading-order`; `--format layout` for rows; `--format json` for ids `p3.b2.l4`, boxes, fonts; `--max-chars 8000` + `--cursor`). Spatial & numeric sum/stats (file or dir): `query <path> --right-of "Total" --nearest-only [--summary-only]`.
- Find & Audit: `search <file> <needle> [--options ignore-case,regexp]`; `tables <file> [--format csv]`; `audit <file> --claims claims.json [--evidence-dir dir]` verifies quotes, ids and hazards with 0 tokens.
- Look: `render <file> -o dir --pages 2 --dpi 72`, only when text cannot answer. Navigate: `outline`, `links`, `labels`, `attachments`, `annots`, `forms`, `signatures`.
- Write (`-o out` or `--dry-run`): `patch <file> --replace p1.b2.l1.s1=NEW -o out` edits spans and checks collisions (exit 4); `apply <file> --plan a.json --plan b.json -o out` commits multi-agent plans on `doc_id` with conflict checks; `redact <file> -o out --text|--regex|--preset pii|--region|--id`; `verify`; `merge`; `split`; `pages`; `meta`; `clean`, `encrypt`, `decrypt`.
- Scanned/Untrusted: `ocr <file> -o out` (Tesseract); `sanitize <file> -o out` drops JS, actions, XFA.
- Annotate & Fill (`-o out`): `annots --add|--highlight|--delete`; `forms --set|--toggle`; `flatten`.
- Compare & Scale: `diff a.pdf b.pdf`; `batch <inputs> -- <command> [args]`. 1-based pages (`N` last); points from top-left; `--password`, `--timeout S`.

## Commands
"#;

/// What the MCP server tells a client at the start: the guide, read as
/// tools. A model gets it without asking, so it need not call anything to
/// learn how to start.
pub fn mcp_instructions() -> String {
    let mut out = String::from(
        "Over MCP each command below is a tool with the same options by name, and \
         `file` is a path on this machine. Give `file` a list of paths, directories or \
         globs to run a tool on all of them in one call: one line per file, each \
         with its report, then a summary.\n\n",
    );
    out.push_str(&guide());
    out
}

/// The guide: what to know, then every command by name.
pub fn guide() -> String {
    let mut out = INTRO.to_string();
    let cli = Cli::command();
    let names: Vec<String> = cli
        .get_subcommands()
        .filter(|c| c.get_name() != "help")
        .map(|c| format!("`{}`", c.get_name()))
        .collect();
    let _ = writeln!(out, "{}.", names.join(", "));
    out
}

/// One command's options in full, as its help gives them.
pub fn command(name: &str) -> Option<String> {
    let mut cli = Cli::command();
    let sub = cli.find_subcommand_mut(name)?;
    Some(sub.render_long_help().to_string())
}

/// The Claude Code skill: the guide, with what Claude Code reads first.
pub fn skill() -> String {
    format!(
        "---\nname: nupdf\ndescription: Read, search, extract, render, compare, edit and redact PDF files with the nupdf CLI. Use whenever a task involves a PDF: getting its text or tables, finding or removing information, checking what changed between versions, splitting or merging, or rendering pages.\n---\n\n{}",
        guide()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_guide_lists_every_command_and_stays_short() {
        let g = guide();
        for c in Cli::command()
            .get_subcommands()
            .filter(|c| c.get_name() != "help")
        {
            assert!(
                g.contains(&format!("`{}`", c.get_name())),
                "{}",
                c.get_name()
            );
        }
        // Read on every task: a few hundred tokens, not thousands.
        assert!(g.len() < 2500, "{}", g.len());
        // Layout text comes before rendering for fields read by position.
        assert!(g.find("--format layout") < g.find("render <file>"));
        assert!(command("extract").is_some_and(|h| h.contains("--max-chars")));
        assert!(command("nope").is_none());
    }

    #[test]
    fn the_skill_in_the_repository_is_the_one_nupdf_makes() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../skills/nupdf/SKILL.md");
        let committed = std::fs::read_to_string(path).unwrap_or_default();
        assert_eq!(
            committed,
            skill(),
            "run: nupdf guide --skill > skills/nupdf/SKILL.md"
        );
    }
}
