//! What this node remembers about its own writes, under
//! `/var/lib/pve-meta-nvidia`: which guests it wrote managed lines into
//! (`managed/<vmid>`, one empty file each) and the `nvidia-uvm` major it last
//! wrote lines with (`uvm-major`).
//!
//! Both answer questions the store cannot. **Which guests**: a document with
//! no `gpu` key is either a container this tool wrote lines into whose key was
//! removed, or one a human configured by hand and this tool must not touch —
//! a key that is gone says nothing, and status in the store is not an option
//! (it would move the version token and fight the human editing the same
//! document). **Which major**: `nvidia-uvm`'s is dynamic, so replacing last
//! boot's rule means knowing which one it was, rather than guessing "a major
//! nobody claims" and deleting somebody else's rule for an unloaded device.
//!
//! This is not [ADR 009]'s sweeper: it removes nothing on its own and reads
//! nothing of the store. It answers an ownership question about a **host**
//! file — the container's config — so it lives on the host, beside the file it
//! is about, and a vmid leaves it when the guest is no longer one of this
//! node's containers.
//!
//! [ADR 009]: pve-meta `docs/decisions/009-no-sweeper.md`

use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::{Context, Result};

/// Where the record lives. `StateDirectory=` in the unit creates it.
pub const DIR: &str = "/var/lib/pve-meta-nvidia";
/// How many `nvidia-uvm` majors are remembered, newest first. Two: this
/// boot's and the one before it, which is what a guest that missed the pass
/// where the major changed still carries. More generations would only widen
/// the window in which a major that has since been given to another device
/// looks like one of ours.
pub const UVM_KEPT: usize = 2;

pub struct Record {
    dir: PathBuf,
}

impl Default for Record {
    fn default() -> Self {
        Record::at(PathBuf::from(DIR))
    }
}

impl Record {
    pub fn at(dir: PathBuf) -> Self {
        Record { dir }
    }

    fn managed_dir(&self) -> PathBuf {
        self.dir.join("managed")
    }

    fn path(&self, vmid: u32) -> PathBuf {
        self.managed_dir().join(vmid.to_string())
    }

    /// Whether this node wrote managed lines into `vmid`'s config.
    pub fn has(&self, vmid: u32) -> bool {
        self.path(vmid).exists()
    }

    /// Every vmid recorded. A directory that is there and cannot be listed is
    /// an error, never an empty record: an empty one would make every managed
    /// guest look hand-configured. A directory that was never created is the
    /// empty record it says it is.
    pub fn all(&self) -> Result<BTreeSet<u32>> {
        let dir = self.managed_dir();
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
            Err(e) => return Err(e).with_context(|| format!("cannot list {}", dir.display())),
        };
        let mut out = BTreeSet::new();
        for entry in entries {
            let entry = entry.with_context(|| format!("cannot list {}", dir.display()))?;
            if let Some(vmid) = entry.file_name().to_str().and_then(|n| n.parse().ok()) {
                out.insert(vmid);
            }
        }
        Ok(out)
    }

    /// Records that `vmid` carries managed lines, or that it does not.
    ///
    /// The caller sets it **before** writing lines and clears it **after**
    /// removing them, so the record is true whenever the config might carry
    /// lines: a record without lines is a no-op, lines without a record are
    /// orphans.
    pub fn set(&self, vmid: u32, managed: bool) -> Result<()> {
        let path = self.path(vmid);
        if managed {
            if path.exists() {
                return Ok(());
            }
            let dir = self.managed_dir();
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("cannot create {}", dir.display()))?;
            std::fs::File::create(&path)
                .with_context(|| format!("cannot create {}", path.display()))?;
        } else if let Err(e) = std::fs::remove_file(&path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                return Err(e).with_context(|| format!("cannot remove {}", path.display()));
            }
        }
        Ok(())
    }

    /// Forgets every vmid that is not one of this node's containers.
    pub fn retain(&self, local: &[u32]) -> Result<()> {
        for vmid in self.all()? {
            if !local.contains(&vmid) {
                self.set(vmid, false)?;
            }
        }
        Ok(())
    }

    fn uvm_path(&self) -> PathBuf {
        self.dir.join("uvm-major")
    }

    /// The `nvidia-uvm` majors this node has written lines with, newest
    /// first. Read once per pass, never per guest.
    ///
    /// A list, not a value: a guest that misses the pass where the major
    /// changes — locked for a backup at boot, its document unreadable that
    /// minute — keeps last boot's rule, and one remembered major later that
    /// rule would belong to nobody and stay for good. [`UVM_KEPT`] of them
    /// are kept: the ordinary loop reaches a skipped guest a poll later, so
    /// one generation of slack is enough, and every extra one is a major that
    /// another device may have been given since.
    ///
    /// A file that cannot be read is an **error**, not an empty list. Losing
    /// it costs twice: an old rule stops being owned, so it is left behind
    /// beside the new one and nothing will ever clean it up, and the next
    /// write starts the list again. The caller says so once and carries on
    /// with what it has.
    pub fn uvm_majors(&self) -> Result<Vec<u32>> {
        let path = self.uvm_path();
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
        };
        Ok(text
            .lines()
            .filter_map(|l| l.trim().parse().ok())
            .take(UVM_KEPT)
            .collect())
    }

    /// Remembers the major the lines being written carry, newest first.
    /// `known` is what this pass already read, so a pass does not read the
    /// file again for every guest.
    pub fn remember_uvm(&self, known: &[u32], major: u32) -> Result<()> {
        if known.first() == Some(&major) {
            return Ok(());
        }
        let mut majors = known.to_vec();
        majors.retain(|m| *m != major);
        majors.insert(0, major);
        majors.truncate(UVM_KEPT);
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("cannot create {}", self.dir.display()))?;
        let text: String = majors.iter().map(|m| format!("{m}\n")).collect();
        // Temp file and rename, not truncate-and-write: a crash mid-write
        // would otherwise leave an empty file, which reads as "this node has
        // written nothing" and orphans every rule it ever wrote.
        write_atomic(&self.uvm_path(), &text)
    }
}

/// Replaces a small file through a temporary sibling, so a crash leaves
/// either the old content or the new one.
fn write_atomic(path: &PathBuf, text: &str) -> Result<()> {
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, text).with_context(|| format!("cannot write {}", tmp.display()))?;
    let renamed = std::fs::rename(&tmp, path);
    if renamed.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    renamed.with_context(|| format!("cannot rename {} to {}", tmp.display(), path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pve-meta-nvidia-state-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_vmid_is_recorded_until_it_is_not() {
        let dir = scratch("managed");
        let r = Record::at(dir.clone());
        assert!(r.all().unwrap().is_empty());
        assert!(!r.has(423));
        r.set(423, true).unwrap();
        r.set(423, true).unwrap();
        r.set(9, true).unwrap();
        assert!(r.has(423));
        assert_eq!(r.all().unwrap().into_iter().collect::<Vec<_>>(), [9, 423]);
        r.retain(&[423]).unwrap();
        assert_eq!(r.all().unwrap().into_iter().collect::<Vec<_>>(), [423]);
        r.set(423, false).unwrap();
        r.set(423, false).unwrap();
        assert!(!r.has(423));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_uvm_majors_are_remembered_newest_first_and_bounded() {
        let dir = scratch("uvm");
        let r = Record::at(dir.clone());
        let majors = |r: &Record| r.uvm_majors().unwrap();
        assert!(majors(&r).is_empty());
        r.remember_uvm(&majors(&r), 505).unwrap();
        r.remember_uvm(&majors(&r), 505).unwrap();
        assert_eq!(majors(&r), [505]);
        r.remember_uvm(&majors(&r), 508).unwrap();
        assert_eq!(majors(&r), [508, 505]);
        // A major that comes back moves to the front instead of appearing
        // twice.
        r.remember_uvm(&majors(&r), 505).unwrap();
        assert_eq!(majors(&r), [505, 508]);
        for m in 1..=UVM_KEPT as u32 {
            r.remember_uvm(&majors(&r), 500 + m).unwrap();
        }
        assert_eq!(majors(&r).len(), UVM_KEPT);
        assert_eq!(majors(&r)[0], 500 + UVM_KEPT as u32);
        // Nothing beside it: the write is a rename, not a truncate.
        let left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n.to_string_lossy().starts_with("uvm-major"))
            .collect();
        assert_eq!(left, ["uvm-major"]);
        std::fs::write(dir.join("uvm-major"), "nonsense").unwrap();
        assert!(majors(&r).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
