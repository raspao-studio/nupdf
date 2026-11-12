// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! `nupdf mcp`: the commands as tools of a Model Context Protocol server,
//! JSON-RPC over stdin and stdout (DECISIONS D42).
//!
//! Each tool is a command, its arguments the command's options, read from
//! the CLI's own definition, and each call runs the command as the CLI
//! does, on a thread of its own, with what it writes kept: its report is
//! the CLI's, byte for byte. A call that runs out of time or is cancelled
//! is stopped where it next checks, and what it made is dropped. Files are
//! read and hashed once while they stay the same.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use clap::{ArgAction, CommandFactory};
use nupdf::interrupt::{Interrupt, Reason};
use serde_json::{Map, Value, json};

use crate::{Cli, ExitCode, Failure, FileFacts};

/// The protocol revisions the server speaks, newest first.
const PROTOCOLS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// What the server tells a client about itself when it connects.
const INSTRUCTIONS: &str = "nupdf reads, searches, renders, edits and redacts PDF files. \
Each tool is a nupdf command and returns its JSON report; `file` is a path on this machine. \
Start with `info`. Use `extract` with `max_chars` or `max_items` and resume with `cursor` \
rather than reading whole documents. Content ids such as p3.b2.l1 are stable for a doc_id.";

/// Arguments the server reads itself rather than passing to the command.
const TIMEOUT: &str = "timeout";

/// Global options a tool does not take: output is always compact JSON.
const NOT_TOOL_OPTIONS: &[&str] = &["pretty", "help", "version", TIMEOUT];

/// Commands that are not tools.
const NOT_TOOLS: &[&str] = &["mcp", "batch", "guide", "help"];

// ---- files ---------------------------------------------------------------

/// Whether files are kept between calls: only while serving.
static SERVING: AtomicBool = AtomicBool::new(false);

/// Most bytes of files kept at once.
const KEPT_BYTES: usize = 1 << 30;

/// A file as it was when read: where it is, its size and when it changed.
type FileKey = (PathBuf, u64, Option<SystemTime>);

#[derive(Default)]
struct Files {
    kept: HashMap<FileKey, (Arc<[u8]>, FileFacts, u64)>,
    bytes: usize,
    clock: u64,
}

static FILES: Mutex<Option<Files>> = Mutex::new(None);

fn file_key(path: &Path) -> Option<FileKey> {
    let full = std::fs::canonicalize(path).ok()?;
    let meta = std::fs::metadata(&full).ok()?;
    Some((full, meta.len(), meta.modified().ok()))
}

/// `path`'s bytes and facts from an earlier call, or read now and kept,
/// while serving; None otherwise, or if the file cannot be read.
pub(crate) fn cached_file(path: &Path) -> Option<(Arc<[u8]>, FileFacts)> {
    if !SERVING.load(Ordering::Relaxed) {
        return None;
    }
    let key = file_key(path)?;
    {
        let mut files = FILES.lock().unwrap_or_else(PoisonError::into_inner);
        let files = files.get_or_insert_with(Files::default);
        files.clock += 1;
        let clock = files.clock;
        if let Some((data, facts, used)) = files.kept.get_mut(&key) {
            *used = clock;
            return Some((data.clone(), facts.clone()));
        }
    }
    let (data, facts) = crate::read_file(path).ok()?;
    if data.len() <= KEPT_BYTES {
        let mut files = FILES.lock().unwrap_or_else(PoisonError::into_inner);
        let files = files.get_or_insert_with(Files::default);
        while files.bytes + data.len() > KEPT_BYTES {
            let Some(oldest) = files
                .kept
                .iter()
                .min_by_key(|(_, (_, _, used))| *used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some((d, _, _)) = files.kept.remove(&oldest) {
                files.bytes -= d.len();
            }
        }
        files.clock += 1;
        let clock = files.clock;
        if files
            .kept
            .insert(key, (data.clone(), facts.clone(), clock))
            .is_none()
        {
            files.bytes += data.len();
        }
    }
    Some((data, facts))
}

// ---- tools ---------------------------------------------------------------

/// A JSON Schema for one option.
fn property(arg: &clap::Arg) -> Value {
    let mut help = arg.get_help().map(ToString::to_string).unwrap_or_default();
    let defaults: Vec<String> = arg
        .get_default_values()
        .iter()
        .map(|v| v.to_string_lossy().into_owned())
        .collect();
    let many = arg.get_num_args().is_some_and(|n| n.max_values() > 1)
        || matches!(arg.get_action(), ArgAction::Append);
    let mut schema = match arg.get_action() {
        ArgAction::SetTrue => json!({"type": "boolean"}),
        ArgAction::Count => json!({"type": "integer", "minimum": 0}),
        _ => {
            let values: Vec<String> = arg
                .get_possible_values()
                .iter()
                .map(|v| v.get_name().to_string())
                .collect();
            let one = if values.is_empty() {
                json!({"type": ["string", "number"]})
            } else {
                json!({"type": "string", "enum": values})
            };
            if many {
                json!({"type": "array", "items": one})
            } else {
                one
            }
        }
    };
    if !defaults.is_empty() && !matches!(arg.get_action(), ArgAction::SetTrue) {
        if !help.is_empty() {
            help.push(' ');
        }
        help.push_str(&format!("Default: {}.", defaults.join(", ")));
    }
    if !help.is_empty() {
        schema["description"] = help.into();
    }
    schema
}

fn is_tool_option(arg: &clap::Arg) -> bool {
    !NOT_TOOL_OPTIONS.contains(&arg.get_id().as_str())
}

/// Each command as a tool: its name, what it does and its options.
fn tools() -> Vec<Value> {
    let cli = Cli::command();
    let globals: Vec<&clap::Arg> = cli.get_arguments().filter(|a| is_tool_option(a)).collect();
    cli.get_subcommands()
        .filter(|c| !NOT_TOOLS.contains(&c.get_name()))
        .map(|c| {
            let mut properties = Map::new();
            let mut required = Vec::new();
            for arg in c.get_arguments().filter(|a| is_tool_option(a)) {
                properties.insert(arg.get_id().to_string(), property(arg));
                if arg.is_required_set() {
                    required.push(Value::from(arg.get_id().as_str()));
                }
            }
            for arg in &globals {
                properties.insert(arg.get_id().to_string(), property(arg));
            }
            properties.insert(
                TIMEOUT.into(),
                json!({"type": "number", "minimum": 0,
                       "description": "Seconds the call may run before it fails with limit.timeout."}),
            );
            let about = c
                .get_long_about()
                .or_else(|| c.get_about())
                .map(ToString::to_string)
                .unwrap_or_default();
            json!({
                "name": c.get_name(),
                "description": about,
                "inputSchema": {
                    "type": "object",
                    "properties": properties,
                    "required": required,
                    "additionalProperties": false,
                },
            })
        })
        .collect()
}

/// An argument's value as the command line gives it.
fn word(v: &Value) -> Result<String, String> {
    match v {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Bool(b) => Ok(b.to_string()),
        _ => Err(format!("expected a string or a number, not {v}")),
    }
}

/// The command line for a call of `tool` with `args`.
fn command_line(tool: &str, args: &Map<String, Value>) -> Result<Vec<String>, String> {
    let cli = Cli::command();
    let sub = cli
        .get_subcommands()
        .find(|c| c.get_name() == tool && !NOT_TOOLS.contains(&tool))
        .ok_or_else(|| format!("no tool named {tool}"))?;
    let mut line = vec!["nupdf".to_string(), tool.to_string()];
    let mut positional: Vec<(usize, Vec<String>)> = Vec::new();
    if let Some(key) = args.keys().find(|k| {
        *k != TIMEOUT
            && !sub
                .get_arguments()
                .chain(cli.get_arguments())
                .any(|a| a.get_id().as_str() == *k && is_tool_option(a))
    }) {
        return Err(format!("{tool} takes no argument {key}"));
    }
    // In the order the command defines them, so a call's line is the same
    // whatever order its arguments come in.
    for arg in sub
        .get_arguments()
        .chain(cli.get_arguments())
        .filter(|a| is_tool_option(a))
    {
        let key = arg.get_id().as_str();
        let Some(value) = args.get(key).filter(|v| !v.is_null()) else {
            continue;
        };
        let values = match value {
            Value::Array(items) => items.iter().map(word).collect::<Result<Vec<_>, _>>()?,
            v => vec![word(v)?],
        };
        if arg.is_positional() {
            positional.push((arg.get_index().unwrap_or(usize::MAX), values));
            continue;
        }
        let flag = match (arg.get_long(), arg.get_short()) {
            (Some(long), _) => format!("--{long}"),
            (None, Some(short)) => format!("-{short}"),
            (None, None) => return Err(format!("{key} cannot be given")),
        };
        match arg.get_action() {
            ArgAction::SetTrue => match value {
                Value::Bool(true) => line.push(flag),
                Value::Bool(false) => {}
                _ => return Err(format!("{key} is true or false")),
            },
            ArgAction::Count => {
                let n = value
                    .as_u64()
                    .filter(|&n| n <= 16)
                    .ok_or_else(|| format!("{key} is a count from 0 to 16"))?;
                line.extend((0..n).map(|_| flag.clone()));
            }
            _ => {
                for v in values {
                    line.push(format!("{flag}={v}"));
                }
            }
        }
    }
    positional.sort_by_key(|(i, _)| *i);
    if !positional.is_empty() {
        line.push("--".into());
        line.extend(positional.into_iter().flat_map(|(_, v)| v));
    }
    Ok(line)
}

// ---- calls ---------------------------------------------------------------

/// How a call ended.
enum Outcome {
    /// The command's exit code and what it wrote.
    Ran(ExitCode, Vec<u8>, Vec<u8>),
    Failed(Failure),
    Cancelled,
}

fn error_json(f: &Failure) -> String {
    json!({"schema": "nupdf.error/v1", "error": {"code": f.code, "message": f.message}}).to_string()
}

/// Runs `tool` with `args`, stopped by `interrupt`.
fn call(tool: &str, args: &Map<String, Value>, interrupt: &Arc<Interrupt>) -> Outcome {
    let usage = |message: String| {
        Outcome::Failed(Failure {
            exit: crate::Exit::Usage,
            code: "usage.bad_argument",
            message,
        })
    };
    let line = match command_line(tool, args) {
        Ok(l) => l,
        Err(e) => return usage(e),
    };
    match (
        crate::run_line(&line, Some(interrupt.clone())),
        interrupt.reason(),
    ) {
        (_, Some(Reason::Cancelled)) => Outcome::Cancelled,
        (_, Some(Reason::TimedOut)) => Outcome::Failed(Failure {
            message: "the call ran out of time (timeout)".into(),
            ..crate::timed_out()
        }),
        (Ok(ran), None) => Outcome::Ran(ran.code, ran.out, ran.err),
        (Err(f), None) => Outcome::Failed(f),
    }
}

/// The result of a `tools/call`: the report as text, and as structured
/// content when it is one JSON object.
fn call_result(outcome: Outcome, max_output: usize) -> Option<Value> {
    let (code, out, err) = match outcome {
        Outcome::Cancelled => return None,
        Outcome::Failed(f) => return Some(failed(&f)),
        Outcome::Ran(code, out, err) => (code, out, err),
    };
    if out.len() + err.len() > max_output {
        return Some(failed(&Failure {
            exit: crate::Exit::Limit,
            code: "limit.output",
            message: format!(
                "the report is {} bytes, more than the server returns ({max_output}); \
                 ask for less with pages, fields, max_chars or max_items",
                out.len() + err.len()
            ),
        }));
    }
    let text = |b: &[u8]| String::from_utf8_lossy(b).trim_end().to_string();
    let mut content = Vec::new();
    for part in [&out, &err] {
        if !part.is_empty() {
            content.push(json!({"type": "text", "text": text(part)}));
        }
    }
    let mut result = json!({"content": content, "isError": code != ExitCode::SUCCESS});
    // The report, when it is the whole of what the call gave: stdout's, or
    // a failure's from stderr. Text and Markdown output has none: clients
    // show a model the structured content in place of the text, and the
    // text is what was asked for.
    let main = if code == ExitCode::SUCCESS {
        &out
    } else {
        &err
    };
    let extra = if code == ExitCode::SUCCESS {
        &err
    } else {
        &out
    };
    if extra.is_empty()
        && let Ok(v @ Value::Object(_)) = serde_json::from_slice::<Value>(main)
    {
        result["structuredContent"] = v;
    }
    Some(result)
}

fn failed(f: &Failure) -> Value {
    let text = error_json(f);
    let structured: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    json!({"content": [{"type": "text", "text": text}], "isError": true, "structuredContent": structured})
}

// ---- the server ----------------------------------------------------------

/// One line of JSON-RPC to the client.
fn send(out: &Mutex<std::io::Stdout>, message: &Value) {
    let mut out = out.lock().unwrap_or_else(PoisonError::into_inner);
    let _ = writeln!(out, "{message}");
    let _ = out.flush();
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// Serves until stdin closes; the process's exit code.
pub fn serve(call_timeout: f64, max_output: usize) -> u8 {
    SERVING.store(true, Ordering::Relaxed);
    let out = Arc::new(Mutex::new(std::io::stdout()));
    let running: Arc<Mutex<HashMap<String, Arc<Interrupt>>>> = Arc::default();
    let mut workers = Vec::new();
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let message: Value = match serde_json::from_str(&line) {
            Ok(m @ Value::Object(_)) => m,
            _ => {
                send(
                    &out,
                    &rpc_error(Value::Null, -32700, "not a JSON-RPC message"),
                );
                continue;
            }
        };
        let id = message.get("id").cloned();
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let reply = |result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result});
        match (method, &id) {
            ("initialize", Some(_)) => {
                let asked = params.get("protocolVersion").and_then(Value::as_str);
                let version = asked
                    .filter(|v| PROTOCOLS.contains(v))
                    .unwrap_or(PROTOCOLS[0]);
                send(
                    &out,
                    &reply(json!({
                        "protocolVersion": version,
                        "capabilities": {"tools": {"listChanged": false}},
                        "serverInfo": {"name": "nupdf", "version": env!("CARGO_PKG_VERSION")},
                        "instructions": INSTRUCTIONS,
                    })),
                );
            }
            ("ping", Some(_)) => send(&out, &reply(json!({}))),
            ("tools/list", Some(_)) => send(&out, &reply(json!({"tools": tools()}))),
            ("tools/call", Some(call_id)) => {
                let tool = params
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let args = match params.get("arguments") {
                    Some(Value::Object(a)) => a.clone(),
                    None | Some(Value::Null) => Map::new(),
                    Some(_) => {
                        send(
                            &out,
                            &rpc_error(call_id.clone(), -32602, "arguments is an object"),
                        );
                        continue;
                    }
                };
                let seconds = args
                    .get(TIMEOUT)
                    .and_then(Value::as_f64)
                    .map_or(call_timeout, |t| t.min(call_timeout));
                let deadline = Duration::try_from_secs_f64(seconds.max(0.0))
                    .ok()
                    .and_then(|d| Instant::now().checked_add(d));
                let interrupt = Interrupt::new(deadline);
                let key = call_id.to_string();
                running
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(key.clone(), interrupt.clone());
                let (out, running, call_id) = (out.clone(), running.clone(), call_id.clone());
                workers.push(std::thread::spawn(move || {
                    let outcome = call(&tool, &args, &interrupt);
                    running
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .remove(&key);
                    if let Some(result) = call_result(outcome, max_output) {
                        send(
                            &out,
                            &json!({"jsonrpc": "2.0", "id": call_id, "result": result}),
                        );
                    }
                }));
                workers.retain(|w| !w.is_finished());
            }
            ("notifications/cancelled", None) => {
                if let Some(request) = params.get("requestId")
                    && let Some(i) = running
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .get(&request.to_string())
                {
                    i.cancel();
                }
            }
            (_, Some(id)) => send(&out, &rpc_error(id.clone(), -32601, "method not found")),
            // Other notifications (initialized, progress) need no answer.
            (_, None) => {}
        }
    }
    // No more requests: finish the calls still running, each within its
    // time, and answer them.
    for w in workers {
        let _ = w.join();
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: Value) -> Map<String, Value> {
        match v {
            Value::Object(m) => m,
            _ => Map::new(),
        }
    }

    #[test]
    fn every_command_but_mcp_is_a_tool_with_its_options() {
        let tools = tools();
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert!(names.contains(&"extract") && names.contains(&"redact"));
        assert!(!names.contains(&"mcp"));
        let extract = tools.iter().find(|t| t["name"] == "extract").unwrap();
        let props = &extract["inputSchema"]["properties"];
        assert_eq!(props["file"]["type"], json!(["string", "number"]));
        assert_eq!(
            props["format"]["enum"],
            json!(["json", "jsonl", "text", "markdown"])
        );
        assert_eq!(props["pretty"], Value::Null);
        assert!(props["password"].is_object() && props["timeout"].is_object());
        assert_eq!(extract["inputSchema"]["required"], json!(["file"]));
    }

    #[test]
    fn arguments_become_the_command_line() {
        let line = command_line(
            "extract",
            &args(json!({"file": "-odd.pdf", "max_chars": 500, "format": "text", "timeout": 3})),
        )
        .unwrap();
        assert_eq!(
            line,
            [
                "nupdf",
                "extract",
                "--format=text",
                "--max-chars=500",
                "--",
                "-odd.pdf"
            ]
        );
        let clean = command_line(
            "clean",
            &args(json!({"file": "a.pdf", "garbage": 2, "dry_run": true})),
        )
        .unwrap();
        assert!(
            clean.iter().filter(|w| *w == "-g").count() == 2
                || clean.contains(&"--garbage".to_string())
        );
        assert!(command_line("extract", &args(json!({"nope": 1}))).is_err());
        assert!(command_line("mcp", &Map::new()).is_err());
    }
}
