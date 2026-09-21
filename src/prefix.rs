//! This node's contribution to the cluster `gpu` prefix file,
//! `/etc/pve/meta.d/prefixes/gpu.yaml`, written through
//! `pve-meta merge prefixes/gpu`.
//!
//! Which GPUs a host has is still not the cluster's to state (the 025 rule:
//! a `nodes.<node>` entry replaces the top level for that node's guests, so
//! one file holds every node's cards without merging schemas). A guest on a
//! node without GPUs is offered no `gpu` rows at all, a guest on this one is
//! offered exactly this node's cards by UUID, with the model and bus location
//! in each row's description, and a guest that migrates gets the other node's
//! set with its document untouched. A merge names only this node's entry, so
//! two inventories never clobber each other; the write is skipped when the
//! entry already says the same thing, because every write moves pve-meta's
//! version token and wakes every operator polling it.
//!
//! A node whose driver is loaded and has no cards removes its entry; a node
//! whose driver is **not** loaded keeps whatever entry it has, because a
//! driver upgrade takes `/proc/driver/nvidia` away for a few seconds and the
//! rows must not blink out of the editor for every guest on the node.

use anyhow::{bail, Result};
use serde_yaml_ng::{Mapping, Value};

use crate::cmd;
use crate::doc::{CAPABILITIES, DEFAULT_CAPABILITIES};
use crate::inventory::Inventory;
use crate::PREFIX;

/// The pve-meta document id of the cluster prefix file.
pub fn doc_id() -> String {
    format!("prefixes/{PREFIX}")
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

/// The schema this node's cards describe, as a value: the `nodes.<node>.schema`
/// entry of the cluster file.
pub fn node_schema(inv: &Inventory) -> Value {
    let devices: Vec<(&str, Value)> = inv
        .gpus
        .iter()
        .map(|g| (g.uuid.as_str(), boolean(false, Some(&g.describe()))))
        .collect();
    let devices = map(devices);
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
    ])
}

/// The whole cluster file when none exists: identity, an empty top-level
/// schema showing no rows where no node reported, and this node's entry.
fn bootstrap(node: &str, inv: &Inventory) -> String {
    let file = map(vec![
        (
            "description",
            Value::from("NVIDIA GPUs by node (written by pve-meta-nvidia)"),
        ),
        ("selector", map(vec![("all", Value::from(true))])),
        ("schema", map(vec![("type", Value::from("object"))])),
        ("nodes", map(vec![(node, map(vec![("schema", node_schema(inv))]))])),
    ]);
    serde_yaml_ng::to_string(&file).expect("a mapping of strings always serialises")
}

/// A root merge patch carrying only this node's entry: recursive merge
/// keeps every other node's entry untouched.
fn merge_entry(node: &str, schema: &Value) -> String {
    let patch =
        map(vec![("nodes", map(vec![(node, map(vec![("schema", schema.clone())]))]))]);
    serde_yaml_ng::to_string(&patch).expect("a mapping of strings always serialises")
}

/// A root merge patch deleting only this node's entry (`null` deletes).
fn merge_remove(node: &str) -> String {
    let patch = map(vec![("nodes", map(vec![(node, Value::Null)]))]);
    serde_yaml_ng::to_string(&patch).expect("a mapping of strings always serialises")
}

/// This node's entry in a parsed cluster file, if it has one.
fn node_entry<'a>(cluster: &'a Value, node: &str) -> Option<&'a Value> {
    cluster.get("nodes")?.get(node)
}

/// What [`sync`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// This node's entry was merged (or the file bootstrapped with it).
    Written,
    Unchanged,
    /// This node's entry was removed from the cluster file.
    EntryRemoved,
    Absent,
    /// The driver is not loaded, so the file was left exactly as it is.
    Kept,
}

/// What [`sync`] should do, given the inventory and the cluster file.
/// Pure, so the one case that matters is a test rather than a live driver
/// upgrade; see the module docs for why a missing driver keeps the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// The file does not exist: create it whole, with this node's entry.
    Bootstrap(String),
    /// Merge this entry patch at the document root; other nodes pass through.
    Merge(String),
    Unchanged,
    RemoveEntry,
    Absent,
    Keep,
}

/// The decision, without doing anything.
///
/// **A node whose driver is not loaded keeps its entry.** A driver package
/// upgrade takes `/proc/driver/nvidia` away for a few seconds; deleting the
/// entry there would empty the editor's rows for every guest on the node and
/// move pve-meta's version token twice, waking every operator in the
/// cluster, for a node that is about to have the same cards again. The entry
/// is removed only when the driver is loaded and really has no cards.
pub fn decide(node: &str, inv: &Inventory, cluster: Option<&Value>) -> Step {
    if inv.gpus.is_empty() {
        if !inv.driver_loaded() {
            return Step::Keep;
        }
        return match cluster.and_then(|c| node_entry(c, node)) {
            Some(_) => Step::RemoveEntry,
            None => Step::Absent,
        };
    }
    let want = map(vec![("schema", node_schema(inv))]);
    match cluster {
        None => Step::Bootstrap(bootstrap(node, inv)),
        Some(c) => match node_entry(c, node) {
            Some(have) if have == &want => Step::Unchanged,
            _ => Step::Merge(merge_entry(node, &node_schema(inv))),
        },
    }
}

/// Reads the cluster prefix file through `pve-meta`, `None` when there is
/// none. A file that does not parse is an error: overwriting unknown
/// content would be rude, and merging into it would fail anyway.
pub fn read() -> Result<Option<Value>> {
    let id = doc_id();
    let out = cmd::run_status("pve-meta", &["get", &id, "--format", "yaml"], cmd::META)?;
    match out.status {
        0 => serde_yaml_ng::from_str(&out.stdout)
            .map(Some)
            .map_err(|e| anyhow::anyhow!("{id} does not parse: {e}")),
        2 => Ok(None),
        s => bail!("pve-meta get {id} failed (exit {s}): {}", out.stderr.trim()),
    }
}

/// Makes the cluster file say what the inventory found about this node.
/// Other nodes' entries pass through untouched: writes are root merges
/// naming only this node's entry, except the bootstrap of a missing file.
pub fn sync(node: &str, inv: &Inventory) -> Result<Action> {
    let id = doc_id();
    // Nothing is read when nothing could be written: a driverless node does
    // not even ask the store.
    if inv.gpus.is_empty() && !inv.driver_loaded() {
        return Ok(Action::Kept);
    }
    let cluster = read()?;
    match decide(node, inv, cluster.as_ref()) {
        Step::Keep => Ok(Action::Kept),
        Step::Absent => Ok(Action::Absent),
        Step::Unchanged => Ok(Action::Unchanged),
        Step::RemoveEntry => {
            cmd::run("pve-meta", &["merge", &id, "--text", &merge_remove(node)], cmd::META)?;
            Ok(Action::EntryRemoved)
        }
        // Handed over as an argument, not through a file: the document is a
        // kilobyte of generated YAML, and a root process that writes a
        // staging file at a predictable path is one symlink away from
        // truncating something else.
        Step::Bootstrap(text) => {
            cmd::run("pve-meta", &["set", &id, "--text", &text], cmd::META)?;
            Ok(Action::Written)
        }
        Step::Merge(text) => {
            cmd::run("pve-meta", &["merge", &id, "--text", &text], cmd::META)?;
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
    fn the_schema_carries_one_row_per_card() {
        let schema = node_schema(&inv());
        let props = &schema["properties"];
        assert_eq!(schema["type"], Value::from("object"));
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
    }

    #[test]
    fn the_bootstrap_is_a_whole_file_with_just_our_entry() {
        let text = bootstrap("jarvis", &inv());
        // Block style, no quotes needed anywhere: the canonical dump pve-meta
        // stores is the text this renders.
        assert!(!text.contains('{'));
        assert!(!text.contains('"'));
        let v: Value = serde_yaml_ng::from_str(&text).unwrap();
        assert_eq!(v["selector"]["all"], Value::from(true));
        assert_eq!(v["schema"]["type"], Value::from("object"));
        assert!(v["schema"].get("properties").is_none());
        assert_eq!(v["nodes"]["jarvis"]["schema"], node_schema(&inv()));
    }

    #[test]
    fn merge_payloads_name_only_our_entry() {
        let v: Value = serde_yaml_ng::from_str(&merge_entry("jarvis", &node_schema(&inv()))).unwrap();
        assert_eq!(v.as_mapping().unwrap().len(), 1);
        assert_eq!(v["nodes"]["jarvis"]["schema"], node_schema(&inv()));
        let v: Value = serde_yaml_ng::from_str(&merge_remove("jarvis")).unwrap();
        assert!(v["nodes"]["jarvis"].is_null());
    }

    fn entry_for(inv: &Inventory) -> Value {
        map(vec![("schema", node_schema(inv))])
    }

    fn cluster_with(entries: Vec<(&str, Value)>) -> Value {
        let mut nodes = Mapping::new();
        for (node, entry) in entries {
            nodes.insert(Value::from(node), entry);
        }
        let mut top = Mapping::new();
        top.insert(Value::from("selector"), map(vec![("all", Value::from(true))]));
        top.insert(Value::from("nodes"), Value::Mapping(nodes));
        Value::Mapping(top)
    }
    #[test]
    fn a_driver_that_is_away_keeps_the_entry() {
        let base = inv();
        let cluster = cluster_with(vec![("node1", entry_for(&base))]);
        // The driver is being upgraded: no /proc/driver/nvidia for a few
        // seconds. The entry stays, and the store is not even asked.
        let mut away = inv();
        away.driver = false;
        away.gpus.clear();
        assert_eq!(decide("node1", &away, Some(&cluster)), Step::Keep);
        assert_eq!(decide("node1", &away, None), Step::Keep);
        // A driver that is loaded and has no cards is a node with no GPUs:
        // its entry goes, other nodes' entries are not this call's business.
        let mut empty = inv();
        empty.gpus.clear();
        assert_eq!(decide("node1", &empty, Some(&cluster)), Step::RemoveEntry);
        assert_eq!(decide("node1", &empty, None), Step::Absent);
        assert_eq!(decide("node1", &empty, Some(&cluster_with(vec![]))), Step::Absent);
    }

    #[test]
    fn an_entry_that_says_the_same_thing_is_not_written_again() {
        let base = inv();
        let cluster = cluster_with(vec![
            ("node1", entry_for(&base)),
            ("node2", entry_for(&inv())),
        ]);
        assert_eq!(decide("node1", &base, Some(&cluster)), Step::Unchanged);
        // Other nodes' entries never trigger a write, however stale.
        let foreign = cluster_with(vec![("node2", entry_for(&inv()))]);
        assert!(matches!(decide("node1", &base, Some(&foreign)), Step::Merge(_)));
        // No file at all bootstraps the whole document.
        assert!(matches!(decide("node1", &base, None), Step::Bootstrap(_)));
    }

    #[test]
    fn the_id_is_the_cluster_prefix_document() {
        assert_eq!(doc_id(), "prefixes/gpu");
    }
}
