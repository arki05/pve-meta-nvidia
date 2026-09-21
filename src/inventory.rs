//! What this node has: its GPUs, and the character device majors the driver
//! currently holds.
//!
//! The authoritative source is the driver's own `/proc` tree, which is there
//! whenever the driver is loaded and needs no library and no GPU wake-up:
//!
//! * `/proc/driver/nvidia/gpus/<busid>/information` — one directory per GPU,
//!   with its `GPU UUID`, its `Device Minor` (the `<N>` of `/dev/nvidia<N>`,
//!   which is not the order of the bus ids) and its `Bus Location`.
//! * `/proc/devices` — the character majors in use. `nvidia` is the
//!   registered 195; **`nvidia-uvm` is dynamic and can differ after every
//!   boot or module reload**, which is the whole reason this operator re-runs
//!   before guests start.
//!
//! **`nvidia-uvm` is loaded lazily**, by `nvidia-modprobe` when something
//! first opens `/dev/nvidia-uvm`. At boot nothing has, so a driver that is
//! perfectly fine has no UVM major yet, and a container started then would get
//! no `/dev/nvidia-uvm` and no CUDA. [`read`] therefore asks for it the way
//! the driver's own tools do — `nvidia-modprobe -u -c 0` — and reads
//! `/proc/devices` again.
//!
//! **A read either succeeds or fails.** Anything that would make the set of
//! cards or majors a guess — `/proc/devices` unreadable, the GPU directory
//! unlistable, an `information` file that is there and cannot be read or that
//! names no UUID — is an error, never a smaller inventory: reconciling against
//! a short inventory would take a GPU away from a container that has one, and
//! a card in reset is exactly when its `information` stops making sense. An
//! absent `/proc/driver/nvidia` is the one honest answer of its own: no
//! driver, and [`Inventory::driver_loaded`] says so.
//!
//! `nvidia-smi` is asked only for the human-readable name and memory size of
//! each card, because the `/proc` `Model` field says `Unknown` for several
//! datacenter cards. It is optional: without it a GPU is described by its bus
//! location alone.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::cmd;

/// One GPU of this node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Gpu {
    /// `GPU-8ddd601a-...`, the id a document selects a card by. Stable across
    /// reboots and unique in the cluster, unlike the minor or the bus id.
    pub uuid: String,
    /// The `<N>` of `/dev/nvidia<N>`.
    pub minor: u32,
    /// `0000:0a:00.0`.
    pub bus: String,
    /// From `nvidia-smi`, when it answered.
    pub name: Option<String>,
    /// Total memory in MiB, from `nvidia-smi`.
    pub memory_mib: Option<u64>,
}

impl Gpu {
    /// The one-line description the prefix file carries, e.g.
    /// `Tesla T10 16 GB · 0000:0a:00.0`.
    pub fn describe(&self) -> String {
        let mut s = String::new();
        if let Some(name) = &self.name {
            s.push_str(name);
            s.push(' ');
        }
        if let Some(mib) = self.memory_mib {
            s.push_str(&format!("{} GB ", mib.div_ceil(1024)));
        }
        s.push_str("· ");
        s.push_str(&self.bus);
        s
    }
}

/// This node's GPUs and majors, as one pass over `/proc` saw them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Inventory {
    /// `/proc/driver/nvidia` is there: the driver is loaded. A [`Default`]
    /// inventory is not an answer about any node, and says so here.
    pub driver: bool,
    /// By device minor, which is the order the rendered lines use.
    pub gpus: Vec<Gpu>,
    /// The current `nvidia-uvm` major, `None` when that module is not loaded.
    pub uvm_major: Option<u32>,
    /// Every character major `/proc/devices` lists.
    pub char_majors: BTreeSet<u32>,
    /// What was odd but not fatal, for one log line.
    pub notes: Vec<String>,
}

impl Inventory {
    /// Whether this node has a working driver. Nothing is reconciled on a node
    /// that has none: no GPU can be given out, and taking one away from every
    /// container because a driver package is mid-upgrade is the opposite of
    /// what is wanted.
    pub fn driver_loaded(&self) -> bool {
        self.driver
    }

    pub fn by_uuid(&self, uuid: &str) -> Option<&Gpu> {
        self.gpus.iter().find(|g| g.uuid == uuid)
    }
}

/// Parses one `/proc/driver/nvidia/gpus/<busid>/information`. `None` when it
/// carries no UUID or no minor, which is not a file this tool understands.
pub fn parse_information(text: &str) -> Option<Gpu> {
    let mut fields: BTreeMap<&str, &str> = BTreeMap::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        fields.insert(key.trim(), value.trim());
    }
    let uuid = fields.get("GPU UUID")?.to_string();
    let minor = fields.get("Device Minor")?.trim().parse().ok()?;
    // "Bus Location: 0000:0a:00.0" — the value was cut at the first colon.
    let bus = text
        .lines()
        .find_map(|l| l.strip_prefix("Bus Location:"))
        .map(|v| v.trim().to_string())
        .unwrap_or_default();
    Some(Gpu {
        uuid,
        minor,
        bus,
        name: None,
        memory_mib: None,
    })
}

/// The character majors of `/proc/devices`, by name.
pub fn parse_devices(text: &str) -> BTreeMap<String, u32> {
    let mut out = BTreeMap::new();
    let mut in_chars = false;
    for line in text.lines() {
        let t = line.trim();
        if t.ends_with("devices:") {
            in_chars = t.eq_ignore_ascii_case("Character devices:");
            continue;
        }
        if !in_chars || t.is_empty() {
            continue;
        }
        if let Some((major, name)) = t.split_once(char::is_whitespace) {
            if let Ok(major) = major.trim().parse::<u32>() {
                out.insert(name.trim().to_string(), major);
            }
        }
    }
    out
}

/// Parses `nvidia-smi --query-gpu=uuid,name,memory.total,pci.bus_id
/// --format=csv,noheader`: `uuid, name, 16384 MiB, 00000000:0A:00.0`. The
/// fields are taken from both ends, because a model name may itself contain a
/// comma and nvidia-smi quotes nothing.
pub fn parse_smi(text: &str) -> BTreeMap<String, (String, Option<u64>)> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split(',').map(str::trim).collect();
        if f.len() < 4 || !f[0].starts_with("GPU-") {
            continue;
        }
        let mib = f[f.len() - 2]
            .split_whitespace()
            .next()
            .and_then(|n| n.parse::<u64>().ok());
        let name = f[1..f.len() - 2].join(", ");
        out.insert(f[0].to_string(), (name, mib));
    }
    out
}

/// Where the driver publishes its GPUs.
const GPUS_DIR: &str = "/proc/driver/nvidia/gpus";
const DEVICES: &str = "/proc/devices";
/// The driver's own tool for loading `nvidia-uvm` and making its device nodes.
const MODPROBE: &str = "/usr/bin/nvidia-modprobe";

/// Reads this node's inventory, loading `nvidia-uvm` if the driver is there
/// and that module is not.
pub fn read() -> Result<Inventory> {
    let (gpus, devices) = (Path::new(GPUS_DIR), Path::new(DEVICES));
    // The first read does not name the cards: when nvidia-uvm has to be
    // loaded it is thrown away, and nvidia-smi is the one expensive call
    // here.
    let inv = read_from(gpus, devices)?;
    if !inv.driver || inv.uvm_major.is_some() {
        return Ok(name_cards(inv));
    }
    let asked = load_uvm();
    let mut inv = name_cards(read_from(gpus, devices)?);
    if inv.uvm_major.is_none() {
        inv.notes.push(match asked {
            Ok(()) => format!(
                "nvidia-uvm is not loaded ({MODPROBE} -u -c 0 did not load it); \
                 containers get no /dev/nvidia-uvm and no CUDA"
            ),
            Err(e) => format!("nvidia-uvm is not loaded and {e}"),
        });
    }
    Ok(inv)
}

/// `nvidia-modprobe -u -c 0`: loads `nvidia-uvm` and creates its device
/// nodes, which is what the driver's own container tooling runs.
fn load_uvm() -> std::result::Result<(), String> {
    match cmd::run_status(MODPROBE, &["-u", "-c", "0"], cmd::MODPROBE) {
        Ok(o) if o.status == 0 => Ok(()),
        Ok(o) => Err(format!(
            "{MODPROBE} exited {}: {}",
            o.status,
            o.stderr.trim()
        )),
        Err(e) => Err(format!("{e:#}")),
    }
}

/// Fills in the names, once, for an inventory that is being kept.
fn name_cards(mut inv: Inventory) -> Inventory {
    if !inv.gpus.is_empty() {
        name_gpus(&mut inv);
    }
    inv
}

fn read_from(gpus_dir: &Path, devices: &Path) -> Result<Inventory> {
    // Without the majors nothing below can be decided, so this is an error and
    // never an empty answer.
    let text = std::fs::read_to_string(devices)
        .with_context(|| format!("cannot read {}", devices.display()))?;
    let majors = parse_devices(&text);
    let mut inv = Inventory {
        driver: false,
        uvm_major: majors.get("nvidia-uvm").copied(),
        char_majors: majors.into_values().collect(),
        ..Inventory::default()
    };
    let entries = match std::fs::read_dir(gpus_dir) {
        Ok(e) => e,
        // No driver on this node. The majors stay: they are the node's, not
        // the driver's, and the ownership rules read them.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(inv),
        Err(e) => return Err(e).with_context(|| format!("cannot list {}", gpus_dir.display())),
    };
    inv.driver = true;
    for entry in entries {
        let entry = entry.with_context(|| format!("cannot list {}", gpus_dir.display()))?;
        let path = entry.path().join("information");
        match std::fs::read_to_string(&path) {
            Ok(text) => match parse_information(&text) {
                Some(gpu) => inv.gpus.push(gpu),
                // The driver published this card and will not say which it
                // is -- a GPU in reset, or a field this parser does not know.
                // Dropping it would shorten the card list, and the next pass
                // would take that card away from the container that has it.
                None => {
                    bail!("{}: the driver names no GPU UUID here", path.display())
                }
            },
            // A directory without one at all is not a GPU's.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => inv
                .notes
                .push(format!("{}: no information file", path.display())),
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
        }
    }
    inv.gpus.sort_by_key(|g| g.minor);
    Ok(inv)
}

/// Fills in names and memory sizes from `nvidia-smi`, if it runs.
fn name_gpus(inv: &mut Inventory) {
    let out = cmd::run_status(
        "nvidia-smi",
        &[
            "--query-gpu=uuid,name,memory.total,pci.bus_id",
            "--format=csv,noheader",
        ],
        cmd::SMI,
    );
    let text = match out {
        Ok(o) if o.status == 0 => o.stdout,
        Ok(o) => {
            inv.notes.push(format!(
                "nvidia-smi exited {}: {}",
                o.status,
                o.stderr.trim()
            ));
            return;
        }
        Err(e) => {
            inv.notes.push(format!("nvidia-smi: {e:#}"));
            return;
        }
    };
    let named = parse_smi(&text);
    for gpu in &mut inv.gpus {
        if let Some((name, mib)) = named.get(&gpu.uuid) {
            gpu.name = Some(name.clone());
            gpu.memory_mib = *mib;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn information_of_a_card_whose_model_is_unknown() {
        let gpu = parse_information(include_str!("../testdata/information-gpu0.txt")).unwrap();
        assert_eq!(gpu.uuid, "GPU-8ddd601a-494f-9489-46c6-d23129ccad16");
        assert_eq!(gpu.minor, 3);
        assert_eq!(gpu.bus, "0000:0a:00.0");
        assert_eq!(gpu.name, None);
        assert_eq!(gpu.describe(), "· 0000:0a:00.0");
    }

    #[test]
    fn information_without_a_uuid_is_not_a_gpu() {
        assert!(parse_information("Model: Unknown\nIRQ: 66\n").is_none());
        assert!(parse_information("GPU UUID: GPU-x\n").is_none());
    }

    #[test]
    fn the_uvm_major_comes_from_proc_devices() {
        let majors = parse_devices(include_str!("../testdata/devices.txt"));
        assert_eq!(majors.get("nvidia"), Some(&195));
        assert_eq!(majors.get("nvidia-uvm"), Some(&505));
        // The block section is not read: 252 is device-mapper's, not a
        // character major.
        assert_eq!(majors.get("device-mapper"), None);
        assert_eq!(majors.get("sd"), None);
        assert!(parse_devices("Character devices:\n").is_empty());
    }

    #[test]
    fn smi_names_the_cards() {
        let named = parse_smi(include_str!("../testdata/nvidia-smi.csv"));
        let (name, mib) = &named["GPU-8ddd601a-494f-9489-46c6-d23129ccad16"];
        assert_eq!(name, "Tesla T10");
        assert_eq!(*mib, Some(16384));
        // A model name with a comma in it: the fields are taken from the ends.
        let named = parse_smi("GPU-x, NVIDIA RTX A4000, Ada, 16376 MiB, 00000000:0A:00.0\n");
        assert_eq!(
            named["GPU-x"],
            ("NVIDIA RTX A4000, Ada".to_string(), Some(16376))
        );
        let gpu = Gpu {
            uuid: "GPU-8ddd601a-494f-9489-46c6-d23129ccad16".into(),
            minor: 3,
            bus: "0000:0a:00.0".into(),
            name: Some(name.clone()),
            memory_mib: *mib,
        };
        assert_eq!(gpu.describe(), "Tesla T10 16 GB · 0000:0a:00.0");
    }

    /// A tree with two cards, one entry that is not a GPU's, and one
    /// `/proc/devices`.
    fn fixture_tree(name: &str, devices_text: &str) -> (std::path::PathBuf, Inventory) {
        let dir = std::env::temp_dir().join(format!(
            "pve-meta-nvidia-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let gpus = dir.join("gpus");
        for (bus, text) in [
            (
                "0000:0a:00.0",
                include_str!("../testdata/information-gpu0.txt"),
            ),
            (
                "0000:0b:00.0",
                include_str!("../testdata/information-gpu1.txt"),
            ),
        ] {
            std::fs::create_dir_all(gpus.join(bus)).unwrap();
            std::fs::write(gpus.join(bus).join("information"), text).unwrap();
        }
        std::fs::create_dir_all(gpus.join("stray")).unwrap();
        let devices = dir.join("devices");
        std::fs::write(&devices, devices_text).unwrap();
        let inv = read_from(&gpus, &devices).unwrap();
        (dir, inv)
    }

    #[test]
    fn a_whole_proc_tree_is_read_in_minor_order() {
        let (dir, inv) = fixture_tree("proc", include_str!("../testdata/devices.txt"));
        assert!(inv.driver_loaded());
        assert_eq!(inv.uvm_major, Some(505));
        assert!(inv.char_majors.contains(&195));
        // Sorted by minor, which is not the order of the bus ids.
        assert_eq!(
            inv.gpus
                .iter()
                .map(|g| (g.minor, g.bus.as_str()))
                .collect::<Vec<_>>(),
            [(1, "0000:0b:00.0"), (3, "0000:0a:00.0")]
        );
        assert_eq!(
            inv.by_uuid("GPU-8ddd601a-494f-9489-46c6-d23129ccad16")
                .unwrap()
                .minor,
            3
        );
        assert!(inv.by_uuid("GPU-nothing").is_none());
        assert!(inv.notes.iter().any(|n| n.contains("stray")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_driver_can_be_loaded_without_nvidia_uvm() {
        let devices = include_str!("../testdata/devices.txt").replace("505 nvidia-uvm\n", "");
        let (dir, inv) = fixture_tree("nouvm", &devices);
        assert!(inv.driver_loaded());
        assert_eq!(inv.uvm_major, None);
        assert_eq!(inv.gpus.len(), 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_card_the_driver_will_not_name_is_an_error() {
        // A GPU in reset publishes its directory and an information file that
        // says nothing useful. Dropping it would shorten the card list, and
        // the next pass would take that card off the container that has it.
        let (dir, _) = fixture_tree("noname", include_str!("../testdata/devices.txt"));
        std::fs::write(
            dir.join("gpus/0000:0a:00.0/information"),
            "Model: \t Unknown\nIRQ: \t 66\n",
        )
        .unwrap();
        let err = read_from(&dir.join("gpus"), &dir.join("devices"))
            .expect_err("a card without a UUID is an error");
        assert!(err.to_string().contains("no GPU UUID"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_node_without_the_driver_keeps_its_majors() {
        let (dir, _) = fixture_tree("nodriver", include_str!("../testdata/devices.txt"));
        let inv = read_from(&dir.join("nothing-here"), &dir.join("devices")).unwrap();
        assert!(!inv.driver_loaded());
        assert!(inv.gpus.is_empty());
        assert!(inv.char_majors.contains(&195));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_unreadable_proc_is_an_error_and_never_an_empty_inventory() {
        let missing = Path::new("/nonexistent/proc/devices");
        assert!(read_from(Path::new("/nonexistent/gpus"), missing).is_err());
    }
}
