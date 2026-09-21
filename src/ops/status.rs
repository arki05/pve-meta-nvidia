//! `status`: what every guest of this node that has a `gpu` key (or that this
//! node wrote lines into) would get, and what its config says now.
//!
//! There is no "pending restart" state to report and none is invented: LXC
//! reads the config when the container starts, and nothing on the host says
//! which config text a running container was started from. What can be said
//! precisely is said: the desired lines, the lines in the config, and whether
//! the container is running — a running container whose lines were just
//! written needs a restart, and `status <vmid>` shows both sets so it is
//! obvious which.

use anyhow::Result;
use serde::Serialize;

use crate::conf;
use crate::doc;
use crate::inventory::Inventory;
use crate::node;
use crate::ops::Ctx;
use crate::plan::{self, Outcome};

#[derive(Debug, Serialize)]
pub struct Row {
    pub vmid: u32,
    pub name: String,
    pub running: bool,
    /// `in sync`, `out of sync`, `no lines`, `no gpu key`, `no driver`,
    /// `refused: ...`, `error: ...`.
    pub state: String,
    pub gpus: Vec<String>,
    pub desired: Vec<String>,
    pub current: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// One row per guest asked for. Guests without a `gpu` key are left out
/// unless one was named or this node wrote lines into them.
pub fn rows(ctx: &Ctx, inv: &Inventory, only: Option<u32>) -> Result<Vec<Row>> {
    let running = node::active_containers().unwrap_or_default();
    let remembered_uvm = ctx.record.uvm_majors().unwrap_or_else(|e| {
        eprintln!("pve-meta-nvidia: {e:#}; the majors written before are not known");
        Vec::new()
    });
    let mut out = Vec::new();
    for vmid in node::local_containers(&ctx.node)? {
        if only.is_some_and(|v| v != vmid) {
            continue;
        }
        let mut row = Row {
            vmid,
            name: String::new(),
            running: running.contains(&vmid),
            state: String::new(),
            gpus: Vec::new(),
            desired: Vec::new(),
            current: Vec::new(),
            notes: Vec::new(),
        };
        let text = match node::read_config(&ctx.node, vmid) {
            Ok(t) => t,
            Err(e) => {
                row.state = format!("error: {e:#}");
                out.push(row);
                continue;
            }
        };
        row.name = conf::hostname(&text).unwrap_or_default().to_string();
        let want = match doc::read(vmid) {
            Ok(w) => w,
            Err(e) => {
                row.state = format!("error: {e:#}");
                out.push(row);
                continue;
            }
        };
        if want.is_none() && !ctx.record.has(vmid) && only.is_none() {
            continue;
        }
        if let Some(l) = conf::value_of(&text, "lock") {
            row.notes.push(format!("the config is locked ({l})"));
        }
        match plan::plan(
            &text,
            want.as_ref(),
            inv,
            &remembered_uvm,
            ctx.record.has(vmid),
        ) {
            Outcome::NoDriver => row.state = "no driver".into(),
            Outcome::Unmanaged => row.state = "no gpu key".into(),
            Outcome::Refused(why) => row.state = format!("refused: {why}"),
            Outcome::Plan(p) => {
                row.state = match (p.changed(), p.desired.is_empty()) {
                    (false, true) => "no lines".into(),
                    (false, false) => "in sync".into(),
                    (true, _) => "out of sync".into(),
                };
                row.gpus = p.gpus.clone();
                row.desired = p.desired.clone();
                row.current = p.current.clone();
                row.notes.extend(p.notes.clone());
            }
        }
        out.push(row);
    }
    Ok(out)
}

/// `pve-meta-nvidia status [<vmid>]`.
///
/// An inventory that cannot be read is reported and the rows are still
/// printed: "what does this node think of my guests" is exactly the question
/// asked when something is wrong with the driver.
pub fn run(ctx: &Ctx, vmid: Option<u32>, json: bool) -> Result<()> {
    let (inv, failed) = match crate::inventory::read() {
        Ok(inv) => (inv, None),
        Err(e) => (Inventory::default(), Some(format!("{e:#}"))),
    };
    let mut rows = rows(ctx, &inv, vmid)?;
    if failed.is_some() {
        for row in &mut rows {
            if row.state == "no driver" {
                row.state = "no inventory".into();
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
        return Ok(());
    }
    match &failed {
        Some(e) => println!("the GPUs of {} could not be read: {e}", ctx.node),
        None if !inv.driver_loaded() => println!(
            "no NVIDIA driver on {} (no /proc/driver/nvidia); no guest is touched",
            ctx.node
        ),
        None => {}
    }
    if rows.is_empty() {
        println!("no guest on {} has a gpu document", ctx.node);
        return Ok(());
    }
    println!("VMID    NAME                 CT        STATE        GPUS");
    for r in &rows {
        println!(
            "{:<7} {:<20} {:<9} {:<12} {}",
            r.vmid,
            trunc(&r.name, 20),
            if r.running { "running" } else { "stopped" },
            trunc(&r.state, 12),
            if r.gpus.is_empty() {
                "-".to_string()
            } else {
                r.gpus.join(", ")
            }
        );
        if let Some(why) = r.state.strip_prefix("refused: ") {
            println!("        {why}");
        }
        for note in &r.notes {
            println!("        {note}");
        }
    }
    // One guest asked for: show the lines themselves, which is the only way
    // to see what a restart would change.
    if vmid.is_some() {
        if let Some(r) = rows.first() {
            print_lines("config", &r.current);
            if r.desired != r.current {
                print_lines("wanted", &r.desired);
                if r.running {
                    println!("        restart the container to apply");
                }
            }
        }
    }
    Ok(())
}

fn print_lines(what: &str, lines: &[String]) {
    if lines.is_empty() {
        println!("        {what}: (no managed lines)");
        return;
    }
    for (i, line) in lines.iter().enumerate() {
        println!("        {:<7} {line}", if i == 0 { what } else { "" });
    }
}

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n - 1).collect::<String>() + "…"
    }
}
