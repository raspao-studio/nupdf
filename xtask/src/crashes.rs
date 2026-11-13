// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! `cargo xtask crashes`: nupdf alone on every file of a tier, each command
//! in a process of its own, to find what makes it crash rather than what
//! makes it differ from MuPDF (that is the harness). A crash is a signal,
//! a panic or another internal error (exit 1), or a timeout; an input
//! error, a usage error or a failed check is a reading.
//!
//! Every run is a line in `target/crashes/<tier>.jsonl`, so a stopped run
//! takes up where it left off with `--resume`.

use std::collections::HashSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{Value, json};

/// The commands run on each file: `{file}` is the file, `{dir}` a scratch
/// directory of the worker's.
const COMMANDS: &[(&str, &[&str])] = &[
    ("info", &["info", "{file}"]),
    ("objects", &["objects", "{file}"]),
    ("extract", &["extract", "{file}", "--format", "text"]),
    ("search", &["search", "{file}", "the"]),
    (
        "render",
        &[
            "render", "{file}", "--pages", "1-3", "--dpi", "36", "-o", "{dir}",
        ],
    ),
    ("fonts", &["fonts", "{file}"]),
    ("images", &["images", "{file}"]),
    ("tables", &["tables", "{file}", "--pages", "1-3"]),
    ("annots", &["annots", "{file}"]),
    ("forms", &["forms", "{file}"]),
    ("signatures", &["signatures", "{file}"]),
    ("outline", &["outline", "{file}"]),
    ("clean", &["clean", "{file}", "--dry-run", "--no-verify"]),
    (
        "flatten",
        &["flatten", "{file}", "--dry-run", "--no-verify"],
    ),
    ("sanitize", &["sanitize", "{file}", "--dry-run"]),
];

/// How a run ended.
fn outcome(status: Option<std::process::ExitStatus>, timed_out: bool) -> &'static str {
    if timed_out {
        return "timeout";
    }
    match status {
        None => "crash",
        Some(s) => match s.code() {
            // 0 done; 2 usage; 3 input; 4 a failed check; 5 a limit.
            Some(0 | 2..=5) => "ok",
            Some(1) => "internal",
            Some(_) | None => "crash",
        },
    }
}

/// Runs `nupdf` with `args`, killing it after `timeout`; its stderr's last
/// line comes back for the record.
fn run(
    nupdf: &Path,
    args: &[String],
    timeout: Duration,
) -> (&'static str, Option<i32>, String, f64) {
    let start = Instant::now();
    let mut child = match Command::new(nupdf)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return ("crash", None, format!("cannot start: {e}"), 0.0),
    };
    let mut stderr = child.stderr.take();
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(e) = stderr.as_mut() {
            use std::io::Read as _;
            let _ = e.take(1 << 20).read_to_string(&mut s);
        }
        s
    });
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if start.elapsed() > timeout => {
                let _ = child.kill();
                timed_out = true;
                break child.wait().ok();
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(_) => break None,
        }
    };
    let err = reader.join().unwrap_or_default();
    let last = err
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .chars()
        .take(400)
        .collect();
    let code = status.and_then(|s| s.code());
    (
        outcome(status, timed_out),
        code,
        last,
        start.elapsed().as_secs_f64(),
    )
}

pub struct Options<'a> {
    pub tier: &'a str,
    pub sample: Option<usize>,
    pub jobs: usize,
    pub timeout: Duration,
    pub resume: bool,
    pub only: Option<&'a str>,
}

pub fn run_all(root: &Path, nupdf: &Path, opts: &Options) -> Result<()> {
    let mut files = crate::corpus::files(root, opts.tier)?;
    if let Some(n) = opts.sample.filter(|&n| n > 0 && n < files.len()) {
        let step = files.len() as f64 / n as f64;
        files = (0..n)
            .map(|i| files[(i as f64 * step) as usize].clone())
            .collect();
    }
    let commands: Vec<(&str, &[&str])> = COMMANDS
        .iter()
        .filter(|(name, _)| opts.only.is_none_or(|o| o.split(',').any(|x| x == *name)))
        .copied()
        .collect();
    let out_dir = root.join("target/crashes");
    std::fs::create_dir_all(&out_dir)?;
    let log_path = out_dir.join(format!("{}.jsonl", opts.tier));
    let mut done: HashSet<(String, String)> = HashSet::new();
    if opts.resume && log_path.exists() {
        for line in std::fs::read_to_string(&log_path)?.lines() {
            if let Ok(v) = serde_json::from_str::<Value>(line)
                && let (Some(f), Some(c)) = (v["file"].as_str(), v["command"].as_str())
            {
                done.insert((f.to_string(), c.to_string()));
            }
        }
    }
    let log = Mutex::new(
        std::fs::OpenOptions::new()
            .create(true)
            .append(opts.resume)
            .write(true)
            .truncate(!opts.resume)
            .open(&log_path)
            .with_context(|| format!("opening {}", log_path.display()))?,
    );
    let jobs: Vec<(usize, usize)> = (0..files.len())
        .flat_map(|f| (0..commands.len()).map(move |c| (f, c)))
        .filter(|&(f, c)| !done.contains(&(files[f].name.clone(), commands[c].0.to_string())))
        .collect();
    let next = AtomicUsize::new(0);
    let finished = AtomicUsize::new(0);
    let started = Instant::now();
    let total = jobs.len();
    eprintln!(
        "{} files, {} commands, {total} runs to do",
        files.len(),
        commands.len()
    );
    std::thread::scope(|s| {
        for worker in 0..opts.jobs.max(1) {
            let (jobs, files, commands, next, finished, log) =
                (&jobs, &files, &commands, &next, &finished, &log);
            s.spawn(move || {
                let scratch: PathBuf = std::env::temp_dir()
                    .join(format!("nupdf-crashes-{}-{worker}", std::process::id()));
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(&(f, c)) = jobs.get(i) else { break };
                    let file = &files[f];
                    let (name, template) = commands[c];
                    let _ = std::fs::remove_dir_all(&scratch);
                    let _ = std::fs::create_dir_all(&scratch);
                    let args: Vec<String> = template
                        .iter()
                        .map(|a| match *a {
                            "{file}" => file.path.to_string_lossy().into_owned(),
                            "{dir}" => scratch.to_string_lossy().into_owned(),
                            a => a.to_string(),
                        })
                        .collect();
                    let (outcome, code, stderr, secs) = run(nupdf, &args, opts.timeout);
                    let line = json!({
                        "file": file.name, "command": name, "outcome": outcome,
                        "exit": code, "seconds": (secs * 1000.0).round() / 1000.0,
                        "stderr": if outcome == "ok" { Value::Null } else { stderr.into() },
                    });
                    if let Ok(mut l) = log.lock() {
                        let _ = writeln!(l, "{line}");
                    }
                    let n = finished.fetch_add(1, Ordering::Relaxed) + 1;
                    if n % 5000 == 0 {
                        let rate = n as f64 / started.elapsed().as_secs_f64().max(1.0);
                        eprintln!(
                            "{n}/{total} runs, {:.0}/s, about {:.0} min left",
                            rate,
                            (total - n) as f64 / rate / 60.0
                        );
                    }
                }
                let _ = std::fs::remove_dir_all(&scratch);
            });
        }
    });
    summarize(
        &log_path,
        &out_dir.join(format!("{}-summary.json", opts.tier)),
    )
}

/// Counts by command and outcome, and every run that was not ok.
pub fn summarize(log_path: &Path, out: &Path) -> Result<()> {
    let mut by: std::collections::BTreeMap<String, std::collections::BTreeMap<String, usize>> =
        Default::default();
    let mut bad = Vec::new();
    let mut files = HashSet::new();
    for line in std::fs::read_to_string(log_path)?.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let cmd = v["command"].as_str().unwrap_or("?").to_string();
        let outcome = v["outcome"].as_str().unwrap_or("?").to_string();
        files.insert(v["file"].as_str().unwrap_or("").to_string());
        *by.entry(cmd)
            .or_default()
            .entry(outcome.clone())
            .or_default() += 1;
        if outcome != "ok" {
            bad.push(v);
        }
    }
    let summary =
        json!({ "files": files.len(), "by_command": by, "not_ok": bad.len(), "runs": bad });
    std::fs::write(out, serde_json::to_string_pretty(&summary)?)?;
    println!("{} files", files.len());
    for (cmd, outcomes) in &by {
        let parts: Vec<String> = outcomes.iter().map(|(o, n)| format!("{o} {n}")).collect();
        println!("{cmd:12} {}", parts.join(", "));
    }
    println!("not ok: {} (in {})", bad.len(), out.display());
    Ok(())
}
