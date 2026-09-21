//! This node's `gpu` prefix file,
//! `/etc/pve/nodes/<node>/meta.d/prefixes/gpu.yaml`, written through
//! `pve-meta set nodes/<node>/prefixes/gpu`.
//!
//! Which GPUs a host has is not the cluster's to state (pve-meta decision
//! 020), so the whole schema lives in the node's own file: a guest on a node
//! without GPUs is offered no `gpu` rows at all, a guest on this one is
//! offered exactly this node's cards by UUID, with the model and bus location
//! in each row's description, and a guest that migrates gets the other node's
//! set with its document untouched. No packaged prefix exists, so there is
//! nothing to shadow and nothing to keep in step.
//!
//! The file is written only when its content would differ, because every
//! write moves pve-meta's version token and wakes every operator polling it.
//! A node whose driver is loaded and has no cards has no file; a node whose
//! driver is **not** loaded keeps whatever file it has, because a driver
//! upgrade takes `/proc/driver/nvidia` away for a few seconds and the rows
//! must not blink out of the editor for every guest on the node.

use anyhow::{bail, Result};
use serde_yaml_ng::{Mapping, Value};

use crate::cmd;
use crate::doc::{CAPABILITIES, DEFAULT_CAPABILITIES};
use crate::inventory::Inventory;
use crate::PREFIX;

/// The pve-meta document id of this node's prefix file.
pub fn doc_id(node: &str) -> String {
    format!("nodes/{node}/prefixes/{PREFIX}")
}

fn map(pairs: Vec<(&str, Value)>) -> Value {
    let mut m = Mapping::new();
    for (k, v) in pairs {
        m.insert(Value::from(k), v);
    }
    Value::Mapping(m)
}

fn boolean(default: bool, description: Option<&str>) -> Value {
    let mut pairs = vec![
        ("type", Value::from("boolean")),
        ("default", Value::from(default)),
    ];
    if let Some(d) = description {
        pairs.push(("description", Value::from(d)));
    }
    map(pairs)
}

/// The prefix file this node should have, as YAML text.
pub fn render(node: &str, inv: &Inventory) -> String {
    let devices: Vec<(&str, Value)> = inv
        .gpus
        .iter()
        .map(|g| (g.uuid.as_str(), boolean(false, Some(&g.describe()))))
        .collect();
    let devices = map(devices);
    let file = map(vec![
        (
            "description",
            Value::from(format!("NVIDIA GPUs on {node} (written by pve-meta-nvidia)")),
        ),
        ("selector", map(vec![("all", Value::from(true))])),
        (
            "schema",
            map(vec![
                ("type", Value::from("object")),
                (
                    "properties",
                    map(vec![
                        (
                            "devices",
                            map(vec![
                                ("type", Value::from("object")),
                                (
                                    "description",
                                    Value::from("GPUs this container gets, by UUID"),
                                ),
                                ("properties", devices),
                            ]),
                        ),
                        (
                            "capabilities",
                            map(vec![
                                ("type", Value::from("object")),
                                (
                                    "description",
                                    Value::from(
                                        "What the driver is used for; the default is compute and utility",
                                    ),
                                ),
                                // Every capability the parser accepts, so the
                                // rows offered and the values allowed cannot
                                // drift apart.
                                (
                                    "properties",
                                    map(CAPABILITIES
                                        .iter()
                                        .map(|c| {
                                            (*c, boolean(DEFAULT_CAPABILITIES.contains(c), None))
                                        })
                                        .collect()),
                                ),
                            ]),
                        ),
                        (
                            "require_cuda",
                            map(vec![
                                ("type", Value::from("string")),
                                (
                                    "description",
                                    Value::from(
                                        "e.g. 12.8 - the container refuses to start on an older host driver",
                                    ),
                                ),
                            ]),
                        ),
                    ]),
                ),
            ]),
        ),
    ]);
    serde_yaml_ng::to_string(&file).expect("a mapping of strings always serialises")
}

/// What [`sync`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// The file was written with this text.
    Written,
    Unchanged,
    Removed,
    Absent,
    /// The driver is not loaded, so the file was left exactly as it is.
    Kept,
}

/// What [`sync`] should do, given the inventory and the file as it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Write(String),
    Unchanged,
    Remove,
    Absent,
    Keep,
}

/// The decision, without doing anything: pure, so the one case that matters
/// is a test rather than a live driver upgrade.
///
/// **A node whose driver is not loaded keeps its file.** A driver package
/// upgrade takes `/proc/driver/nvidia` away for a few seconds; deleting the
/// prefix document there would empty the editor's rows for every guest on the
/// node and move pve-meta's version token twice, waking every operator in the
/// cluster, for a node that is about to have the same cards again. The file is
/// removed only when the driver is loaded and really has no cards.
pub fn decide(node: &str, inv: &Inventory, current: Option<&str>) -> Step {
    if inv.gpus.is_empty() {
        if !inv.driver_loaded() {
            return Step::Keep;
        }
        return match current {
            Some(_) => Step::Remove,
            None => Step::Absent,
        };
    }
    let text = render(node, inv);
    match current {
        Some(c) if same(c, &text) => Step::Unchanged,
        _ => Step::Write(text),
    }
}

/// Reads this node's prefix file through `pve-meta`, `None` when there is
/// none.
pub fn read(node: &str) -> Result<Option<String>> {
    let id = doc_id(node);
    let out = cmd::run_status("pve-meta", &["get", &id, "--format", "yaml"], cmd::META)?;
    match out.status {
        0 => Ok(Some(out.stdout)),
        2 => Ok(None),
        s => bail!("pve-meta get {id} failed (exit {s}): {}", out.stderr.trim()),
    }
}

/// Whether two prefix files say the same thing. Compared as documents, not as
/// text: key order is not meaning (pve-meta `docs/DESIGN.md` §2), and the
/// stored file may carry comment keys a read never shows.
fn same(a: &str, b: &str) -> bool {
    match (
        serde_yaml_ng::from_str::<Value>(a),
        serde_yaml_ng::from_str::<Value>(b),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Makes this node's prefix file say what the inventory found.
pub fn sync(node: &str, inv: &Inventory) -> Result<Action> {
    let id = doc_id(node);
    // Nothing is read when nothing could be written: a driverless node does
    // not even ask the store.
    if inv.gpus.is_empty() && !inv.driver_loaded() {
        return Ok(Action::Kept);
    }
    let current = read(node)?;
    match decide(node, inv, current.as_deref()) {
        Step::Keep => Ok(Action::Kept),
        Step::Absent => Ok(Action::Absent),
        Step::Unchanged => Ok(Action::Unchanged),
        Step::Remove => {
            let out = cmd::run_status("pve-meta", &["delete", &id], cmd::META)?;
            match out.status {
                0 => Ok(Action::Removed),
                2 => Ok(Action::Absent),
                s => bail!(
                    "pve-meta delete {id} failed (exit {s}): {}",
                    out.stderr.trim()
                ),
            }
        }
        // Handed over as an argument, not through a file: the document is a
        // kilobyte of generated YAML, and a root process that writes a
        // staging file at a predictable path is one symlink away from
        // truncating something else.
        Step::Write(text) => {
            cmd::run("pve-meta", &["set", &id, "--text", &text], cmd::META)?;
            Ok(Action::Written)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::Gpu;

    fn inv() -> Inventory {
        Inventory {
            driver: true,
            gpus: vec![Gpu {
                uuid: "GPU-8ddd601a-494f-9489-46c6-d23129ccad16".into(),
                minor: 3,
                bus: "0000:0a:00.0".into(),
                name: Some("Tesla T10".into()),
                memory_mib: Some(16384),
            }],
            uvm_major: Some(505),
            char_majors: [195, 505].into_iter().collect(),
            notes: Vec::new(),
        }
    }

    #[test]
    fn the_file_carries_one_row_per_card() {
        let text = render("jarvis", &inv());
        assert!(text.starts_with(
            "description: NVIDIA GPUs on jarvis (written by pve-meta-nvidia)\nselector:\n  all: true\nschema:\n"
        ));
        let v: Value = serde_yaml_ng::from_str(&text).unwrap();
        let props = &v["schema"]["properties"];
        assert_eq!(v["schema"]["type"], Value::from("object"));
        let device = &props["devices"]["properties"]["GPU-8ddd601a-494f-9489-46c6-d23129ccad16"];
        assert_eq!(device["type"], Value::from("boolean"));
        assert_eq!(device["default"], Value::from(false));
        assert_eq!(
            device["description"],
            Value::from("Tesla T10 16 GB · 0000:0a:00.0")
        );
        let caps = props["capabilities"]["properties"].as_mapping().unwrap();
        // Every capability the document parser accepts has a row, with the
        // default the parser applies.
        assert_eq!(caps.len(), crate::doc::CAPABILITIES.len());
        for c in crate::doc::CAPABILITIES {
            assert_eq!(
                caps[Value::from(c)]["default"],
                Value::from(crate::doc::DEFAULT_CAPABILITIES.contains(&c)),
                "{c}"
            );
        }
        assert_eq!(props["require_cuda"]["type"], Value::from("string"));
        // Block style, two-space indent, no quotes needed anywhere: the
        // canonical dump pve-meta stores is the text this renders.
        assert!(!text.contains('{'));
        assert!(!text.contains('"'));
    }

    #[test]
    fn a_reordered_file_is_the_same_file() {
        let text = render("jarvis", &inv());
        let mut value: Value = serde_yaml_ng::from_str(&text).unwrap();
        let m = value.as_mapping_mut().unwrap();
        let description = m.remove(Value::from("description")).unwrap();
        m.insert(Value::from("description"), description);
        assert!(same(&serde_yaml_ng::to_string(&value).unwrap(), &text));
        assert!(!same("description: other\n", &text));
        assert!(!same("{", &text));
    }

    #[test]
    fn a_driver_that_is_away_keeps_the_file() {
        let mut inv = inv();
        let text = render("jarvis", &inv);
        // The driver is being upgraded: no /proc/driver/nvidia for a few
        // seconds. The file stays, and the store is not even asked.
        inv.driver = false;
        inv.gpus.clear();
        assert_eq!(decide("jarvis", &inv, Some(&text)), Step::Keep);
        assert_eq!(decide("jarvis", &inv, None), Step::Keep);
        // A driver that is loaded and has no cards is a node with no GPUs.
        inv.driver = true;
        assert_eq!(decide("jarvis", &inv, Some(&text)), Step::Remove);
        assert_eq!(decide("jarvis", &inv, None), Step::Absent);
    }

    #[test]
    fn a_file_that_says_the_same_thing_is_not_written_again() {
        let inv = inv();
        let text = render("jarvis", &inv);
        assert_eq!(decide("jarvis", &inv, Some(&text)), Step::Unchanged);
        assert_eq!(decide("jarvis", &inv, None), Step::Write(text.clone()));
        assert_eq!(
            decide("jarvis", &inv, Some("description: something else\n")),
            Step::Write(text)
        );
    }

    #[test]
    fn the_id_is_the_node_prefix_document() {
        assert_eq!(doc_id("jarvis"), "nodes/jarvis/prefixes/gpu");
    }
}
