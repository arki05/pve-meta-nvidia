//! `reconcile`: make one guest's config say what its document asks for.
//!
//! The plan is made twice: once unlocked, to decide whether anything has to
//! happen at all, and again under the container's own config lock with the
//! file read inside it, because that is the only plan a write may be based
//! on. Nothing is written when the managed lines are already what they should
//! be, so a pass over an unchanged node writes nothing at all.
//!
//! The record of what this node wrote is set **before** the lines are written
//! and cleared **after** they are removed: a record without lines is a no-op,
//! lines without a record are orphans nothing will ever clean up.
//!
//! A guest whose config PVE has locked (a backup, a snapshot, a migration) is
//! left alone until it is not, and so is every guest of a node whose driver is
//! not loaded.

use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::conf;
use crate::doc;
use crate::inventory::Inventory;
use crate::lock::{self, ConfigLock};
use crate::node;
use crate::ops::Ctx;
use crate::plan::{self, Outcome, Plan};

/// What one guest's reconcile did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Done {
    /// This node has no working driver; nothing was touched.
    NoDriver,
    /// No `gpu` key, and nothing this tool wrote: not ours.
    Unmanaged,
    /// PVE holds the config (`lock: backup`); try again later.
    Locked(String),
    /// The config cannot carry the lines; nothing was touched.
    Refused(String),
    /// Already what it should be.
    InSync(Plan),
    /// The managed lines were replaced.
    Wrote(Plan),
}

impl Done {
    pub fn plan(&self) -> Option<&Plan> {
        match self {
            Done::InSync(p) | Done::Wrote(p) => Some(p),
            _ => None,
        }
    }

    /// The word `status` and `--json` use.
    pub fn state(&self) -> &'static str {
        match self {
            Done::NoDriver => "no driver",
            Done::Unmanaged => "no gpu key",
            Done::Locked(_) => "locked",
            Done::Refused(_) => "refused",
            Done::InSync(p) if p.desired.is_empty() => "no lines",
            Done::InSync(_) => "in sync",
            Done::Wrote(_) => "written",
        }
    }

    /// One line for the log, or `None` when there is nothing to say.
    /// `running` decides whether a restart is owed for what was written.
    pub fn describe(&self, vmid: u32, running: bool) -> Option<String> {
        match self {
            Done::Unmanaged | Done::InSync(_) | Done::NoDriver => None,
            Done::Locked(l) => Some(format!("{vmid}: config locked ({l}); waiting")),
            Done::Refused(why) => Some(format!("{vmid}: refused: {why}")),
            Done::Wrote(p) if p.desired.is_empty() => {
                Some(format!("{vmid}: removed the managed lines"))
            }
            Done::Wrote(p) => Some(format!(
                "{vmid}: wrote {} line(s) for {}{}",
                p.desired.len(),
                p.gpus.join(", "),
                if running {
                    " (restart the container to apply)"
                } else {
                    ""
                }
            )),
        }
    }
}

/// One reconcile pass over this node: the inventory it was made with, the
/// `nvidia-uvm` majors this node has written lines with, and how long a
/// guest's config lock is waited for.
pub struct Pass<'a> {
    pub ctx: &'a Ctx,
    pub inv: &'a Inventory,
    pub remembered_uvm: Vec<u32>,
    pub lock_wait: Duration,
    /// Whether this pass has already written the major it is using into the
    /// record: it is one line for the node, not one per guest.
    pub noted_uvm: std::cell::Cell<bool>,
}

impl<'a> Pass<'a> {
    /// A pass for a command a human is waiting for: it may wait a while for a
    /// lock, because it has nothing else to do and nothing else is waiting on
    /// it.
    pub fn new(ctx: &'a Ctx, inv: &'a Inventory) -> Pass<'a> {
        Pass {
            ctx,
            inv,
            remembered_uvm: ctx.record.uvm_majors().unwrap_or_else(|e| {
                eprintln!("pve-meta-nvidia: {e:#}; the majors written before are not known");
                Vec::new()
            }),
            lock_wait: Duration::from_secs(30),
            noted_uvm: std::cell::Cell::new(false),
        }
    }

    /// Reconciles one guest of this node. `want` is its `gpu` subtree,
    /// already read.
    pub fn guest(&self, vmid: u32, want: Option<&doc::Gpu>) -> Result<Done> {
        if !self.inv.driver_loaded() {
            return Ok(Done::NoDriver);
        }
        let path = node::config_path(&self.ctx.node, vmid);
        let text = node::read_config(&self.ctx.node, vmid)?;
        if let Some(l) = conf::value_of(&text, "lock") {
            return Ok(Done::Locked(l.to_string()));
        }
        let managed = self.ctx.record.has(vmid);
        match self.plan(&text, want, managed) {
            Outcome::NoDriver => Ok(Done::NoDriver),
            Outcome::Unmanaged => Ok(Done::Unmanaged),
            Outcome::Refused(why) => Ok(Done::Refused(why)),
            Outcome::Plan(p) if !p.changed() => {
                self.remember(vmid, &p)?;
                Ok(Done::InSync(p))
            }
            Outcome::Plan(_) => {
                // The plan that decides the write is the one made under the
                // lock, from the file read inside it. A guest whose lock
                // somebody else holds -- at boot, where nothing is waited
                // for, that is any guest PVE is touching -- is simply done
                // later.
                let Some(_lock) = ConfigLock::take(vmid, self.lock_wait)? else {
                    return Ok(Done::Locked("another process holds its config lock".into()));
                };
                let text = node::read_config(&self.ctx.node, vmid)?;
                if let Some(l) = conf::value_of(&text, "lock") {
                    return Ok(Done::Locked(l.to_string()));
                }
                let p = match self.plan(&text, want, managed) {
                    Outcome::Plan(p) => p,
                    Outcome::Refused(why) => return Ok(Done::Refused(why)),
                    Outcome::Unmanaged => return Ok(Done::Unmanaged),
                    Outcome::NoDriver => return Ok(Done::NoDriver),
                };
                if !p.changed() {
                    self.remember(vmid, &p)?;
                    return Ok(Done::InSync(p));
                }
                let new = conf::splice(&text, &p.desired, &p.owned);
                if !p.desired.is_empty() {
                    self.remember(vmid, &p)?;
                }
                lock::write_config(std::path::Path::new(&path), &new)
                    .with_context(|| format!("guest {vmid}"))?;
                if p.desired.is_empty() {
                    self.ctx.record.set(vmid, false)?;
                }
                Ok(Done::Wrote(p))
            }
        }
    }

    fn plan(&self, text: &str, want: Option<&doc::Gpu>, managed: bool) -> Outcome {
        plan::plan(text, want, self.inv, &self.remembered_uvm, managed)
    }

    /// Records what the config is about to carry, or carries already.
    fn remember(&self, vmid: u32, p: &Plan) -> Result<()> {
        self.ctx
            .record
            .set(vmid, !p.desired.is_empty())
            .with_context(|| format!("guest {vmid}"))?;
        if !p.desired.is_empty() {
            if let Some(major) = self.inv.uvm_major {
                self.ctx.record.remember_uvm(&self.remembered_uvm, major)?;
            }
        }
        Ok(())
    }

    /// Reads a guest's document and reconciles it.
    pub fn one(&self, vmid: u32) -> Result<Done> {
        let want = doc::read(vmid)?;
        self.guest(vmid, want.as_ref())
    }

    /// Reconciles every container of this node, in vmid order. One guest's
    /// broken document or unreadable config is that guest's problem: the
    /// others are still done.
    pub fn all(&self) -> Result<Vec<(u32, Result<Done>)>> {
        let local = node::local_containers(&self.ctx.node)?;
        self.ctx.record.retain(&local)?;
        Ok(local
            .into_iter()
            .map(|vmid| (vmid, self.one(vmid)))
            .collect())
    }
}

/// `pve-meta-nvidia reconcile [<vmid>]`, at a terminal: every note and every
/// refusal is printed, and a refusal of the guest that was named is an error.
pub fn run(ctx: &Ctx, vmid: Option<u32>, json: bool) -> Result<()> {
    // An inventory that cannot be read means nothing is reconciled -- the
    // same rule the daemon follows. It is reported rather than thrown at
    // once, so the rows below still show what each guest asks for, and the
    // command still ends non-zero.
    let (inv, failed) = match crate::inventory::read() {
        Ok(inv) => (inv, None),
        Err(e) => (Inventory::default(), Some(format!("{e:#}"))),
    };
    if !json {
        for note in &inv.notes {
            eprintln!("pve-meta-nvidia: {note}");
        }
        match &failed {
            Some(e) => println!(
                "the GPUs of {} could not be read: {e}; nothing is reconciled",
                ctx.node
            ),
            None if !inv.driver_loaded() => println!(
                "no NVIDIA driver on {} (no /proc/driver/nvidia); nothing is reconciled",
                ctx.node
            ),
            None => {}
        }
    }
    let pass = Pass::new(ctx, &inv);
    let running = node::active_containers().unwrap_or_default();
    let Some(vmid) = vmid else {
        let mut rows = Vec::new();
        for (vmid, done) in pass.all()? {
            match done {
                Ok(done) => {
                    report(vmid, &done, running.contains(&vmid), json, &mut rows);
                }
                Err(e) => {
                    if json {
                        rows.push(serde_json::json!({"vmid": vmid, "error": format!("{e:#}")}));
                    } else {
                        eprintln!("{vmid}: {e:#}");
                    }
                }
            }
        }
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "node": ctx.node,
                    "inventory_error": failed,
                    "guests": rows,
                }))?
            );
        }
        // Rows first, then the truth about the exit status: a caller that
        // scripts this must not read "nothing to do" as "done".
        return match failed {
            Some(e) => bail!("the GPUs of {} could not be read: {e}", ctx.node),
            None => Ok(()),
        };
    };
    let local = node::local_containers(&ctx.node)?;
    if !local.contains(&vmid) {
        bail!("{vmid} is not a container of this node ({})", ctx.node);
    }
    let done = pass.one(vmid)?;
    let mut rows = Vec::new();
    report(vmid, &done, running.contains(&vmid), json, &mut rows);
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "node": ctx.node,
                "inventory_error": failed,
                "guests": rows,
            }))?
        );
    }
    if let Some(e) = failed {
        bail!("the GPUs of {} could not be read: {e}", ctx.node);
    }
    match done {
        Done::Refused(why) => bail!("{vmid}: {why}"),
        Done::Locked(l) => bail!("{vmid}: the config is locked ({l})"),
        _ => Ok(()),
    }
}

fn report(vmid: u32, done: &Done, running: bool, json: bool, rows: &mut Vec<serde_json::Value>) {
    if json {
        let plan = done.plan();
        rows.push(serde_json::json!({
            "vmid": vmid,
            "state": done.state(),
            "running": running,
            "gpus": plan.map(|p| p.gpus.clone()).unwrap_or_default(),
            "lines": plan.map(|p| p.desired.clone()).unwrap_or_default(),
            "notes": plan.map(|p| p.notes.clone()).unwrap_or_default(),
            "detail": match done {
                Done::Refused(why) => Some(why.clone()),
                Done::Locked(l) => Some(l.clone()),
                _ => None,
            },
        }));
        return;
    }
    if let Some(p) = done.plan() {
        for note in &p.notes {
            println!("{vmid}: {note}");
        }
    }
    match done {
        Done::Unmanaged | Done::NoDriver => {}
        Done::InSync(p) if p.desired.is_empty() => {}
        Done::InSync(p) => println!("{vmid}: in sync ({})", p.gpus.join(", ")),
        other => {
            if let Some(line) = other.describe(vmid, running) {
                println!("{line}");
            }
        }
    }
}
