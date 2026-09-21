//! The loop, one per node, acting only on the containers whose config lives
//! on this node. PVE keeps exactly one owner per guest, so a daemon on every
//! node never overlaps, and a migration moves the responsibility with the
//! guest.
//!
//! **The change signal is a `stat`.** Every [`Timing::poll`] the loop reads
//! the vmlist and stats each local guest's document
//! (`/etc/pve/meta/<vmid>.yaml`); a document whose stamp moved is read again
//! through the `pve-meta` CLI, which owns every rule about what a document
//! means. Every [`Timing::full`] each of them is read again anyway, because a
//! stamp is a hint and a rewrite inside one clock tick could look like none.
//! An API call per node per ten seconds — three operators, three forks of the
//! whole Perl API tree — buys nothing over that.
//!
//! Planning runs every poll for every local container: it is a config-file
//! read and a few string comparisons, and it is what catches a `pct set`, a
//! snapshot rollback or a hand edit with no second timer and no memo that can
//! go stale. Nothing is written when the lines are already right.
//!
//! The node's GPUs are read at the top of every poll — a walk of `/proc` and
//! one `nvidia-smi` — so a driver update without a reboot is noticed within
//! ten seconds and every managed line carrying the old major is rewritten.
//! This node's prefix file is written afterwards, when the cards changed or
//! the slow pass comes round, and on the boot pass **after** ready: it is a
//! courtesy to the editor, and two `pve-meta` calls of it have no business
//! standing between a booted node and its containers.
//!
//! **The first pass is the boot pass.** The unit is ordered before
//! `pve-guests.service`, so the containers PVE starts at boot are started from
//! configs that carry this boot's majors. It reads every local document
//! whatever the stamps say, and `sd_notify` reports ready when the pass is
//! done, whether it changed anything or not: a boot is never held up by this
//! tool failing. What it promises is bounded and no more than that — the
//! inventory, then up to [`Timing::boot`] of guests, taken in vmid order with
//! no lock waited for; the rest follow in the ordinary loop a poll later, and
//! a container that starts before its guest was reached starts without its
//! GPU until it is restarted.
//!
//! **Nothing unknown is treated as fact.**
//!
//! * The vmlist cannot be read: the poll ends, nothing is reconciled. An
//!   unmounted pmxcfs must never read as "no guest wants a GPU".
//! * The inventory could not be read: the last good one is kept and the
//!   reconcile phase is skipped entirely. Reconciling against a *guess* about
//!   the hardware is how a container loses the GPU it had.
//! * One guest's document cannot be read or does not parse: that guest is
//!   skipped, with its state logged once, and every other guest of the node is
//!   reconciled as usual. A typo in one document never freezes a node, least
//!   of all at boot.
//!
//! Logs go to stderr, which is the journal under systemd. A write is logged
//! every time; a state that holds (a refusal, a lock, a skipped UUID, a
//! document that does not parse, a store that is away) once, when it begins.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::doc::{self, Gpu};
use crate::inventory::Inventory;
use crate::node;
use crate::notify;
use crate::ops::{inventory, reconcile, Ctx};

/// How often the loop does what.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// How often everything is planned.
    pub poll: Duration,
    /// How often every document is read again, stamps or no stamps.
    pub full: Duration,
    /// How long a pass waits for a guest's config lock before leaving it to
    /// the next poll. The boot pass waits not at all (see [`Timing::boot`]).
    pub lock_wait: Duration,
    /// What the first pass may spend on guests before it reports ready.
    ///
    /// The boot pass is ordered before `pve-guests.service`, so every second
    /// it takes is a second before the containers start — and if it took
    /// longer than the unit's start timeout, systemd would kill it, the
    /// ordering would be satisfied anyway, and the containers would start
    /// with last boot's major: the exact failure the ordering exists to
    /// prevent. So it is given a budget, takes locks without waiting, and
    /// hands whatever is left to the ordinary loop, which is ten seconds
    /// behind it.
    pub boot: Duration,
}

impl Default for Timing {
    fn default() -> Timing {
        Timing {
            poll: Duration::from_secs(10),
            full: Duration::from_secs(300),
            lock_wait: Duration::from_secs(5),
            boot: Duration::from_secs(60),
        }
    }
}

/// Lines logged once per state: by guest (0 for the node) and key, the line
/// last logged.
#[derive(Default)]
pub struct Log {
    seen: HashMap<(u32, String), String>,
}

impl Log {
    /// Logs `line` unless it is what `key` last logged; `None` ends the
    /// state. `true` when something changed.
    pub fn once(&mut self, vmid: u32, key: &str, line: Option<String>) -> bool {
        let k = (vmid, key.to_string());
        match line {
            Some(l) if self.seen.get(&k) != Some(&l) => {
                eprintln!("{l}");
                self.seen.insert(k, l);
                true
            }
            Some(_) => false,
            None => self.seen.remove(&k).is_some(),
        }
    }

    /// Forgets every state of a guest that is no longer this node's.
    pub fn settle(&mut self, local: &[u32]) {
        self.seen.retain(|(v, _), _| *v == 0 || local.contains(v));
    }
}

/// A guest's document as the loop last read it, with the stamp it was read at.
struct Cached {
    stamp: Option<Stamp>,
    /// The parsed subtree, or why it could not be had.
    state: std::result::Result<Option<Gpu>, String>,
}

type Stamp = (i64, i64, u64);

/// The loop's whole state.
#[derive(Default)]
pub struct Daemon {
    pub log: Log,
    /// The last inventory that was read successfully. `None` until one is:
    /// an empty inventory is not an answer about this node.
    inv: Option<Inventory>,
    docs: HashMap<u32, Cached>,
    read_at: Option<Instant>,
    /// Whether a pass has run: the first one is the boot pass.
    started: bool,
    /// The inventory this node's prefix file was last written from, so an
    /// unchanged node does not ask the store about it every poll.
    published: Option<Inventory>,
}

/// Runs the loop forever.
pub fn run(ctx: Ctx, timing: Timing) -> Result<()> {
    eprintln!(
        "pve-meta-nvidia on {}: polling every {}s, every document again every {}s",
        ctx.node,
        timing.poll.as_secs(),
        timing.full.as_secs()
    );
    let mut d = Daemon::default();
    // The boot pass, then ready, then the courtesy: pve-guests waits for the
    // first of those and for neither of the others.
    d.tick(&ctx, timing, Instant::now());
    notify::ready();
    d.publish(&ctx, true);
    loop {
        std::thread::sleep(timing.poll);
        d.tick(&ctx, timing, Instant::now());
    }
}

impl Daemon {
    /// One poll. The first one is the boot pass: it spends at most
    /// [`Timing::boot`] on guests, takes no lock it cannot have at once, and
    /// leaves the rest to the poll ten seconds later.
    pub fn tick(&mut self, ctx: &Ctx, timing: Timing, now: Instant) {
        // The loop is alive at the top of every poll, before anything that
        // takes time.
        notify::alive();
        let boot = !self.started;
        self.started = true;
        self.take_inventory();
        // **After the inventory, not before it.** The inventory is bounded
        // but not quick -- a lazy nvidia-modprobe and an nvidia-smi are 40s
        // of legal worst case -- and a budget that started before it could be
        // spent by the time the guests come round, which would skip every one
        // of them and report ready with last boot's major still in their
        // configs.
        let budget = budget_from(boot, Instant::now(), timing);
        let local = match node::local_containers(&ctx.node) {
            Ok(l) => l,
            Err(e) => {
                self.log.once(0, "vmlist", Some(format!("{e:#}; waiting")));
                return;
            }
        };
        if self.log.once(0, "vmlist", None) {
            eprintln!("the vmlist is readable again");
        }
        self.log.settle(&local);
        if let Err(e) = ctx.record.retain(&local) {
            self.log.once(0, "record", Some(format!("{e:#}")));
            return;
        }
        self.log.once(0, "record", None);
        let full = self.read_documents(ctx, &local, timing, now, budget);

        // Nothing at all without an inventory that was really read: a plan
        // made against a guess is how a container loses the GPU it had.
        let Some(inv) = self.inv.clone() else {
            self.log.once(
                0,
                "reconcile",
                Some("no inventory of this node yet; nothing is reconciled".into()),
            );
            return;
        };
        if !inv.driver_loaded() {
            self.log.once(
                0,
                "reconcile",
                Some(format!(
                    "no NVIDIA driver on {} (no /proc/driver/nvidia); nothing is reconciled",
                    ctx.node
                )),
            );
            return;
        }
        self.log.once(0, "reconcile", None);

        let remembered = match ctx.record.uvm_majors() {
            Ok(majors) => {
                self.log.once(0, "uvm-record", None);
                majors
            }
            Err(e) => {
                // Two costs, both worth one line: an older rule stops being
                // ours, so a rewrite leaves it beside the new one, and the
                // list starts again at the next write.
                self.log.once(
                    0,
                    "uvm-record",
                    Some(format!(
                        "{e:#}; a rule written with an earlier nvidia-uvm major is no longer \
                         ours, so a rewrite leaves it beside the new one, and the record \
                         starts again at the next write"
                    )),
                );
                Vec::new()
            }
        };
        let pass = reconcile::Pass {
            ctx,
            inv: &inv,
            remembered_uvm: remembered,
            // The boot pass takes no lock it cannot have at once: a guest
            // whose config PVE is holding is done at the next poll, and the
            // containers are not kept waiting for it.
            lock_wait: if boot {
                Duration::ZERO
            } else {
                timing.lock_wait
            },
            noted_uvm: std::cell::Cell::new(false),
        };
        let running = node::active_containers().unwrap_or_default();
        for (vmid, want) in reconcilable(&self.docs, &local) {
            // Between guests as well as between ticks: a node with many
            // containers takes longer than one watchdog interval to walk.
            notify::alive();
            if spent(budget) {
                self.log.once(0, "budget", Some(out_of_time(1)));
                break;
            }
            match pass.guest(vmid, want.as_ref()) {
                Ok(done) => {
                    self.log.once(vmid, "error", None);
                    let notes = done.plan().map(|p| p.notes.join("; ")).unwrap_or_default();
                    self.log.once(
                        vmid,
                        "notes",
                        (!notes.is_empty()).then(|| format!("{vmid}: {notes}")),
                    );
                    if let reconcile::Done::Wrote(_) = &done {
                        // Every write is logged; the state it leaves is the
                        // one the next poll finds.
                        self.log.once(vmid, "state", None);
                        if let Some(line) = done.describe(vmid, running.contains(&vmid)) {
                            eprintln!("{line}");
                        }
                    } else {
                        self.log
                            .once(vmid, "state", done.describe(vmid, running.contains(&vmid)));
                    }
                }
                Err(e) => {
                    self.log.once(vmid, "error", Some(format!("{vmid}: {e:#}")));
                }
            }
        }
        if !boot {
            self.publish(ctx, full);
        }
    }

    /// Reads the documents whose stamp moved, every document every
    /// [`Timing::full`], and every document on the first pass. One guest's
    /// failure is that guest's alone. Returns whether this was a full pass.
    fn read_documents(
        &mut self,
        ctx: &Ctx,
        local: &[u32],
        timing: Timing,
        now: Instant,
        budget: Option<Instant>,
    ) -> bool {
        let full = self
            .read_at
            .is_none_or(|at| now.duration_since(at) >= timing.full);
        if full {
            self.read_at = Some(now);
        }
        let mut skipped = 0;
        let mut docs = HashMap::new();
        for &vmid in local {
            // Reading documents is the other half of a pass that can take a
            // while: every `pve-meta` call may wait for a document's cluster
            // lock, and a handful of those add up past a watchdog interval.
            notify::alive();
            if spent(budget) {
                // The boot pass is out of time. Whatever is left keeps its
                // last answer (or none) and is read at the next poll, which
                // is not ordered before anything.
                skipped += 1;
                if let Some(c) = self.docs.remove(&vmid) {
                    docs.insert(vmid, c);
                }
                continue;
            }
            // A stat that fails for any other reason than "no document" is
            // not an answer either: read the document and let that say. "No
            // document" is a stamp of its own and is cached like any other,
            // so the guests without one -- most of them -- cost a stat and
            // never a process.
            let stamped = doc::stamp(vmid);
            let stamp = stamped.as_ref().ok().copied().flatten();
            let cached = self.docs.remove(&vmid);
            let cached = match source_of(
                cached.as_ref(),
                stamp,
                stamped.is_ok(),
                full,
                ctx.record.has(vmid),
            ) {
                Source::Cached => cached.expect("only a cached read is `Cached`"),
                Source::Absent => Cached {
                    stamp,
                    state: Ok(None),
                },
                Source::Read => Cached {
                    stamp,
                    state: doc::read(vmid).map_err(|e| format!("{e:#}")),
                },
            };
            match &cached.state {
                Ok(_) => {
                    self.log.once(vmid, "document", None);
                }
                Err(e) => {
                    self.log.once(
                        vmid,
                        "document",
                        Some(format!("{vmid}: {e}; leaving its config alone")),
                    );
                }
            }
            docs.insert(vmid, cached);
        }
        self.docs = docs;
        if skipped > 0 {
            self.log.once(0, "budget", Some(out_of_time(skipped)));
        }
        full
    }

    /// Reads the node's GPUs. Every poll: it is a walk of `/proc` and one
    /// `nvidia-smi`, and a clock of its own was one more thing to reason
    /// about than the cost it saved.
    fn take_inventory(&mut self) {
        let inv = match crate::inventory::read() {
            Ok(inv) => {
                self.log.once(0, "inventory", None);
                inv
            }
            Err(e) => {
                // The last good inventory stays: hardware does not come and
                // go, and a reconcile against nothing would strip lines.
                self.log.once(
                    0,
                    "inventory",
                    Some(format!("inventory: {e:#}; keeping the last one")),
                );
                return;
            }
        };
        let notes = inv.notes.join("; ");
        self.log
            .once(0, "inv-notes", (!notes.is_empty()).then_some(notes));
        self.keep_inventory(inv);
    }

    /// Writes this node's prefix file, if the cards changed or the slow pass
    /// came round.
    ///
    /// **After the guests, and on the boot pass after `READY`.** It is a
    /// courtesy to the editor, not something any reconcile depends on, and up
    /// to two `pve-meta` calls of it have no business standing between a node
    /// that has booted and the containers it is meant to start.
    fn publish(&mut self, ctx: &Ctx, full: bool) {
        let Some(inv) = self.inv.clone() else { return };
        if !full && self.published.as_ref() == Some(&inv) {
            return;
        }
        match inventory::publish(ctx, &inv) {
            Ok(action) => {
                self.log.once(0, "prefix", None);
                self.published = Some(inv);
                if matches!(
                    action,
                    crate::prefix::Action::Written | crate::prefix::Action::Removed
                ) {
                    eprintln!(
                        "{}: {}",
                        crate::prefix::doc_id(&ctx.node),
                        inventory::action_word(&action)
                    );
                }
            }
            Err(e) => {
                self.log.once(
                    0,
                    "prefix",
                    Some(format!("the prefix file was not written: {e:#}")),
                );
            }
        }
    }

    /// Keeps an inventory that was really read.
    fn keep_inventory(&mut self, inv: Inventory) {
        let uvm = inv.uvm_major;
        if let Some(before) = self.inv.replace(inv) {
            if before.driver_loaded() && before.uvm_major != uvm {
                // The module was reloaded under us: every managed line
                // carrying the old major is rewritten by the next poll.
                eprintln!(
                    "the nvidia-uvm major moved ({} -> {})",
                    major(before.uvm_major),
                    major(uvm)
                );
            }
        }
    }
}

/// Whether a budgeted pass has run out of time. `None` is no budget: an
/// ordinary poll has all the time it needs, because nothing waits on it.
fn spent(budget: Option<Instant>) -> bool {
    budget.is_some_and(|deadline| Instant::now() >= deadline)
}

/// The boot pass's deadline, started **when the guests start** — `at` is a
/// timestamp taken after the inventory, which is not inside the budget.
fn budget_from(boot: bool, at: Instant, timing: Timing) -> Option<Instant> {
    boot.then(|| at + timing.boot)
}

/// What being out of time means for whoever reads the journal.
fn out_of_time(guests: usize) -> String {
    format!(
        "the boot pass is out of time with {guests} guest(s) left; they follow in the loop \
         within a poll, and a container of theirs that starts first starts without its GPU \
         until it is restarted"
    )
}

/// Where this poll's answer for one guest comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    /// The last read still stands.
    Cached,
    /// There is no document, so there is no `gpu` key; no process is run.
    Absent,
    /// Ask the `pve-meta` CLI.
    Read,
}

/// The one rule that decides what a poll costs, kept apart from the poll so
/// it can be read and tested as a rule:
///
/// * **the stat did not answer** — the document is read, and whatever is
///   wrong with the store is `pve-meta`'s to report, per guest;
/// * **no document file, and this node wrote no lines into that guest** — it
///   has no `gpu` key, and forking to be told so costs a process per guest
///   per poll. Never inferred for a guest in the record: there "no key" means
///   "remove the lines", which is not a conclusion to draw from a path;
/// * **the same stamp as the last read, outside the full pass** — the cached
///   read stands.
fn source_of(
    cached: Option<&Cached>,
    stamp: Option<Stamp>,
    stamped_ok: bool,
    full: bool,
    managed: bool,
) -> Source {
    if !stamped_ok {
        return Source::Read;
    }
    if stamp.is_none() && !managed {
        return Source::Absent;
    }
    match cached {
        Some(c) if !full && c.stamp == stamp => Source::Cached,
        _ => Source::Read,
    }
}

/// The guests this tick reconciles, with the subtree each asks for: those
/// whose document was read. A guest whose document could not be read or did
/// not parse is left out, and nothing else is — one bad document must never
/// cost the node a pass.
fn reconcilable(docs: &HashMap<u32, Cached>, local: &[u32]) -> Vec<(u32, Option<Gpu>)> {
    local
        .iter()
        .filter_map(|vmid| match docs.get(vmid) {
            Some(Cached {
                state: Ok(want), ..
            }) => Some((*vmid, want.clone())),
            _ => None,
        })
        .collect()
}

fn major(m: Option<u32>) -> String {
    match m {
        Some(m) => m.to_string(),
        None => "not loaded".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_state_is_logged_once() {
        let mut log = Log::default();
        assert!(log.once(423, "state", Some("refused".into())));
        assert!(!log.once(423, "state", Some("refused".into())));
        assert!(log.once(423, "state", Some("locked".into())));
        assert!(log.once(423, "state", None));
        assert!(!log.once(423, "state", None));
        // One key per condition: a document that does not parse does not end
        // the "the vmlist is away" state.
        assert!(log.once(0, "vmlist", Some("waiting".into())));
        assert!(log.once(423, "document", Some("bad".into())));
        assert!(log.once(0, "vmlist", None));
        assert!(!log.once(0, "vmlist", None));
        log.settle(&[]);
        assert!(log.once(423, "document", Some("bad".into())));
    }

    /// The reconcile phase is skipped, and no guest is touched, until an
    /// inventory has really been read.
    #[test]
    fn nothing_is_reconciled_without_an_inventory() {
        let d = Daemon::default();
        assert!(d.inv.is_none());
        // `Inventory::default()` is not a node with no GPUs; it is no answer,
        // and `plan` refuses to act on it either way.
        assert!(!Inventory::default().driver_loaded());
    }

    fn cached(stamp: Option<Stamp>) -> Option<Cached> {
        Some(Cached {
            stamp,
            state: Ok(None),
        })
    }

    fn at(secs: i64) -> Option<Stamp> {
        Some((secs, 0, 12))
    }

    #[test]
    fn a_document_is_read_again_only_when_its_stamp_moved() {
        let src = |c: Option<Cached>, stamp, ok, full| source_of(c.as_ref(), stamp, ok, full, true);
        // The stamp stands.
        assert_eq!(src(cached(at(7)), at(7), true, false), Source::Cached);
        // It appeared, changed, or went away.
        assert_eq!(src(cached(None), at(7), true, false), Source::Read);
        assert_eq!(src(cached(at(7)), at(8), true, false), Source::Read);
        assert_eq!(src(cached(at(7)), None, true, false), Source::Read);
        // Nothing cached, a stat that did not answer, and the full pass.
        assert_eq!(src(None, at(7), true, false), Source::Read);
        assert_eq!(src(cached(at(7)), at(7), false, false), Source::Read);
        assert_eq!(src(cached(at(7)), at(7), true, true), Source::Read);
    }

    #[test]
    fn a_guest_with_no_document_costs_no_process_and_a_managed_one_always_does() {
        // No document, never written to: no `gpu` key, no fork.
        assert_eq!(source_of(None, None, true, false, false), Source::Absent);
        assert_eq!(
            source_of(cached(None).as_ref(), None, true, false, false),
            Source::Absent
        );
        // The same guest, but this node wrote lines into it: "no key" would
        // mean "remove them", so it is read rather than inferred.
        assert_eq!(
            source_of(cached(None).as_ref(), None, true, false, true),
            Source::Cached
        );
        assert_eq!(source_of(None, None, true, false, true), Source::Read);
    }

    #[test]
    fn the_boot_pass_stops_when_its_budget_is_spent() {
        let now = Instant::now();
        // No budget: an ordinary poll has all the time it needs, because
        // nothing is ordered after it.
        assert!(!spent(None));
        // A budget that is already in the past: the guests still to do are
        // left to the next poll rather than holding up pve-guests.
        assert!(spent(Some(now - Duration::from_millis(1))));
        assert!(!spent(Some(now + Duration::from_secs(60))));
        // And the boot pass takes no lock it cannot have at once.
        let timing = Timing::default();
        assert!(timing.boot >= Duration::from_secs(30));
        assert!(timing.lock_wait > Duration::ZERO);
    }

    /// Every bounded step reports progress, so the inventory -- the longest
    /// stretch of a pass, and the one with no guests to ping between -- is
    /// never a ping-free path. This checks the property at its source: a
    /// command cannot be run without pinging.
    #[test]
    fn no_bounded_step_is_ping_free() {
        let before = crate::notify::pings();
        let _ = crate::inventory::read();
        let after = crate::notify::pings();
        // A node with no driver runs no command at all, which is why this is
        // a floor and not an equality; where a command does run, `cmd` pings
        // as it returns (see its own tests).
        assert!(after >= before);
        let before = crate::notify::pings();
        let _ = crate::cmd::run_status("/bin/sh", &["-c", "exit 0"], crate::cmd::SMI);
        assert!(crate::notify::pings() > before);
    }

    #[test]
    fn the_boot_budget_starts_after_the_inventory() {
        let now = Instant::now();
        let timing = Timing::default();
        // An ordinary poll is not budgeted: nothing is ordered after it.
        assert_eq!(budget_from(false, now, timing), None);
        // The boot pass is, and the clock starts where it is taken -- after
        // the inventory. A slow inventory (a lazy nvidia-modprobe and an
        // nvidia-smi are 40s of legal worst case) therefore leaves the guests
        // their full minute instead of skipping every one of them.
        let after_a_slow_inventory = now + Duration::from_secs(100);
        let budget = budget_from(true, after_a_slow_inventory, timing);
        assert_eq!(budget, Some(after_a_slow_inventory + timing.boot));
        assert!(!spent(budget));
        // And what it says when it does run out names the consequence.
        let line = out_of_time(3);
        assert!(line.contains("3 guest(s)"), "{line}");
        assert!(line.contains("without its GPU"), "{line}");
    }

    #[test]
    fn one_unreadable_document_costs_that_guest_and_nothing_else() {
        let docs = HashMap::from([
            (
                105,
                Cached {
                    stamp: None,
                    state: Ok(Some(Gpu::default())),
                },
            ),
            (
                423,
                Cached {
                    stamp: None,
                    state: Err("bad gpu document: unknown key".into()),
                },
            ),
            (
                900,
                Cached {
                    stamp: None,
                    state: Ok(None),
                },
            ),
        ]);
        let due = reconcilable(&docs, &[105, 423, 900, 999]);
        assert_eq!(
            due.iter().map(|(vmid, _)| *vmid).collect::<Vec<_>>(),
            [105, 900]
        );
        assert_eq!(due[1].1, None);
    }
}
