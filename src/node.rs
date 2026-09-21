//! The node this runs on: its name, the cluster's vmlist, its containers'
//! config files and whether they run.
//!
//! Everything here reads pmxcfs directly. A read error is never an absence:
//! without `/etc/pve` mounted the vmlist is not "no guests", it is "ask
//! again later" (pve-meta `docs/DESIGN.md` §7, decision 022).

use std::collections::BTreeMap;

use anyhow::{anyhow, Context, Result};
use serde_json::Value;

/// pmxcfs's vmid map: every guest in the cluster, its type and its node.
pub const VMLIST: &str = "/etc/pve/.vmlist";

/// The kind of guest a vmid is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Lxc,
    Qemu,
    Other,
}

/// One guest of the vmlist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Guest {
    pub node: String,
    pub kind: Kind,
}

/// Parses `/etc/pve/.vmlist`.
pub fn parse_vmlist(text: &str) -> Result<BTreeMap<u32, Guest>> {
    let v: Value = serde_json::from_str(text).context("the vmlist is not JSON")?;
    let ids = v
        .get("ids")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("the vmlist has no ids"))?;
    let mut out = BTreeMap::new();
    for (id, entry) in ids {
        let Ok(vmid) = id.parse::<u32>() else {
            continue;
        };
        let node = entry
            .get("node")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let kind = match entry.get("type").and_then(Value::as_str) {
            Some("lxc") => Kind::Lxc,
            Some("qemu") => Kind::Qemu,
            _ => Kind::Other,
        };
        out.insert(vmid, Guest { node, kind });
    }
    Ok(out)
}

/// Reads the vmlist.
pub fn vmlist() -> Result<BTreeMap<u32, Guest>> {
    let text = std::fs::read_to_string(VMLIST).with_context(|| format!("cannot read {VMLIST}"))?;
    parse_vmlist(&text)
}

/// The containers of `node`, in vmid order.
pub fn local_containers(node: &str) -> Result<Vec<u32>> {
    Ok(vmlist()?
        .into_iter()
        .filter(|(_, g)| g.node == node && g.kind == Kind::Lxc)
        .map(|(vmid, _)| vmid)
        .collect())
}

/// The local node's name: where `/etc/pve/local` points. Without it pmxcfs is
/// not mounted and nothing else can be read either.
pub fn nodename() -> Result<String> {
    let target = std::fs::read_link("/etc/pve/local").context("cannot read /etc/pve/local")?;
    let name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    if name.is_empty() {
        return Err(anyhow!("/etc/pve/local does not name a node"));
    }
    Ok(name.to_string())
}

/// A container's config file, on its own node.
pub fn config_path(node: &str, vmid: u32) -> String {
    format!("/etc/pve/nodes/{node}/lxc/{vmid}.conf")
}

/// Reads a container's config file.
pub fn read_config(node: &str, vmid: u32) -> Result<String> {
    let path = config_path(node, vmid);
    std::fs::read_to_string(&path).with_context(|| format!("cannot read {path}"))
}

/// The running containers in `/proc/net/unix`'s text: the test
/// `PVE::LXC::list_active_containers` makes, without a process per guest.
pub fn parse_active_containers(text: &str) -> Vec<u32> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some(path) = line.split_whitespace().next_back() else {
            continue;
        };
        let vmid = path
            .strip_prefix("@/var/lib/lxc/")
            .and_then(|p| p.strip_suffix("/command"))
            .and_then(|v| v.parse::<u32>().ok());
        if let Some(vmid) = vmid {
            if !out.contains(&vmid) {
                out.push(vmid);
            }
        }
    }
    out.sort_unstable();
    out
}

/// The containers running on this node.
pub fn active_containers() -> Result<Vec<u32>> {
    let text = std::fs::read_to_string("/proc/net/unix").context("cannot read /proc/net/unix")?;
    Ok(parse_active_containers(&text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vmlist_is_parsed() {
        let list = parse_vmlist(
            r#"{"version": 42, "ids": {
                "105": {"node": "jarvis", "type": "lxc", "version": 3},
                "106": {"node": "pve2", "type": "qemu", "version": 4},
                "x": {"node": "jarvis", "type": "lxc"}
            }}"#,
        )
        .unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(
            list[&105],
            Guest {
                node: "jarvis".into(),
                kind: Kind::Lxc
            }
        );
        assert_eq!(list[&106].kind, Kind::Qemu);
        assert!(parse_vmlist("{}").is_err());
        assert!(parse_vmlist("").is_err());
    }

    #[test]
    fn active_containers_are_parsed() {
        let text = "Num       RefCount Protocol Flags    Type St Inode Path\n\
                    0000000000000000: 00000002 00000000 00010000 0001 01 31415 @/var/lib/lxc/423/command\n\
                    0000000000000000: 00000003 00000000 00000000 0001 03 31417 /run/systemd/journal/stdout\n\
                    0000000000000000: 00000002 00000000 00010000 0001 01 31419 @/var/lib/lxc/107/monitor\n\
                    0000000000000000: 00000002 00000000 00010000 0001 01 31420\n";
        assert_eq!(parse_active_containers(text), vec![423]);
    }
}
