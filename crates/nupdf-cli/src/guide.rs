// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! `nupdf guide`: how to use nupdf, written for models, short enough to
//! read whole; and the Claude Code skill made of it (DECISIONS D45).

use std::fmt::Write as _;

use clap::CommandFactory;

use crate::Cli;

/// What the guide says before its list of commands.
const INTRO: &str = r#"# nupdf

A PDF toolkit for agents. `nupdf <command> <file> [options]` prints one JSON report on stdout: `schema` (`nupdf.<command>/v1`), `doc_id`, the command's fields, `diagnostics` (warnings as data: `code`, `severity`, `message`). Errors are JSON on stderr: `{"schema":"nupdf.error/v1","error":{"code","message"}}`. Exit codes: 0 ok, 1 bug, 2 usage, 3 bad input (unreadable, not a PDF, wrong password), 4 verification failed, 5 limit (`limit.timeout`, `limit.*`). `nupdf schema <command>` prints a report's JSON Schema; `nupdf guide <command>` a command's options.

## Start here

- `info <file>`: version, page count, encryption, repairs, revisions. Do this first.
- `extract <file>`: the text with content ids. `--format markdown` or `--format text` for reading; `--format json` (default) for ids, boxes, fonts. Budget it: `--max-chars 8000` stops at a whole block with `"truncated": true` and `next_cursor`; pass that to `--cursor` to go on. `--pages 3-5`, `--fields page,text` for less.
- `search <file> <needle>`: hits with page, quads, text and content ids. `--options ignore-case,ignore-diacritics,regexp`.

## Addressing

- Pages are 1-based. Ranges: `1-3,7`, `N` (last), `5-N`, `N-1` (reversed).
- Content ids: `p3` page, `p3.b2` block, `p3.b2.l4` line, `p3.b2.l4.s1` span, `p3.b2.l4.s1:5-12` characters. Stable for a `doc_id`; a cursor or id from another document is refused.
- Coordinates are points, origin top left, y down, rotation applied: what `render` draws.

## Changing documents

Writers never touch the input: `-o <out>` writes atomically, `--dry-run` reports the plan and writes nothing. Output is read back and checked before it is kept.

- `redact <file> -o <out>` with `--text`, `--regex`, `--preset pii|email|phone|card|ssn|iban`, `--region p3:[x0,y0,x1,y1]`, `--id`, or `--hits` (a `search` report). It removes the content, scrubs metadata, annotations, outline and the rest, rewrites the file whole, then searches it again: a target still found exits 4 and writes nothing. `--dry-run --preview DIR` renders what would go. `verify <file> --text ...` checks a file by itself.
- `clean`, `merge a.pdf b.pdf 1-3 -o out`, `split -o dir`, `pages --select|--delete|--rotate`, `meta --set Title=...`, `encrypt`, `decrypt`.

## More

- `render <file> -o dir [--dpi 72] [--pages]`: page-NNNN.png files.
- `diff <a> <b> [--by text,structure,pixels]`: what changed, lines with ids on both sides.
- `batch <files|dirs|globs> -- <command> [args]`: a JSON line per file; `-o run.jsonl --resume` takes a stopped run up again; `{stem}`, `{name}`, `{index}` name outputs.
- `fonts`, `images` (`--extract dir`), `stext` (MuPDF's structured text), `trace`, `operators`, `objects`, `show <ref>`, `streams`: lower level.
- `--threads N` on `stext`, `extract`, `search`, `render`; `--timeout SECONDS` on anything; `--password` for encrypted files.
- `nupdf mcp` serves every command as an MCP tool, arguments named as the options (`max_chars`).

## Commands
"#;

/// The guide: what to know, then each command and its options.
pub fn guide() -> String {
    let mut out = INTRO.to_string();
    let cli = Cli::command();
    for c in cli.get_subcommands().filter(|c| c.get_name() != "help") {
        let about = c.get_about().map(ToString::to_string).unwrap_or_default();
        let first = about
            .split(". ")
            .next()
            .unwrap_or(&about)
            .trim_end_matches('.');
        let positional: Vec<String> = c
            .get_positionals()
            .map(|a| format!("<{}>", a.get_id()))
            .collect();
        let options: Vec<String> = c
            .get_arguments()
            .filter(|a| !a.is_positional() && a.get_id() != "help")
            .filter_map(|a| match (a.get_long(), a.get_short()) {
                (Some(l), _) => Some(format!("--{l}")),
                (None, Some(s)) => Some(format!("-{s}")),
                _ => None,
            })
            .collect();
        let usage = std::iter::once(c.get_name().to_string())
            .chain(positional)
            .collect::<Vec<_>>()
            .join(" ");
        let _ = write!(out, "- `{usage}`: {first}.");
        if !options.is_empty() {
            let _ = write!(out, " Options: {}.", options.join(", "));
        }
        out.push('\n');
    }
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
                g.contains(&format!("- `{}", c.get_name())),
                "{}",
                c.get_name()
            );
        }
        // Read whole by a model: about two thousand tokens at most.
        assert!(g.len() < 9000, "{}", g.len());
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
