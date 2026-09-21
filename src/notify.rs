//! `sd_notify`: telling systemd the boot pass is done, and that the loop is
//! still turning.
//!
//! The watchdog ping lives here rather than in the loop because **a ping must
//! mean "a bounded step finished"**, and the bounded steps are the external
//! commands (`cmd`). Pinging only between guests would leave the longest
//! stretch of a pass — the inventory, which is a `nvidia-modprobe`, an
//! `nvidia-smi` and up to two `pve-meta` calls, each legal and each with its
//! own deadline — with no ping at all, and a watchdog that kills a daemon for
//! being slow rather than for being stuck is worse than no watchdog.
//!
//! Nothing but systemd reads any of this, and a daemon started by hand has no
//! `NOTIFY_SOCKET`. A socket that is there and does not work is reported
//! **once**: it is one line of truth, not one line per guest per poll.

use std::sync::atomic::{AtomicBool, Ordering};

/// Tells systemd the boot pass is done, so `pve-guests.service` may start the
/// containers.
pub fn ready() {
    send("READY=1\n");
}

/// Tells systemd this loop is still turning, which it does between bounded
/// steps. With `WatchdogSec=` in the unit, a step that never ends restarts the
/// daemon instead of leaving a process that runs and reconciles nothing.
pub fn alive() {
    send("WATCHDOG=1\n");
    #[cfg(test)]
    PINGS.fetch_add(1, Ordering::Relaxed);
}

/// Whether a failing socket has already been reported.
static REPORTED: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
static PINGS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many pings have been sent, for a test that wants to know a code path
/// reports progress.
#[cfg(test)]
pub fn pings() -> u64 {
    PINGS.load(Ordering::Relaxed)
}

/// The socket and the address, made once: these are sent from every bounded
/// step, and a new socket per ping is a syscall pair for nothing.
fn channel() -> Option<&'static (std::os::unix::net::UnixDatagram, std::path::PathBuf)> {
    static CHANNEL: std::sync::OnceLock<
        Option<(std::os::unix::net::UnixDatagram, std::path::PathBuf)>,
    > = std::sync::OnceLock::new();
    CHANNEL
        .get_or_init(|| {
            let path = std::path::PathBuf::from(std::env::var_os("NOTIFY_SOCKET")?);
            let sock = std::os::unix::net::UnixDatagram::unbound().ok()?;
            Some((sock, path))
        })
        .as_ref()
}

fn send(message: &str) {
    let Some((sock, path)) = channel() else {
        return;
    };
    let sent = send_to(sock, path, message);
    match sent {
        Ok(_) => REPORTED.store(false, Ordering::Relaxed),
        Err(e) => note_failure(&path.display().to_string(), &e),
    }
}

/// Says once that the socket does not work. Once, because this is called
/// around every bounded step: a stale `NOTIFY_SOCKET` would otherwise be a
/// line per command per guest per poll, which is how a journal stops being
/// read.
fn note_failure(path: &str, e: &std::io::Error) {
    if !REPORTED.swap(true, Ordering::Relaxed) {
        eprintln!("cannot notify systemd through {path}: {e}");
    }
}

fn send_to(
    sock: &std::os::unix::net::UnixDatagram,
    path: &std::path::Path,
    message: &str,
) -> std::io::Result<usize> {
    let name = path.to_string_lossy();
    // The abstract namespace, which systemd spells with a leading '@'.
    if let Some(abstract_name) = name.strip_prefix('@') {
        #[cfg(target_os = "linux")]
        {
            use std::os::linux::net::SocketAddrExt;
            let addr =
                std::os::unix::net::SocketAddr::from_abstract_name(abstract_name.as_bytes())?;
            return sock.send_to_addr(message.as_bytes(), &addr);
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = abstract_name;
            return Ok(0);
        }
    }
    sock.send_to(message.as_bytes(), path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_socket_that_does_not_work_is_reported_once() {
        let failed = || std::io::Error::other("no such socket");
        REPORTED.store(false, Ordering::Relaxed);
        note_failure("/run/systemd/notify", &failed());
        assert!(REPORTED.load(Ordering::Relaxed));
        // Every further failure is silent: this is called around every
        // bounded step, so a stale socket is one line, not one per command.
        for _ in 0..100 {
            note_failure("/run/systemd/notify", &failed());
        }
        assert!(REPORTED.load(Ordering::Relaxed));
        // A socket that works again is a fresh state, and the next failure
        // is said once more.
        REPORTED.store(false, Ordering::Relaxed);
        note_failure("/run/systemd/notify", &failed());
        assert!(REPORTED.load(Ordering::Relaxed));
    }

    #[test]
    fn a_ping_without_a_socket_does_nothing_and_says_nothing() {
        let before = pings();
        alive();
        assert_eq!(pings(), before + 1);
    }
}
