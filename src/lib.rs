//! pve-meta-nvidia: NVIDIA GPUs for unprivileged LXC guests, described by the
//! `gpu` subtree of the guest's pve-meta document and reconciled from the node
//! the guest runs on.
//!
//! Nothing is installed inside a guest: LXC's stock `nvidia` hook
//! (`lxc-pve`, which calls `nvidia-container-cli`) bind-mounts the host's
//! driver into the container at start. This operator only writes the handful
//! of config lines that hook needs, and keeps them true for this node's
//! hardware.
//!
//! The layering, top down:
//!
//! * `ops`: the verbs: inventory, reconcile, status, daemon. Each is a
//!   function over the types below.
//! * `plan`: what a guest's config should say, given its document and this
//!   node's GPUs. Pure over the config text, a [`doc::Gpu`] and an
//!   [`inventory::Inventory`]; every rule that decides what is written lives
//!   here or in `conf`.
//! * `conf`: the container config as text: which lines this tool owns, and
//!   splicing a new set of them into the main section.
//! * `doc`: the `gpu` subtree of the guest's pve-meta document.
//! * `prefix`: this node's `gpu` prefix file, rendered from the inventory and
//!   written through `pve-meta`.
//! * `inventory`: the node's GPUs and device majors, from `/proc` and
//!   `nvidia-smi`.
//! * `node`, `state`, `lock`, `cmd`, `notify`: the vmlist and container
//!   facts, the record of which guests this tool wrote lines into, PVE's
//!   config lock and atomic write, running the node's own tools under a
//!   deadline, and what systemd is told about all of it.
//!
//! Nothing about what was applied goes into the store: a document is intent
//! (pve-meta `docs/DESIGN.md` §2), and the config file is the applied state.

pub mod cmd;
pub mod conf;
pub mod doc;
pub mod inventory;
pub mod lock;
pub mod node;
pub mod notify;
pub mod ops;
pub mod plan;
pub mod prefix;
pub mod state;

/// The pve-meta prefix this operator reads and writes: the top-level key of a
/// guest document, and the name of this node's prefix file.
pub const PREFIX: &str = "gpu";

/// LXC's own hook, shipped by `lxc-pve`. It calls `nvidia-container-cli
/// --user configure --no-cgroups`, which bind-mounts the host driver's
/// libraries and binaries into the container's rootfs, creates the device
/// nodes there and runs the host's `ldconfig` on it.
pub const HOOK: &str = "/usr/share/lxc/hooks/nvidia";

/// The character major of `/dev/nvidia<N>`, `/dev/nvidiactl` (minor 255) and
/// `/dev/nvidia-modeset` (minor 254). Fixed and registered; only the UVM
/// major is dynamic.
pub const NVIDIA_MAJOR: u32 = 195;
/// `/dev/nvidiactl`, the control device every user of the driver opens.
pub const CTL_MINOR: u32 = 255;
/// `/dev/nvidia-modeset`, needed only by the graphics and display
/// capabilities.
pub const MODESET_MINOR: u32 = 254;
