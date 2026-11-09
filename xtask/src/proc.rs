// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! Runs a child process with a timeout and reports its wall time and peak
//! memory. Peak RSS comes from wait4(), which std does not expose.

use std::ffi::OsStr;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Exited(i32),
    /// Killed by a signal: a crash (SIGSEGV, SIGABRT, ...) unless we sent it.
    Signaled(i32),
    TimedOut,
}

#[derive(Debug)]
pub struct Run {
    pub status: Status,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub wall: Duration,
    pub max_rss_bytes: u64,
}

impl Run {
    pub fn succeeded(&self) -> bool {
        self.status == Status::Exited(0)
    }
}

pub fn run<S: AsRef<OsStr>>(program: &Path, args: &[S], timeout: Duration) -> Result<Run> {
    let start = Instant::now();
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning {}", program.display()))?;
    let pid = libc::pid_t::try_from(child.id()).context("pid out of range")?;

    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut buf);
            }
            buf
        })
    };
    let stdout = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let stderr = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );

    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut status: libc::c_int = 0;
        // SAFETY: rusage is plain old data; zeroed is a valid value.
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        // SAFETY: pid is our own child, and both out-pointers are valid.
        let rc = unsafe { libc::wait4(pid, &mut status, 0, &mut usage) };
        let _ = tx.send((rc, status, usage, Instant::now()));
    });

    let (reaped, timed_out) = match rx.recv_timeout(timeout) {
        Ok(reaped) => (reaped, false),
        Err(_) => {
            // SAFETY: the child has not been reaped yet, so the pid is still ours.
            unsafe { libc::kill(pid, libc::SIGKILL) };
            (rx.recv().context("waiter thread vanished")?, true)
        }
    };
    // The child is reaped; dropping the handle neither waits nor kills.
    drop(child);
    let (rc, raw, usage, end) = reaped;
    if rc < 0 {
        bail!("wait4 failed: {}", std::io::Error::last_os_error());
    }

    let status = if timed_out {
        Status::TimedOut
    } else if libc::WIFEXITED(raw) {
        Status::Exited(libc::WEXITSTATUS(raw))
    } else if libc::WIFSIGNALED(raw) {
        Status::Signaled(libc::WTERMSIG(raw))
    } else {
        Status::Exited(-1)
    };
    let max_rss = u64::try_from(usage.ru_maxrss).unwrap_or(0);
    // macOS reports bytes, Linux reports kilobytes.
    let max_rss_bytes = if cfg!(target_os = "macos") {
        max_rss
    } else {
        max_rss * 1024
    };

    Ok(Run {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
        wall: end - start,
        max_rss_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str, timeout: Duration) -> Run {
        run(Path::new("/bin/sh"), &["-c", script], timeout).unwrap()
    }

    #[test]
    fn captures_exit_code_and_output() {
        let run = sh("echo out; echo err >&2; exit 3", Duration::from_secs(10));
        assert_eq!(run.status, Status::Exited(3));
        assert_eq!(run.stdout, b"out\n");
        assert_eq!(run.stderr, b"err\n");
        assert!(run.max_rss_bytes > 0);
    }

    #[test]
    fn reports_crashes_as_signals() {
        let run = sh("kill -SEGV $$", Duration::from_secs(10));
        assert_eq!(run.status, Status::Signaled(libc::SIGSEGV));
    }

    #[test]
    fn kills_processes_that_exceed_the_timeout() {
        let started = Instant::now();
        let run = sh("exec sleep 30", Duration::from_millis(200));
        assert_eq!(run.status, Status::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
