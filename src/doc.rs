//! The `gpu` subtree of a guest's pve-meta document, read through the
//! `pve-meta` CLI as root on the node: no token, no pveproxy, exact YAML
//! types, and it works in the boot pass before the API is up.
//!
//! ```yaml
//! gpu:
//!   devices: { GPU-8ddd601a-494f-9489-46c6-d23129ccad16: true }
//!   capabilities: { compute: true, utility: true }
//!   require_cuda: "12.8"
//! ```
//!
//! Every key is optional; the whole subtree is the intent "this container
//! gets these GPUs". Booleans are accepted as `true`/`false` and as `1`/`0`,
//! the spelling a JSON-mode write leaves behind. Comment keys (`devices__`)
//! never reach a read (pve-meta decision 021), so nothing here knows them.

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};

use crate::cmd;
use crate::PREFIX;

/// The capabilities LXC's nvidia hook accepts (`capability_to_cli`); anything
/// else makes the hook fail the container's start, so a document naming one
/// is refused here instead.
pub const CAPABILITIES: [&str; 6] = [
    "compute", "compat32", "display", "graphics", "utility", "video",
];

/// The capabilities a document that says nothing gets.
pub const DEFAULT_CAPABILITIES: [&str; 2] = ["compute", "utility"];

/// A guest's `gpu` subtree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Gpu {
    /// GPU UUID -> wanted. A UUID that is not on this node is skipped, not an
    /// error: a document travels with its guest, the cards do not.
    pub devices: BTreeMap<String, bool>,
    /// `None` when the document says nothing, which means
    /// [`DEFAULT_CAPABILITIES`].
    pub capabilities: Option<BTreeMap<String, bool>>,
    /// `NVIDIA_REQUIRE_CUDA=cuda>=<this>`: the hook refuses to start the
    /// container on an older host driver.
    pub require_cuda: Option<String>,
}

impl Gpu {
    /// The UUIDs the document asks for, in document order.
    pub fn wanted(&self) -> Vec<&str> {
        self.devices
            .iter()
            .filter(|(_, on)| **on)
            .map(|(uuid, _)| uuid.as_str())
            .collect()
    }

    /// The capabilities to pass, in the hook's own order.
    pub fn capability_list(&self) -> Vec<&'static str> {
        let Some(set) = &self.capabilities else {
            return DEFAULT_CAPABILITIES.to_vec();
        };
        CAPABILITIES
            .iter()
            .copied()
            .filter(|c| set.get(*c).copied().unwrap_or(false))
            .collect()
    }
}

/// A `true`/`false`, `1`/`0` or `yes`/`no` value.
fn flag_of(v: &serde_yaml_ng::Value) -> Option<bool> {
    use serde_yaml_ng::Value as V;
    match v {
        V::Bool(b) => Some(*b),
        V::Number(n) => n.as_i64().and_then(|i| match i {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }),
        V::String(s) => match s.trim() {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn map_of<'a>(v: &'a serde_yaml_ng::Value, what: &str) -> Result<&'a serde_yaml_ng::Mapping> {
    v.as_mapping()
        .with_context(|| format!("gpu.{what} must be a map"))
}

/// Parses the YAML text of a `gpu` subtree. Everything that ends up in a
/// container config line is validated here, so a document can never become a
/// config key of its own: a UUID is the driver's charset, `require_cuda` is a
/// version.
pub fn parse(text: &str) -> Result<Gpu> {
    let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(text).context("gpu subtree")?;
    let root = value.as_mapping().context("gpu must be a map")?;
    let mut gpu = Gpu::default();
    for (key, value) in root {
        let key = key
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("gpu: keys must be strings"))?;
        match key {
            "devices" => {
                for (uuid, on) in map_of(value, "devices")? {
                    let uuid = uuid
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("gpu.devices: keys must be strings"))?;
                    if !is_device_id(uuid) {
                        bail!(
                            "gpu.devices: '{uuid}' is not a GPU UUID (GPU-<uuid>{})",
                            if uuid.starts_with("MIG-") {
                                "; MIG instances are not supported"
                            } else {
                                ""
                            }
                        );
                    }
                    let on = flag_of(on).with_context(|| {
                        format!("gpu.devices.{uuid}: expected a boolean (true/false or 1/0)")
                    })?;
                    gpu.devices.insert(uuid.to_string(), on);
                }
            }
            "capabilities" => {
                let mut caps = BTreeMap::new();
                for (name, on) in map_of(value, "capabilities")? {
                    let name = name
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("gpu.capabilities: keys must be strings"))?;
                    if !CAPABILITIES.contains(&name) {
                        bail!(
                            "gpu.capabilities.{name}: not a driver capability ({})",
                            CAPABILITIES.join(", ")
                        );
                    }
                    let on = flag_of(on).with_context(|| {
                        format!("gpu.capabilities.{name}: expected a boolean (true/false or 1/0)")
                    })?;
                    caps.insert(name.to_string(), on);
                }
                gpu.capabilities = Some(caps);
            }
            "require_cuda" => {
                let v = match value {
                    serde_yaml_ng::Value::String(s) => s.trim().to_string(),
                    serde_yaml_ng::Value::Number(n) => n.to_string(),
                    _ => bail!("gpu.require_cuda must be a version string, e.g. \"12.8\""),
                };
                if !is_version(&v) {
                    bail!("gpu.require_cuda: '{v}' is not a version like 12.8");
                }
                gpu.require_cuda = Some(v);
            }
            other => bail!("gpu.{other}: unknown key (devices, capabilities, require_cuda)"),
        }
    }
    Ok(gpu)
}

/// `GPU-<uuid>`, as `nvidia-container-cli --device` takes it: hex, dashes,
/// and nothing that could end a config line or a list.
///
/// `MIG-<uuid>` is deliberately not accepted. A MIG instance is not a device
/// of `/proc/driver/nvidia/gpus`, so this node could neither offer it in its
/// prefix file nor turn it into a `/dev/nvidia<N>` rule; half of the feature
/// would look like all of it. Refusing it names the limit instead.
pub fn is_device_id(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("GPU-") else {
        return false;
    };
    !rest.is_empty()
        && rest
            .chars()
            .all(|c| c.is_ascii_hexdigit() || c == '-' || c.is_ascii_digit())
}

fn is_version(s: &str) -> bool {
    !s.is_empty()
        && s.starts_with(|c: char| c.is_ascii_digit())
        && s.chars().all(|c| c.is_ascii_digit() || c == '.')
}

/// Where pve-meta keeps the documents (`docs/DESIGN.md` §2). Only ever
/// stat-ed, never parsed here: the content always comes from the `pve-meta`
/// CLI, which owns every rule about what a document means.
pub const STORE: &str = "/etc/pve/meta";

/// A document's modification stamp, `None` when there is no document.
///
/// A **hint about when to read**, never a value: everything a document says
/// still comes from `pve-meta get`. pmxcfs reports whole-second mtimes (the
/// nanoseconds are there for a store on an ordinary filesystem), so two writes
/// inside one second with the same size look like one — which is why the
/// daemon reads every document again on its slow full pass whatever the
/// stamps say.
pub fn stamp(vmid: u32) -> Result<Option<(i64, i64, u64)>> {
    use std::os::unix::fs::MetadataExt as _;
    let path = format!("{STORE}/{vmid}.yaml");
    match std::fs::metadata(&path) {
        Ok(m) => Ok(Some((m.mtime(), m.mtime_nsec(), m.size()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("cannot stat {path}")),
    }
}

/// Reads guest `vmid`'s `gpu` subtree. `Ok(None)` when the guest has no such
/// subtree (or no document at all); the guest is then never touched.
pub fn read(vmid: u32) -> Result<Option<Gpu>> {
    let out = cmd::run_status(
        "pve-meta",
        &["get", &vmid.to_string(), PREFIX, "--format", "yaml"],
        cmd::META,
    )?;
    match out.status {
        0 => {}
        2 => return Ok(None),
        s => bail!(
            "pve-meta get {vmid} {PREFIX} failed (exit {s}): {}",
            out.stderr.trim()
        ),
    }
    let gpu = parse(&out.stdout).with_context(|| format!("guest {vmid}: bad gpu document"))?;
    Ok(Some(gpu))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_document() {
        let g = parse(
            "devices: { GPU-8ddd601a-494f-9489-46c6-d23129ccad16: true, GPU-dead: 0 }\n\
             capabilities: { compute: true, utility: 1, video: false }\n\
             require_cuda: \"12.8\"\n",
        )
        .unwrap();
        assert_eq!(g.wanted(), ["GPU-8ddd601a-494f-9489-46c6-d23129ccad16"]);
        assert_eq!(g.capability_list(), ["compute", "utility"]);
        assert_eq!(g.require_cuda.as_deref(), Some("12.8"));
    }

    #[test]
    fn defaults_when_the_document_says_nothing() {
        let g = parse("devices: {}\n").unwrap();
        assert!(g.wanted().is_empty());
        assert_eq!(g.capability_list(), ["compute", "utility"]);
        assert_eq!(g.require_cuda, None);
    }

    #[test]
    fn capabilities_are_the_hooks_own_set_in_its_order() {
        let g = parse("capabilities: { video: true, compute: true, utility: false }\n").unwrap();
        assert_eq!(g.capability_list(), ["compute", "video"]);
        // All off is a document that wants none; the hook then falls back to
        // utility, and nothing is written.
        let g = parse("capabilities: { compute: false, utility: false }\n").unwrap();
        assert!(g.capability_list().is_empty());
        assert!(parse("capabilities: { cuda: true }\n").is_err());
    }

    #[test]
    fn what_is_refused() {
        assert!(parse("device: {}\n").is_err());
        assert!(parse("devices: { 'GPU-8ddd; rm -rf /': true }\n").is_err());
        assert!(parse("devices: { all: true }\n").is_err());
        // MIG instances are named, not half-supported.
        assert!(parse("devices: { MIG-GPU-8ddd601a-0-1: true }\n").is_err());
        assert!(parse("devices: { GPU-8ddd: maybe }\n").is_err());
        assert!(parse("devices: []\n").is_err());
        assert!(parse("require_cuda: 'cuda>=12.8'\n").is_err());
        assert!(parse("require_cuda: 12.8\n").is_ok());
    }
}
