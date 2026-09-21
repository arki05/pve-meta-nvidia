//! What one guest's config should say. Pure over the config text, the `gpu`
//! subtree and this node's inventory, so every rule below is a unit test.
//!
//! The rules, in the order they decide:
//!
//! 1. **No driver on this node**: nothing at all. A node whose
//!    `/proc/driver/nvidia` is not there cannot give a GPU to anyone, and
//!    taking one away from every container because a driver package is
//!    mid-upgrade is the opposite of what is wanted. An inventory that could
//!    not be read never reaches here (`ops::daemon`).
//! 2. **No `gpu` key, nothing written before**: the guest is not this tool's,
//!    whatever its config contains. A container configured by hand keeps its
//!    lines; adding the key is how a human hands it over.
//! 3. **No `gpu` key, lines written before**: the key was removed. The lines
//!    go (`state`, the node-local record of what this tool wrote, is what
//!    tells the two cases apart).
//! 4. **A privileged container** is refused, whole: LXC's nvidia hook only
//!    works in a user namespace of its own and fails the start otherwise.
//! 5. **A `devN:` entry for an NVIDIA device** is refused, whole: PVE's
//!    autodev hook mknods the same nodes after the nvidia hook and the start
//!    fails. The two mechanisms are exclusive.
//! 6. Otherwise the lines are rendered for the selected GPUs that are on this
//!    node. A selected UUID that is not is skipped with a note — that is what
//!    a migration to a node with other cards looks like — and a guest that
//!    selects nothing, or nothing present, gets no lines at all.
//!
//! Nothing here starts, stops or restarts a guest: LXC reads the config when
//! the container starts, so a change applies at its next start.

use crate::conf::{self, Owned};
use crate::doc::Gpu;
use crate::inventory::Inventory;

/// What reconciling one guest would do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// This node has no working driver; nothing is touched, for any guest.
    NoDriver,
    /// No `gpu` key and nothing this tool ever wrote: not ours.
    Unmanaged,
    /// The config cannot carry the lines; nothing is touched.
    Refused(String),
    /// The managed lines as they should be, and as they are.
    Plan(Plan),
}

/// The managed lines a guest should have, beside the ones it has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub desired: Vec<String>,
    pub current: Vec<String>,
    /// Which lines of the config this plan may replace.
    pub owned: Owned,
    /// The GPUs the document asked for and this node has, by UUID.
    pub gpus: Vec<String>,
    /// One line each for something worth saying: a UUID that is not here, a
    /// UVM module that is not loaded.
    pub notes: Vec<String>,
}

impl Plan {
    pub fn changed(&self) -> bool {
        self.desired != self.current
    }
}

/// Plans one guest's config. `want` is its `gpu` subtree, `remembered_uvm` the
/// `nvidia-uvm` majors this node has written lines with (newest first; one
/// another driver holds now is dropped by [`Owned::new`]), `managed` whether
/// this node's record says lines were written for it before.
pub fn plan(
    text: &str,
    want: Option<&Gpu>,
    inv: &Inventory,
    remembered_uvm: &[u32],
    managed: bool,
) -> Outcome {
    if !inv.driver_loaded() {
        return Outcome::NoDriver;
    }
    let owned = Owned::new(inv.uvm_major, remembered_uvm, &inv.char_majors);
    let current = conf::managed_lines(text, &owned);
    let Some(want) = want else {
        if !managed {
            return Outcome::Unmanaged;
        }
        return Outcome::Plan(Plan {
            desired: Vec::new(),
            current,
            owned: owned.clone(),
            gpus: Vec::new(),
            notes: vec!["the gpu key is gone; removing the managed lines".into()],
        });
    };
    if !conf::is_unprivileged(text) {
        return Outcome::Refused(
            "privileged container: LXC's nvidia hook only works in unprivileged ones".into(),
        );
    }
    let devs = conf::nvidia_dev_entries(text);
    if !devs.is_empty() {
        return Outcome::Refused(format!(
            "{} passes an NVIDIA device through PVE's own device passthrough, \
             which cannot be combined with the hook (pct set <vmid> --delete {})",
            devs.join(", "),
            devs.join(",")
        ));
    }

    let mut notes = Vec::new();
    let mut gpus = Vec::new();
    for uuid in want.wanted() {
        match inv.by_uuid(uuid) {
            Some(gpu) => gpus.push(gpu),
            None => notes.push(format!("{uuid} is not a GPU of this node; skipped")),
        }
    }
    gpus.sort_by_key(|g| g.minor);
    if !gpus.is_empty() && inv.uvm_major.is_none() {
        notes.push(
            "nvidia-uvm is not loaded; writing no UVM rule and leaving any already there alone"
                .into(),
        );
    }
    // A major this node once wrote with, that some other driver holds now:
    // the rule in this config is not this tool's to touch any more, and
    // nothing else will ever clean it up.
    for major in remembered_uvm {
        if !owned.owns_major(*major) && conf::allow_majors(text).contains(major) {
            notes.push(format!(
                "the rule for major {major} was written by an earlier boot and now belongs to \
                 another device; it cannot be replaced and can be removed by hand"
            ));
        }
    }
    let caps = want.capability_list();
    if !gpus.is_empty() && caps.is_empty() {
        notes.push("no driver capability is on; the hook falls back to utility".into());
    }
    let desired = conf::render(&gpus, &caps, want.require_cuda.as_deref(), inv.uvm_major);
    Outcome::Plan(Plan {
        desired,
        current,
        owned,
        gpus: gpus.into_iter().map(|g| g.uuid.clone()).collect(),
        notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc;
    use crate::inventory::Gpu as Card;

    const CONF: &str = include_str!("../testdata/ct-hand-configured.conf");
    const UUID0: &str = "GPU-8ddd601a-494f-9489-46c6-d23129ccad16";
    const UUID1: &str = "GPU-1f4d0e27-1111-2222-3333-444455556666";

    fn inv() -> Inventory {
        Inventory {
            driver: true,
            gpus: vec![
                Card {
                    uuid: UUID1.into(),
                    minor: 1,
                    bus: "0000:0b:00.0".into(),
                    name: Some("Tesla T10".into()),
                    memory_mib: Some(16384),
                },
                Card {
                    uuid: UUID0.into(),
                    minor: 3,
                    bus: "0000:0a:00.0".into(),
                    name: Some("Tesla T10".into()),
                    memory_mib: Some(16384),
                },
            ],
            uvm_major: Some(505),
            char_majors: [1, 195, 505, 506].into_iter().collect(),
            notes: Vec::new(),
        }
    }

    fn plan_of(text: &str, yaml: &str) -> Plan {
        let want = doc::parse(yaml).unwrap();
        match plan(text, Some(&want), &inv(), &[], false) {
            Outcome::Plan(p) => p,
            other => panic!("expected a plan, got {other:?}"),
        }
    }

    #[test]
    fn a_guest_without_the_key_is_never_touched() {
        assert_eq!(plan(CONF, None, &inv(), &[], false), Outcome::Unmanaged);
    }

    #[test]
    fn a_removed_key_removes_the_lines() {
        let Outcome::Plan(p) = plan(CONF, None, &inv(), &[], true) else {
            panic!("expected a plan");
        };
        assert!(p.desired.is_empty());
        assert_eq!(p.current.len(), 7);
        assert!(p.changed());
    }

    #[test]
    fn a_node_without_a_driver_does_nothing_to_anyone() {
        let mut inv = inv();
        inv.driver = false;
        inv.gpus.clear();
        inv.uvm_major = None;
        let want = doc::parse(&format!("devices: {{ {UUID0}: true }}\n")).unwrap();
        assert_eq!(
            plan(CONF, Some(&want), &inv, &[505], true),
            Outcome::NoDriver
        );
        assert_eq!(plan(CONF, None, &inv, &[505], true), Outcome::NoDriver);
        // The same for an inventory nobody ever filled in.
        let empty = Inventory::default();
        assert_eq!(
            plan(CONF, Some(&want), &empty, &[], true),
            Outcome::NoDriver
        );
    }

    #[test]
    fn the_hand_configured_container_is_already_in_sync() {
        let p = plan_of(CONF, &format!("devices: {{ {UUID0}: true }}\n"));
        assert!(!p.changed());
        assert_eq!(p.gpus, [UUID0]);
        assert!(p.notes.is_empty());
    }

    #[test]
    fn selecting_nothing_removes_the_lines() {
        let p = plan_of(CONF, &format!("devices: {{ {UUID0}: false }}\n"));
        assert!(p.desired.is_empty());
        assert!(p.changed());
        assert!(p.gpus.is_empty());
    }

    #[test]
    fn a_uuid_of_another_node_is_skipped() {
        let p = plan_of(CONF, "devices: { GPU-abc123: true }\n");
        assert!(p.desired.is_empty());
        assert_eq!(p.notes.len(), 1);
        assert!(p.notes[0].contains("not a GPU of this node"));
        // The other card of this node is still attached.
        let p = plan_of(
            CONF,
            &format!("devices: {{ GPU-abc123: true, {UUID1}: true }}\n"),
        );
        assert_eq!(p.gpus, [UUID1]);
        assert!(p
            .desired
            .contains(&"lxc.cgroup2.devices.allow: c 195:1 rwm".to_string()));
    }

    #[test]
    fn gpus_are_written_in_minor_order_whatever_the_document_says() {
        let p = plan_of(
            CONF,
            &format!("devices: {{ {UUID0}: true, {UUID1}: true }}\n"),
        );
        assert_eq!(p.gpus, [UUID1, UUID0]);
        assert!(p
            .desired
            .iter()
            .any(|l| l == &format!("lxc.environment: NVIDIA_VISIBLE_DEVICES={UUID1},{UUID0}")));
    }

    #[test]
    fn a_privileged_container_is_refused_whole() {
        let text = CONF.replace("unprivileged: 1\n", "");
        match plan(
            &text,
            Some(&doc::parse(&format!("devices: {{ {UUID0}: true }}\n")).unwrap()),
            &inv(),
            &[],
            true,
        ) {
            Outcome::Refused(why) => assert!(why.contains("privileged")),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_device_passthrough_of_the_same_card_is_refused_whole() {
        let text = CONF.replace("arch: amd64\n", "arch: amd64\ndev0: /dev/nvidia0\n");
        match plan(
            &text,
            Some(&doc::parse(&format!("devices: {{ {UUID0}: true }}\n")).unwrap()),
            &inv(),
            &[],
            true,
        ) {
            Outcome::Refused(why) => {
                assert!(why.contains("dev0"));
                assert!(why.contains("--delete dev0"));
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn without_nvidia_uvm_the_rules_already_there_are_left_alone() {
        // The boot case: the driver is up, nothing has opened /dev/nvidia-uvm
        // yet, and last boot's UVM rule is in the config. It stays, and the
        // pass writes no UVM rule of its own -- the alternative is stripping
        // the rule the container needs and never putting it back.
        let mut inv = inv();
        inv.uvm_major = None;
        inv.char_majors.remove(&505);
        let want = doc::parse(&format!("devices: {{ {UUID0}: true }}\n")).unwrap();
        let Outcome::Plan(p) = plan(CONF, Some(&want), &inv, &[505], true) else {
            panic!("expected a plan");
        };
        assert!(p.notes.iter().any(|n| n.contains("nvidia-uvm")));
        assert!(!p.desired.iter().any(|l| l.contains("505")));
        assert!(!p.current.iter().any(|l| l.contains("505")));
        assert!(!p.changed());
        assert!(conf::splice(CONF, &p.desired, &p.owned).contains("c 505:0 rwm"));
    }

    #[test]
    fn a_major_that_belongs_to_something_else_now_is_named_not_deleted() {
        // 508 was this node's last boot; today /proc/devices has it for
        // something else. The rule stays and the operator is told, instead of
        // it sitting in the config for ever with nobody able to see why.
        let mut inv = inv();
        inv.uvm_major = Some(510);
        inv.char_majors.insert(508);
        inv.char_majors.insert(510);
        let text = CONF.replace("c 505:0 rwm", "c 508:0 rwm");
        let want = doc::parse(&format!("devices: {{ {UUID0}: true }}\n")).unwrap();
        let Outcome::Plan(p) = plan(&text, Some(&want), &inv, &[508, 505], true) else {
            panic!("expected a plan");
        };
        assert!(
            p.notes.iter().any(|n| n.contains("major 508")
                && n.contains("another device")
                && n.contains("by hand")),
            "{:?}",
            p.notes
        );
        let out = conf::splice(&text, &p.desired, &p.owned);
        assert!(out.contains("c 508:0 rwm"), "the rule is left alone");
    }

    #[test]
    fn a_remembered_major_is_replaced_and_a_strangers_rule_is_not() {
        let mut inv = inv();
        inv.uvm_major = Some(508);
        inv.char_majors.remove(&505);
        inv.char_majors.insert(508);
        let text = CONF.replace(
            "lxc.cgroup2.devices.allow: c 195:3 rwm\n",
            "lxc.cgroup2.devices.allow: c 195:3 rwm\nlxc.cgroup2.devices.allow: c 226:0 rwm\n",
        );
        let want = doc::parse(&format!("devices: {{ {UUID0}: true }}\n")).unwrap();
        let Outcome::Plan(p) = plan(&text, Some(&want), &inv, &[505], true) else {
            panic!("expected a plan");
        };
        assert!(p.changed());
        let out = conf::splice(&text, &p.desired, &p.owned);
        // Last boot's rule replaced, the drm rule untouched.
        assert!(!out.contains("c 505:0 rwm"));
        assert!(out.contains("c 508:0 rwm"));
        assert!(out.contains("c 226:0 rwm"));
    }
}
