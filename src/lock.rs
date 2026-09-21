//! PVE's own container config lock, and PVE's own way of replacing a config
//! file.
//!
//! The lock is `flock(LOCK_EX)` on `/run/lock/lxc/pve-config-<vmid>.lock`,
//! which is what `PVE::LXC::Config::config_file_lock` names and
//! `PVE::AbstractConfig::lock_config` takes around every read-modify-write of
//! a container config (through `PVE::Tools::lock_file_full`, which opens the
//! file `>>` and flocks it). Taking the same lock, the same way, is what
//! makes a `pct set` and a pass of this tool take turns instead of losing
//! each other's changes. The file is node-local, as the config is.
//!
//! The write is `PVE::File::file_set_contents`: a sibling `<file>.tmp.<pid>`
//! created `O_EXCL`, written, then renamed over the target. In `/etc/pve`
//! that is what pmxcfs sees as one atomic replacement, and it is what every
//! PVE config write does.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

/// Where PVE keeps container config locks.
pub const DIR: &str = "/run/lock/lxc";

pub struct ConfigLock {
    _file: File,
}

impl ConfigLock {
    /// Takes container `vmid`'s config lock, waiting up to `wait`.
    ///
    /// The daemon waits briefly and comes back at the next poll; the boot
    /// pass passes [`Duration::ZERO`] and does not wait at all, because it
    /// runs before `pve-guests.service` and a couple of locked guests must
    /// never add up to the containers' start being delayed.
    /// `Ok(None)` means somebody else holds it, which at boot -- where the
    /// wait is zero -- is the ordinary answer for a guest PVE is touching,
    /// not a failure.
    ///
    /// `flock` belongs to an open file description, so a process that forks
    /// while the file is open shares the lock with the child until it execs:
    /// "free" can be a moment away even after the holder let go. That is
    /// what the wait is for, and why the boot pass simply comes back at the
    /// next poll instead.
    pub fn take(vmid: u32, wait: Duration) -> Result<Option<Self>> {
        Self::take_in(Path::new(DIR), vmid, wait)
    }

    fn take_in(dir: &Path, vmid: u32, wait: Duration) -> Result<Option<Self>> {
        fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        let path = format!("{}/pve-config-{vmid}.lock", dir.display());
        let file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
            .with_context(|| format!("cannot open {path}"))?;
        let deadline = Instant::now() + wait;
        loop {
            // SAFETY: a valid, open descriptor owned by `file` for the call.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(Some(ConfigLock { _file: file }));
            }
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EWOULDBLOCK) {
                return Err(err).with_context(|| format!("flock {path}"));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(250).min(wait));
        }
    }
}

/// Replaces `path`'s content, the way `PVE::File::file_set_contents` does.
pub fn write_config(path: &Path, text: &str) -> Result<()> {
    let tmp = path.with_extension(format!("conf.tmp.{}", std::process::id()));
    for attempt in 0..3 {
        match OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(mut fh) => {
                // No fsync: pmxcfs replicates on close and PVE's own writer
                // does not call it either.
                let write = fh
                    .write_all(text.as_bytes())
                    .with_context(|| format!("cannot write {}", tmp.display()));
                if let Err(e) = write {
                    let _ = fs::remove_file(&tmp);
                    return Err(e);
                }
                drop(fh);
                // The guest may have been destroyed while this pass planned:
                // putting its config back would resurrect a file PVE removed.
                // This narrows the window, it does not close it -- a destroy
                // between this check and the rename still loses -- and the
                // guest's own config lock, held around all of it, is what
                // makes that window small.
                if !path.exists() {
                    let _ = fs::remove_file(&tmp);
                    bail!("{} is gone; not writing it back", path.display());
                }
                return fs::rename(&tmp, path).with_context(|| {
                    format!("cannot rename {} to {}", tmp.display(), path.display())
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempt < 2 => {
                fs::remove_file(&tmp)
                    .with_context(|| format!("cannot delete the old {}", tmp.display()))?;
            }
            Err(e) => return Err(e).with_context(|| format!("cannot create {}", tmp.display())),
        }
    }
    bail!("cannot create {}", tmp.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lock_somebody_holds_is_refused_at_once_with_no_wait() {
        let dir = std::env::temp_dir().join(format!("pve-meta-nvidia-lock-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let held = ConfigLock::take_in(&dir, 423, Duration::ZERO)
            .unwrap()
            .expect("a free lock is taken");
        let started = Instant::now();
        // The boot pass's rule: a guest PVE is holding is left to the next
        // poll rather than waited for.
        let busy = ConfigLock::take_in(&dir, 423, Duration::ZERO).unwrap();
        assert!(busy.is_none(), "a held lock is not taken twice");
        assert!(started.elapsed() < Duration::from_millis(250));
        // A different guest is not affected.
        assert!(ConfigLock::take_in(&dir, 424, Duration::ZERO)
            .unwrap()
            .is_some());
        drop(held);
        // With a wait, not at once: a process that forks while this file is
        // open passes the lock to the child until it execs, so "free again"
        // is a moment away rather than instant. The daemon's five seconds and
        // the CLI's thirty absorb that; a zero wait would not.
        assert!(ConfigLock::take_in(&dir, 423, Duration::from_secs(5))
            .unwrap()
            .is_some());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_config_is_replaced_through_a_temporary_sibling() {
        let dir = std::env::temp_dir().join(format!("pve-meta-nvidia-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("423.conf");
        fs::write(&path, "arch: amd64\n").unwrap();
        write_config(&path, "arch: amd64\nhostname: llm\n").unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "arch: amd64\nhostname: llm\n"
        );
        // Nothing is left beside it.
        let left: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, ["423.conf"]);
        // A config that vanished under the pass is not written back.
        fs::remove_file(&path).unwrap();
        assert!(write_config(&path, "arch: amd64\n").is_err());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        fs::remove_dir_all(&dir).unwrap();
    }
}
