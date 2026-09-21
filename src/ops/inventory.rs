//! `inventory`: what this node has, and this node's `gpu` prefix file.
//!
//! The two halves fail apart on purpose. Reading the GPUs is what every
//! reconcile depends on; publishing the prefix file is a courtesy to the
//! editor. A prefix file that could not be written must not discard a good
//! inventory — that would turn "pmxcfs hiccup" into "no node has GPUs", which
//! is the answer that takes cards away from running containers.

use anyhow::Result;

use crate::inventory::{self, Inventory};
use crate::ops::Ctx;
use crate::prefix::{self, Action};

/// Publishes `inv` as this node's prefix file.
pub fn publish(ctx: &Ctx, inv: &Inventory) -> Result<Action> {
    prefix::sync(&ctx.node, inv)
}

pub fn action_word(action: &Action) -> &'static str {
    match action {
        Action::Written => "written",
        Action::Unchanged => "unchanged",
        Action::EntryRemoved => "entry removed",
        Action::Absent => "not there",
        Action::Kept => "kept (the driver is not loaded)",
    }
}

/// `pve-meta-nvidia inventory`.
pub fn run(ctx: &Ctx, json: bool) -> Result<()> {
    let inv = inventory::read()?;
    let action = publish(ctx, &inv);
    if json {
        let action = match &action {
            Ok(a) => serde_json::json!(action_word(a)),
            Err(e) => serde_json::json!({ "error": format!("{e:#}") }),
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "node": ctx.node,
                "driver": inv.driver_loaded(),
                "gpus": inv.gpus,
                "uvm_major": inv.uvm_major,
                "notes": inv.notes,
                "prefix": { "id": prefix::doc_id(), "action": action },
            }))?
        );
        return Ok(());
    }
    for note in &inv.notes {
        eprintln!("pve-meta-nvidia: {note}");
    }
    if !inv.driver_loaded() {
        println!("no NVIDIA driver on {} (no /proc/driver/nvidia)", ctx.node);
    } else {
        println!("MINOR  UUID                                          GPU");
        for gpu in &inv.gpus {
            println!("{:<6} {:<45} {}", gpu.minor, gpu.uuid, gpu.describe());
        }
        println!(
            "nvidia-uvm major: {}",
            match inv.uvm_major {
                Some(m) => m.to_string(),
                None => "not loaded".into(),
            }
        );
    }
    println!("{}: {}", prefix::doc_id(), action_word(&action?));
    Ok(())
}
