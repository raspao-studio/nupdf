// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! `nupdf batch`: one command on many files, several at once, a JSON line
//! per file in the order the files were given, and a run that can be taken
//! up again where it stopped (DECISIONS D43).

use std::collections::{BTreeMap, HashSet};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant, UNIX_EPOCH};

use clap::CommandFactory;
use serde_json::{Value, json};

use crate::{Cli, Exit, ExitCode, Failure};

/// What `batch` was asked for.
pub struct Args<'a> {
    pub inputs: &'a [String],
    pub jobs: usize,
    pub output: Option<&'a Path>,
    pub resume: bool,
    pub item_timeout: Option<f64>,
    pub command: &'a [String],
}

/// Commands `batch` does not run: they take no single file, or run others.
const NOT_BATCHED: &[&str] = &["batch", "mcp", "merge", "help"];

fn usage(message: String) -> Failure {
    Failure {
        exit: Exit::Usage,
        code: "usage.bad_option",
        message,
    }
}

/// The files `inputs` name: a file as it is, a directory's PDFs at any
/// depth, a glob's matches; each once, in name order within an input and
/// the inputs in the order given.
pub fn files(inputs: &[String]) -> Result<Vec<PathBuf>, Failure> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for input in inputs {
        let path = Path::new(input);
        let mut found = if path.is_file() {
            vec![path.to_path_buf()]
        } else if path.is_dir() {
            let pattern = path.join("**").join("*.pdf");
            matches(&pattern.to_string_lossy(), true)?
        } else {
            let found = matches(input, false)?;
            if found.is_empty() {
                return Err(usage(format!("no files match {input}")));
            }
            found
        };
        found.sort();
        for f in found {
            if seen.insert(f.clone()) {
                out.push(f);
            }
        }
    }
    Ok(out)
}

fn matches(pattern: &str, any_case: bool) -> Result<Vec<PathBuf>, Failure> {
    let options = glob::MatchOptions {
        case_sensitive: !any_case,
        ..glob::MatchOptions::new()
    };
    let paths = glob::glob_with(pattern, options)
        .map_err(|e| usage(format!("bad pattern {pattern}: {e}")))?;
    Ok(paths
        .filter_map(Result::ok)
        .filter(|p| p.is_file())
        .collect())
}

/// The command line for one file: the command, the file, the rest of the
/// arguments with `{stem}`, `{name}` and `{index}` filled in.
fn line_for(command: &[String], file: &Path, n: usize, width: usize) -> Vec<String> {
    let stem = file
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = file
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let number = format!("{n:0width$}");
    let mut line = vec!["nupdf".to_string()];
    let mut words = command.iter();
    if let Some(c) = words.next() {
        line.push(c.clone());
    }
    line.push(file.to_string_lossy().into_owned());
    line.extend(words.map(|w| {
        w.replace("{stem}", &stem)
            .replace("{name}", &name)
            .replace("{index}", &number)
    }));
    line
}

/// Checks that `command` names a command that takes one file first.
fn check_command(command: &[String]) -> Result<(), Failure> {
    let name = command
        .first()
        .ok_or_else(|| usage("give the command after --".into()))?;
    let cli = Cli::command();
    let sub = cli
        .get_subcommands()
        .find(|c| c.get_name() == name)
        .ok_or_else(|| usage(format!("no command named {name}")))?;
    let first = sub.get_positionals().next().map(|a| a.get_id().as_str());
    if NOT_BATCHED.contains(&name.as_str()) || first != Some("file") {
        return Err(usage(format!("{name} cannot be run on each file")));
    }
    Ok(())
}

/// A file as it was when it ran: its size and when it changed.
fn stamp(path: &Path) -> (u64, String) {
    let meta = std::fs::metadata(path).ok();
    let size = meta.as_ref().map_or(0, std::fs::Metadata::len);
    let modified = meta
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or_else(String::new, |d| {
            format!("{}.{:09}", d.as_secs(), d.subsec_nanos())
        });
    (size, modified)
}

/// The files an earlier run's output reports, as they were then.
fn done_before(output: &Path) -> HashSet<(String, u64, String)> {
    let Ok(file) = std::fs::File::open(output) else {
        return HashSet::new();
    };
    std::io::BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter_map(|l| serde_json::from_str::<Value>(&l).ok())
        .filter(|v| v["schema"] == "nupdf.batch.item/v0")
        .filter_map(|v| {
            Some((
                v["file"].as_str()?.to_string(),
                v["size"].as_u64()?,
                v["modified"].as_str()?.to_string(),
            ))
        })
        .collect()
}

/// One file's line.
fn item(
    file: &Path,
    (size, modified): (u64, String),
    ran: Result<crate::Ran, Failure>,
    timed_out: bool,
) -> Value {
    let mut line = json!({
        "schema": "nupdf.batch.item/v0",
        "file": file.to_string_lossy(),
        "size": size,
        "modified": modified,
    });
    let parse = |b: &[u8]| match serde_json::from_slice::<Value>(b) {
        Ok(v @ Value::Object(_)) => Some(v),
        _ => None,
    };
    let failure = |f: &Failure| json!({"schema": "nupdf.error/v0", "error": {"code": f.code, "message": f.message}});
    match ran {
        _ if timed_out => {
            let f = Failure {
                message: "the file ran out of time (--item-timeout)".into(),
                ..crate::timed_out()
            };
            line["exit"] = (f.exit as u8).into();
            line["error"] = failure(&f);
        }
        Err(f) => {
            line["exit"] = (f.exit as u8).into();
            line["error"] = failure(&f);
        }
        Ok(ran) if ran.code == ExitCode::SUCCESS => {
            line["exit"] = 0.into();
            match parse(&ran.out) {
                Some(report) => line["report"] = report,
                // Text and Markdown: the text, and the report from stderr.
                None => {
                    line["text"] = String::from_utf8_lossy(&ran.out).into_owned().into();
                    line["report"] = parse(&ran.err).unwrap_or(Value::Null);
                }
            }
        }
        Ok(ran) => {
            line["exit"] = ran.code.0.into();
            line["error"] = parse(&ran.err).unwrap_or_else(|| {
                failure(&Failure {
                    exit: Exit::Internal,
                    code: "internal.no_report",
                    message: String::from_utf8_lossy(&ran.err).into_owned(),
                })
            });
        }
    }
    line
}

/// Runs the batch; the summary report, or why it could not run.
pub fn run(args: &Args) -> Result<Value, Failure> {
    check_command(args.command)?;
    if args.resume && args.output.is_none() {
        return Err(usage("--resume needs --output".into()));
    }
    let files = files(args.inputs)?;
    let done = match (args.resume, args.output) {
        (true, Some(o)) => done_before(o),
        _ => HashSet::new(),
    };
    let todo: Vec<(usize, PathBuf, (u64, String))> = files
        .iter()
        .enumerate()
        .map(|(i, f)| (i, f.clone(), stamp(f)))
        .filter(|(_, f, (size, modified))| {
            !done.contains(&(f.to_string_lossy().into_owned(), *size, modified.clone()))
        })
        .collect();
    let skipped = files.len() - todo.len();
    let mut sink: Box<dyn Write> = match args.output {
        Some(path) => {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(args.resume)
                .write(true)
                .truncate(!args.resume)
                .open(path)
                .map_err(|e| Failure {
                    exit: Exit::Usage,
                    code: "usage.bad_output",
                    message: format!("cannot write {}: {e}", path.display()),
                })?;
            // A run cut off mid-line leaves a line to end before adding.
            if args.resume
                && std::fs::read(path).is_ok_and(|d| d.last().is_some_and(|&b| b != b'\n'))
            {
                let _ = file.write_all(b"\n");
            }
            Box::new(file)
        }
        None => crate::standard_output(),
    };
    let jobs = match args.jobs {
        0 => std::thread::available_parallelism().map_or(1, usize::from),
        n => n,
    }
    .clamp(1, todo.len().max(1));
    let width = files.len().to_string().len();
    let next = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel::<(usize, Value)>();
    let (mut ok, mut failed) = (0usize, 0usize);
    let whole = nupdf::interrupt::current();
    std::thread::scope(|s| {
        for _ in 0..jobs {
            let tx = tx.clone();
            let (todo, next, whole) = (&todo, &next, &whole);
            s.spawn(move || {
                loop {
                    let k = next.fetch_add(1, Ordering::Relaxed);
                    let Some((i, file, stamp)) = todo.get(k) else {
                        break;
                    };
                    // The batch itself ran out of time: start no more.
                    if whole.as_ref().is_some_and(|w| w.reason().is_some()) {
                        break;
                    }
                    let deadline = args.item_timeout.and_then(|t| {
                        Instant::now().checked_add(Duration::try_from_secs_f64(t.max(0.0)).ok()?)
                    });
                    let interrupt = nupdf::interrupt::Interrupt::new(deadline);
                    let line = line_for(args.command, file, i + 1, width);
                    let ran = crate::run_line(&line, Some(interrupt.clone()));
                    let timed_out = interrupt.reason().is_some();
                    if tx
                        .send((k, item(file, stamp.clone(), ran, timed_out)))
                        .is_err()
                    {
                        break;
                    }
                }
            });
        }
        drop(tx);
        // Lines go out in the order the files were given, each as soon as
        // those before it are out.
        let mut waiting = BTreeMap::new();
        let mut written = 0;
        for (k, line) in rx {
            waiting.insert(k, line);
            while let Some(line) = waiting.remove(&written) {
                if line["exit"] == 0 {
                    ok += 1;
                } else {
                    failed += 1;
                }
                let _ = writeln!(sink, "{line}");
                let _ = sink.flush();
                written += 1;
            }
        }
    });
    Ok(json!({
        "files": files.len(),
        "ok": ok,
        "failed": failed,
        "skipped": skipped,
        "not_run": todo.len() - ok - failed,
        "command": args.command,
        "output": args.output.map(|p| p.to_string_lossy().into_owned()),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_file_gets_its_line() {
        let command: Vec<String> = ["clean", "-g", "-o", "out/{stem}-{index}.pdf"]
            .map(String::from)
            .into();
        assert_eq!(
            line_for(&command, Path::new("in/a b.pdf"), 7, 3),
            [
                "nupdf",
                "clean",
                "in/a b.pdf",
                "-g",
                "-o",
                "out/a b-007.pdf"
            ]
        );
    }

    #[test]
    fn commands_need_a_file_first() {
        let words = |w: &[&str]| -> Vec<String> { w.iter().map(|s| s.to_string()).collect() };
        assert!(check_command(&words(&["extract", "--max-chars", "9"])).is_ok());
        assert!(check_command(&words(&["search", "needle"])).is_ok());
        for bad in [&["merge"][..], &["mcp"], &["batch"], &["nope"], &[]] {
            assert!(check_command(&words(bad)).is_err(), "{bad:?}");
        }
    }
}
