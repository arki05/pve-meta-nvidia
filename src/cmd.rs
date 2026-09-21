//! Running the node's own tools: `pve-meta`, `nvidia-smi`, `nvidia-modprobe`.
//!
//! **Every call is bounded, and the bound is real.** A GPU in a bad state
//! wedges `nvidia-smi` in uninterruptible sleep, and a `pve-meta` write waits
//! for whoever holds the document's cluster lock. An unbounded wait here is
//! not a slow pass, it is a pass that never ends: before the daemon's first
//! tick reports ready it holds up `pve-guests.service`, and after it, the loop
//! stops reconciling for good with nothing in the journal.
//!
//! The whole mechanism is one loop. Both pipes are non-blocking and are read
//! inside the same poll that waits for the child, so there are no reader
//! threads to join, nothing to hand to a reaper, and — the point — **no way
//! to return a short answer**: either the command finished and its output is
//! all of it, or the call failed. A truncated `pve-meta get` is still valid
//! YAML with fewer `devices:` entries in it, which would take a card away from
//! a running container; that must not be representable.
//!
//! A child that misses its deadline is killed by **process group**, which
//! takes the tools' own children with it, and is then *not waited for*: a
//! process in `D` state never takes `SIGKILL`. It goes on a short list that
//! the next call clears with `try_wait`, so a kill that worked is reaped
//! within one poll and one that did not costs one entry, not a thread.
//!
//! Each call pings the watchdog when it returns, so a ping means a bounded
//! step *finished*: a daemon stuck inside one of these is not alive, and
//! should be restarted rather than kept.

use std::io::{ErrorKind, Read};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt as _;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::notify;

/// What a `pve-meta` call is given. It takes the document's cluster lock, so
/// it waits for whoever holds it, and pve-meta's own lock timeout is 30s.
pub const META: Duration = Duration::from_secs(30);
/// `nvidia-smi` only lists the cards here, but it talks to the driver.
pub const SMI: Duration = Duration::from_secs(10);
/// `nvidia-modprobe` loads a kernel module and makes device nodes.
pub const MODPROBE: Duration = Duration::from_secs(30);
/// How much of a wedged command's stderr goes into the error.
const TAIL: usize = 400;

/// Captured output of a finished command. There is no "partial": a call that
/// could not finish is an error.
#[derive(Debug)]
pub struct Output {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

fn render(program: &str, args: &[&str]) -> String {
    let mut s = String::from(program);
    for a in args {
        s.push(' ');
        if a.contains(' ') || a.is_empty() {
            s.push('\'');
            s.push_str(a);
            s.push('\'');
        } else {
            s.push_str(a);
        }
    }
    s
}

/// Runs `program args`, capturing both streams. Fails on a non-zero exit.
pub fn run(program: &str, args: &[&str], timeout: Duration) -> Result<Output> {
    let out = run_status(program, args, timeout)?;
    if out.status != 0 {
        bail!(
            "{} failed (exit {}){}",
            render(program, args),
            out.status,
            if out.stderr.trim().is_empty() {
                String::new()
            } else {
                format!(": {}", out.stderr.trim())
            }
        );
    }
    Ok(out)
}

/// Children that were killed and have not been collected yet. A `SIGKILL`
/// that landed is reaped by the next call; one that did not — the `D`-state
/// case this exists for — waits here instead of holding a thread.
static KILLED: Mutex<Vec<Child>> = Mutex::new(Vec::new());

fn reap_killed() {
    if let Ok(mut killed) = KILLED.lock() {
        killed.retain_mut(|child| matches!(child.try_wait(), Ok(None)));
    }
}

fn remember_killed(child: Child) {
    if let Ok(mut killed) = KILLED.lock() {
        killed.push(child);
        killed.retain_mut(|child| matches!(child.try_wait(), Ok(None)));
    }
}

/// How many killed children are still uncollected, for a test that wants to
/// know nothing accumulates.
#[cfg(test)]
pub fn pending_kills() -> usize {
    KILLED.lock().map(|k| k.len()).unwrap_or(0)
}

fn set_nonblocking(fd: &impl AsRawFd) -> Result<()> {
    // SAFETY: a pipe this process owns, for the duration of the call.
    let rc = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error()).context("cannot set a pipe non-blocking");
    }
    Ok(())
}

/// Reads whatever is there without waiting for more.
fn drain(fh: &mut impl Read, buf: &mut Vec<u8>) {
    let mut chunk = [0u8; 8192];
    loop {
        match fh.read(&mut chunk) {
            Ok(0) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return,
        }
    }
}

/// Like [`run`], but a non-zero exit is returned, not an error. A child that
/// outlives `timeout` is killed with its process group and reported as an
/// error — promptly, whether or not the kill actually ends it.
pub fn run_status(program: &str, args: &[&str], timeout: Duration) -> Result<Output> {
    if verbose() {
        eprintln!("+ {}", render(program, args));
    }
    reap_killed();
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Its own process group: killing it later takes whatever it spawned
        // with it, instead of leaving the wedged half behind.
        .process_group(0)
        .spawn()
        .with_context(|| format!("cannot run {program}"))?;
    let pgid = child.id() as libc::pid_t;
    let mut out = child.stdout.take().expect("stdout is piped");
    let mut err = child.stderr.take().expect("stderr is piped");
    set_nonblocking(&out)?;
    set_nonblocking(&err)?;
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());

    let deadline = Instant::now() + timeout;
    let status = loop {
        // Drained every turn, so a command that fills a pipe keeps going
        // instead of blocking against a parent that is waiting for it.
        drain(&mut out, &mut stdout);
        drain(&mut err, &mut stderr);
        match child.try_wait() {
            Ok(Some(status)) => {
                // The writer is gone: what is in the pipe is the rest of it.
                drain(&mut out, &mut stdout);
                drain(&mut err, &mut stderr);
                break status;
            }
            Ok(None) if Instant::now() >= deadline => {
                // SAFETY: `pgid` is this child's own process group, made above.
                unsafe { libc::killpg(pgid, libc::SIGKILL) };
                let said = tail(&String::from_utf8_lossy(&stderr));
                remember_killed(child);
                notify::alive();
                bail!(
                    "{} did not finish within {}s and was killed{said}",
                    render(program, args),
                    timeout.as_secs()
                );
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => {
                unsafe { libc::killpg(pgid, libc::SIGKILL) };
                remember_killed(child);
                notify::alive();
                return Err(e).with_context(|| format!("waiting for {program}"));
            }
        }
    };
    notify::alive();
    Ok(Output {
        status: status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

/// The last [`TAIL`] bytes of what a command said, for an error message.
fn tail(text: &str) -> String {
    let text = text.trim();
    if text.is_empty() {
        return String::new();
    }
    let start = text.len().saturating_sub(TAIL);
    let cut = if text.is_char_boundary(start) {
        start
    } else {
        0
    };
    format!(": {}", text[cut..].trim())
}

static VERBOSE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Turns on command echoing to stderr (`-v`).
pub fn set_verbose(on: bool) {
    VERBOSE.store(on, std::sync::atomic::Ordering::Relaxed);
}

pub fn verbose() -> bool {
    VERBOSE.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_is_captured_and_the_status_returned() {
        let out = run_status("/bin/sh", &["-c", "echo out; echo err >&2; exit 3"], SMI).unwrap();
        assert_eq!(out.status, 3);
        assert_eq!(out.stdout.trim(), "out");
        assert_eq!(out.stderr.trim(), "err");
        assert!(run("/bin/sh", &["-c", "exit 3"], SMI).is_err());
        assert!(run_status("/nonexistent/tool", &[], SMI).is_err());
    }

    #[test]
    fn output_is_never_short() {
        // More than a pipe holds, on both streams at once: a partial read of
        // a document is still valid YAML with fewer GPUs in it, which would
        // take a card off a running container. It must not be representable.
        let out = run_status(
            "/bin/sh",
            &[
                "-c",
                "i=0; while [ $i -lt 1200 ]; do \
                 echo \"line $i 0123456789012345678901234567890123456789012345678901234567890123\"; \
                 echo \"err $i\" >&2; i=$((i+1)); done",
            ],
            SMI,
        )
        .unwrap();
        assert_eq!(out.stdout.lines().count(), 1200);
        assert_eq!(out.stderr.lines().count(), 1200);
        assert!(out
            .stdout
            .lines()
            .next_back()
            .unwrap()
            .starts_with("line 1199"));
        // Past a pipe buffer on both streams, which is where a reader that
        // only drains at the end would lose the difference.
        assert!(out.stdout.len() > 64 * 1024, "{} bytes", out.stdout.len());
    }

    #[test]
    fn every_call_pings_the_watchdog_when_it_returns() {
        // A ping means a bounded step finished, so a daemon stuck inside one
        // is not counted as alive.
        let before = notify::pings();
        run_status("/bin/sh", &["-c", "exit 0"], SMI).unwrap();
        assert!(notify::pings() > before);
        let before = notify::pings();
        let _ = run_status("/bin/sh", &["-c", "sleep 300"], Duration::from_millis(200));
        assert!(notify::pings() > before, "a timeout pings too");
    }

    #[test]
    fn a_command_that_hangs_is_killed_and_reported() {
        let started = Instant::now();
        let err = run_status(
            "/bin/sh",
            &[
                "-c",
                "echo 'stuck on /dev/nvidia0' >&2; sleep 300 & sleep 300",
            ],
            Duration::from_millis(300),
        )
        .expect_err("a hanging command must be an error");
        let text = err.to_string();
        assert!(text.contains("did not finish"), "{text}");
        // What it said before wedging is usually what names the wedge.
        assert!(text.contains("stuck on /dev/nvidia0"), "{text}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "it waited {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_grandchild_that_left_the_process_group_does_not_hold_the_deadline() {
        // The case the deadline exists for: the kill cannot reach everything
        // holding the pipe, so nothing on this path may wait for it.
        let started = Instant::now();
        let err = run_status(
            "/bin/sh",
            &[
                "-c",
                "perl -e 'use POSIX; POSIX::setsid(); sleep 5' & sleep 300",
            ],
            Duration::from_millis(300),
        )
        .expect_err("a hanging command must be an error");
        assert!(err.to_string().contains("did not finish"));
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "it waited {:?} for a grandchild it cannot kill",
            started.elapsed()
        );
    }

    #[test]
    fn repeated_wedges_leave_nothing_behind() {
        // The motivating case recurs every poll for as long as the card is
        // bad, so a wedge may cost nothing that accumulates.
        for _ in 0..5 {
            let _ = run_status("/bin/sh", &["-c", "sleep 300"], Duration::from_millis(100));
        }
        // Killed children are collected by the next call, so this settles at
        // zero rather than growing with the wedges.
        run_status("/bin/sh", &["-c", "exit 0"], SMI).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        run_status("/bin/sh", &["-c", "exit 0"], SMI).unwrap();
        assert_eq!(pending_kills(), 0);
    }

    #[test]
    fn a_tail_is_the_end_of_what_was_said() {
        assert_eq!(tail("   "), "");
        assert_eq!(tail("boom"), ": boom");
        let long = "x".repeat(TAIL + 50);
        assert_eq!(tail(&long).len(), TAIL + 2);
    }
}
