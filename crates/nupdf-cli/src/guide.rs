// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! `nupdf guide`: how to use nupdf, written for models, short enough to
//! read whole; and the Claude Code skill made of it (DECISIONS D45).

use std::fmt::Write as _;

use clap::CommandFactory;

use crate::Cli;

/// What the guide says before its list of commands.
const INTRO: &str = r#"# nupdf

PDF toolkit for agents. `nupdf <command> <file> [options]` prints one JSON report (`schema`, `doc_id`, fields, `diagnostics`); errors are JSON on stderr. Exit: 0 ok, 2 usage, 3 bad input, 4 verification failed, 5 limit. `nupdf <command> --help` lists a command's options.

- Triage: `info <file>`.
- Read: `extract <file> --format markdown` (or `text`); add `--options reading-order` when text comes out of order (columns, forms). `--format json` gives ids (`p3.b2.l4`), boxes and fonts. Long files: `--max-chars 8000`, then `--cursor <next_cursor>`; `--pages 3-5`.
- Find: `search <file> <needle> [--options ignore-case,regexp]`: page, quads, text, ids. Tables: `tables <file> [--format csv]`.
- Look: `render <file> -o dir --pages 2 --dpi 72`. Find your way: `outline` (bookmarks, with pages), `links`, `labels` (page labels), `attachments [--extract dir]`.
- Write (input never changed; `-o out` or `--dry-run`): `redact <file> -o out --text|--regex|--preset pii|--region|--id` removes it everywhere and re-checks, exit 4 if anything is left; `verify <file> --text`; `merge a.pdf b.pdf 1-3 -o out`; `split <file> -o dir --ranges "1-3;4-N"`; `pages --select|--delete|--rotate`; `meta --set Title=...`; `clean`, `encrypt`, `decrypt`.
- Compare: `diff a.pdf b.pdf`. Many files: `batch <files|dirs|globs> -- <command> [args]`.
- Pages are 1-based (`N` is the last); coordinates are points, origin top left. `--password` for encrypted files, `--timeout S` on anything.

## Commands
"#;

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
