//! The CLI. Four verbs, all local: this tool only ever touches the
//! containers whose config lives on the node it runs on, because that is the
//! only node whose GPUs it can see.

use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};

use pve_meta_nvidia::cmd;
use pve_meta_nvidia::ops::{daemon, inventory, reconcile, status, Ctx};

#[derive(Parser)]
#[command(
    name = "pve-meta-nvidia",
    version,
    about = "NVIDIA GPUs for unprivileged LXC guests, from pve-meta documents"
)]
struct Cli {
    /// Echo every command run.
    #[arg(short, long, global = true)]
    verbose: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// The reconcile loop (what the systemd unit runs).
    Daemon,
    /// Read this node's GPUs and write its `gpu` prefix file.
    Inventory {
        #[arg(long)]
        json: bool,
    },
    /// Write the managed config lines of one guest, or of every guest here.
    Reconcile {
        vmid: Option<u32>,
        #[arg(long)]
        json: bool,
    },
    /// What every guest here asks for, and what its config says.
    Status {
        vmid: Option<u32>,
        #[arg(long)]
        json: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    cmd::set_verbose(cli.verbose);
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("pve-meta-nvidia: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    let ctx = Ctx::load()?;
    match cli.cmd {
        Cmd::Daemon => daemon::run(ctx, daemon::Timing::default()),
        Cmd::Inventory { json } => inventory::run(&ctx, json),
        Cmd::Reconcile { vmid, json } => reconcile::run(&ctx, vmid, json),
        Cmd::Status { vmid, json } => status::run(&ctx, vmid, json),
    }
}
