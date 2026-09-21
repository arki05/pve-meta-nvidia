//! A container config as text: which of its lines this tool owns, and
//! splicing a new set of them in.
//!
//! The file is `/etc/pve/nodes/<node>/lxc/<vmid>.conf`, PVE's own format: a
//! main section, then `[pve:pending]` and one section per snapshot, each
//! introduced by a `[name]` line (`PVE::LXC::Config::parse_pct_config`).
//! **Only the main section is ever read or written here**: a snapshot section
//! is a past config, and rewriting it would rewrite history.
//!
//! The managed lines, and nothing else, are:
//!
//! ```text
//! lxc.cgroup2.devices.allow: c 195:<minor> rwm   one per selected GPU
//! lxc.cgroup2.devices.allow: c 195:255 rwm       /dev/nvidiactl
//! lxc.cgroup2.devices.allow: c <uvm>:0 rwm       /dev/nvidia-uvm
//! lxc.cgroup2.devices.allow: c <uvm>:1 rwm       /dev/nvidia-uvm-tools
//! lxc.hook.mount: /usr/share/lxc/hooks/nvidia
//! lxc.environment: NVIDIA_*
//! ```
//!
//! Ownership is decided per line, never by position ([`Owned`]): the hook
//! line, every `lxc.environment` line naming an `NVIDIA_` variable, and every
//! `lxc.cgroup2.devices.allow` line for major 195, for the current
//! `nvidia-uvm` major, or for a major this node has written lines with and
//! that no other driver has taken over since. A
//! guest whose document has no `gpu` key is never looked at this way (see
//! `plan`), so a container configured by hand keeps its lines.
//!
//! A line is read the way PVE reads it: a raw `lxc.` key may be spelled
//! `key: value` or `key = value`, and both forms are the same line. A form
//! this tool did not recognise would be a line it never replaces and never
//! removes — a second hook line beside the one already there, which is the
//! double-`mknod` failure it refuses `devN:` over.

use std::collections::BTreeSet;

use crate::{CTL_MINOR, HOOK, MODESET_MINOR, NVIDIA_MAJOR};

/// The key of a cgroup device allow line.
pub const ALLOW: &str = "lxc.cgroup2.devices.allow";
/// The key of the mount hook line.
pub const HOOK_KEY: &str = "lxc.hook.mount";
/// The key of an environment line.
pub const ENV_KEY: &str = "lxc.environment";

/// Which device majors' allow lines this tool owns in a guest's config.
///
/// 195 is the driver's registered major and is always ours. `nvidia-uvm`'s is
/// dynamic, so the majors this node has written lines with are ours too: they,
/// and nothing else, are how an earlier boot's rule is replaced instead of
/// piling up. **A major nobody here ever used is not ours** — an allow line
/// for a device whose module happens to be out (a Coral TPU, say) is
/// somebody's rule, and guessing from absence would delete it for good. A
/// handful of them are remembered rather than one, because a guest that
/// missed the pass where the major changed still carries an older one.
///
/// When `nvidia-uvm` is not loaded at all, no UVM line is owned, not even a
/// remembered one: a rule this pass cannot replace is a rule it leaves alone
/// (the module is loaded lazily, so "absent" mostly means "not yet").
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Owned {
    uvm: Option<u32>,
    remembered: Vec<u32>,
}

impl Owned {
    /// From the inventory this pass read and the majors this node has used.
    ///
    /// `live` is every character major `/proc/devices` lists now. A
    /// remembered major that some driver holds today, and that is not the
    /// current `nvidia-uvm` major, **is not ours**: dynamic majors are
    /// reused, and the one this node wrote in March can be a Coral TPU's or a
    /// v4l2loopback's by June. Remembering a handful of them is what keeps an
    /// old rule replaceable; this is what keeps that from reaching a rule
    /// somebody else's device needs.
    pub fn new(uvm: Option<u32>, remembered: &[u32], live: &BTreeSet<u32>) -> Owned {
        Owned {
            uvm,
            remembered: match uvm {
                Some(uvm) => remembered
                    .iter()
                    .copied()
                    .filter(|m| *m == uvm || !live.contains(m))
                    .collect(),
                None => Vec::new(),
            },
        }
    }

    fn owns(&self, major: u32) -> bool {
        major == NVIDIA_MAJOR || Some(major) == self.uvm || self.remembered.contains(&major)
    }
}

/// Splits a config into its main section and everything from the first
/// `[section]` line on, which is returned verbatim.
pub fn split_sections(text: &str) -> (&str, &str) {
    if text.starts_with('[') {
        return ("", text);
    }
    match text.find("\n[") {
        Some(at) => text.split_at(at + 1),
        None => (text, ""),
    }
}

/// One `key: value` of a config line, PVE's own shape: a raw `lxc.` key is
/// `^(lxc\.[a-z0-9_\-\.]+)(:|\s*=)\s*(.*?)\s*$`, everything else is
/// `^([a-z][a-z_]*\d*):\s*(.+?)\s*$`.
pub fn split_line(line: &str) -> Option<(&str, &str)> {
    let line = line.trim_end();
    if line.starts_with('#') || line.trim().is_empty() {
        return None;
    }
    let end = line.find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')))?;
    let (key, rest) = line.split_at(end);
    if key.is_empty() {
        return None;
    }
    let value = match rest.strip_prefix(':') {
        Some(v) => v,
        // `lxc.key = value` is the other spelling PVE accepts.
        None if key.starts_with("lxc.") => rest.trim_start().strip_prefix('=')?,
        None => return None,
    };
    Some((key, value.trim()))
}

/// A parsed `c <major>:<minor> rwm` value; `None` for a `major`/`minor` that
/// is a `*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Device {
    pub major: Option<u32>,
    pub minor: Option<u32>,
}

/// Parses the value of a `lxc.cgroup2.devices.allow` line. Only character
/// devices are ours; a block device line is nothing this tool writes.
pub fn parse_allow(value: &str) -> Option<Device> {
    let mut parts = value.split_whitespace();
    if parts.next()? != "c" {
        return None;
    }
    let (major, minor) = parts.next()?.split_once(':')?;
    let num = |s: &str| {
        if s == "*" {
            Some(None)
        } else {
            s.parse::<u32>().ok().map(Some)
        }
    };
    Some(Device {
        major: num(major)?,
        minor: num(minor)?,
    })
}

/// Whether this tool owns `line`.
pub fn is_managed(line: &str, owned: &Owned) -> bool {
    let Some((key, value)) = split_line(line) else {
        return false;
    };
    match key {
        HOOK_KEY => value == HOOK,
        ENV_KEY => value.starts_with("NVIDIA_"),
        ALLOW => match parse_allow(value) {
            // `c *:* rwm` is somebody else's blanket rule.
            Some(dev) => dev.major.is_some_and(|m| owned.owns(m)),
            None => false,
        },
        _ => false,
    }
}

/// The managed lines of the main section, in file order.
pub fn managed_lines(text: &str, owned: &Owned) -> Vec<String> {
    let (main, _) = split_sections(text);
    main.lines()
        .filter(|l| is_managed(l, owned))
        .map(|l| l.trim_end().to_string())
        .collect()
}

/// Every character major an allow line of the main section names.
pub fn allow_majors(text: &str) -> BTreeSet<u32> {
    let (main, _) = split_sections(text);
    main.lines()
        .filter_map(split_line)
        .filter(|(key, _)| *key == ALLOW)
        .filter_map(|(_, value)| parse_allow(value)?.major)
        .collect()
}

impl Owned {
    /// Whether this tool would replace an allow line for `major`.
    pub fn owns_major(&self, major: u32) -> bool {
        self.owns(major)
    }
}

/// A value of the main section, e.g. `hostname` or `unprivileged`.
pub fn value_of<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let (main, _) = split_sections(text);
    main.lines()
        .find_map(|l| split_line(l).filter(|(k, _)| *k == key).map(|(_, v)| v))
}

/// Whether the container runs unprivileged, which is the only kind LXC's
/// nvidia hook supports: it refuses anything but a user namespace whose root
/// is not the host's (`in_userns`). PVE's own test is the `unprivileged`
/// flag or a custom `lxc.idmap`.
pub fn is_unprivileged(text: &str) -> bool {
    if value_of(text, "unprivileged") == Some("1") {
        return true;
    }
    let (main, _) = split_sections(text);
    main.lines()
        .any(|l| matches!(split_line(l), Some(("lxc.idmap", _))))
}

/// The `devN:` entries that pass an NVIDIA device through PVE's own device
/// passthrough. They cannot be combined with the hook: PVE's
/// `lxc-pve-autodev-hook` mknods them after the nvidia hook has already
/// created the same nodes, and the start fails with `Could not mknod
/// .../dev/nvidia-uvm: File exists`.
pub fn nvidia_dev_entries(text: &str) -> Vec<String> {
    let (main, _) = split_sections(text);
    main.lines()
        .filter_map(split_line)
        .filter(|(key, value)| {
            key.strip_prefix("dev")
                .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
                && value.contains("/dev/nvidia")
        })
        .map(|(key, _)| key.to_string())
        .collect()
}

/// The container's hostname, for a status table.
pub fn hostname(text: &str) -> Option<&str> {
    value_of(text, "hostname")
}

/// Renders the managed lines for the selected GPUs.
///
/// `gpus` are this node's cards the document asked for, in the order they are
/// written; `caps` the driver capabilities; `require_cuda` the constraint the
/// hook turns into `--require=cuda>=<v>`; `uvm` the current `nvidia-uvm`
/// major, when the module is loaded. An empty `gpus` renders nothing: no GPU,
/// no lines, and the container starts as an ordinary one.
pub fn render(
    gpus: &[&crate::inventory::Gpu],
    caps: &[&str],
    require_cuda: Option<&str>,
    uvm: Option<u32>,
) -> Vec<String> {
    if gpus.is_empty() {
        return Vec::new();
    }
    let allow = |major: u32, minor: u32| format!("{ALLOW}: c {major}:{minor} rwm");
    let mut out = Vec::new();
    for gpu in gpus {
        out.push(allow(NVIDIA_MAJOR, gpu.minor));
    }
    // /dev/nvidia-modeset, which the graphics and display capabilities need
    // and nothing else does.
    if caps.iter().any(|c| *c == "graphics" || *c == "display") {
        out.push(allow(NVIDIA_MAJOR, MODESET_MINOR));
    }
    out.push(allow(NVIDIA_MAJOR, CTL_MINOR));
    if let Some(uvm) = uvm {
        out.push(allow(uvm, 0));
        out.push(allow(uvm, 1));
    }
    out.push(format!("{HOOK_KEY}: {HOOK}"));
    let uuids: Vec<&str> = gpus.iter().map(|g| g.uuid.as_str()).collect();
    out.push(format!(
        "{ENV_KEY}: NVIDIA_VISIBLE_DEVICES={}",
        uuids.join(",")
    ));
    if !caps.is_empty() {
        out.push(format!(
            "{ENV_KEY}: NVIDIA_DRIVER_CAPABILITIES={}",
            caps.join(",")
        ));
    }
    if let Some(v) = require_cuda {
        out.push(format!("{ENV_KEY}: NVIDIA_REQUIRE_CUDA=cuda>={v}"));
    }
    out
}

/// Replaces the managed lines of the main section with `desired`, keeping
/// every other line and every section below untouched. The new lines land
/// where the first managed line stood, or at the end of the main section.
///
/// Lines are rejoined with `\n`. A config written with CRLF — nothing PVE
/// does — keeps its own line endings on the lines that are kept and gets LF
/// on the managed ones; both parse, here and in PVE, and the mixture is
/// cosmetic.
pub fn splice(text: &str, desired: &[String], owned: &Owned) -> String {
    let (main, rest) = split_sections(text);
    let mut out: Vec<&str> = Vec::new();
    let mut at = None;
    for line in main.lines() {
        if is_managed(line, owned) {
            at.get_or_insert(out.len());
            continue;
        }
        out.push(line);
    }
    // Trailing blank lines are the separator before a section; the managed
    // lines belong above them.
    while out.last().is_some_and(|l| l.trim().is_empty()) {
        out.pop();
    }
    let mut lines: Vec<String> = out.into_iter().map(str::to_string).collect();
    let at = at.unwrap_or(lines.len()).min(lines.len());
    for (i, line) in desired.iter().enumerate() {
        lines.insert(at + i, line.clone());
    }
    let mut text = String::new();
    for line in lines {
        text.push_str(&line);
        text.push('\n');
    }
    if !rest.is_empty() {
        text.push('\n');
        text.push_str(rest);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::Gpu;

    const CONF: &str = include_str!("../testdata/ct-hand-configured.conf");

    /// The majors a plain node has: 195 and the current UVM one, never the
    /// ones this node used in an earlier boot.
    fn live() -> BTreeSet<u32> {
        [1, 4, 5, 10, 13, 195, 234, 506].into_iter().collect()
    }

    fn cards() -> Vec<Gpu> {
        vec![
            Gpu {
                uuid: "GPU-8ddd601a-494f-9489-46c6-d23129ccad16".into(),
                minor: 3,
                bus: "0000:0a:00.0".into(),
                name: Some("Tesla T10".into()),
                memory_mib: Some(16384),
            },
            Gpu {
                uuid: "GPU-1f4d0e27-1111-2222-3333-444455556666".into(),
                minor: 1,
                bus: "0000:0b:00.0".into(),
                name: Some("Tesla T10".into()),
                memory_mib: Some(16384),
            },
        ]
    }

    #[test]
    fn sections_are_split_at_the_first_header() {
        let (main, rest) = split_sections(CONF);
        assert!(main.contains("hostname: llm"));
        assert!(!main.contains("[before-gpu]"));
        assert!(rest.starts_with("[before-gpu]"));
        assert_eq!(split_sections("a: 1\n").1, "");
        assert_eq!(split_sections("[snap]\na: 1\n"), ("", "[snap]\na: 1\n"));
        // A file written with CRLF keeps its sections apart all the same.
        let (main, rest) = split_sections("arch: amd64\r\n\r\n[snap]\r\narch: amd64\r\n");
        assert_eq!(main, "arch: amd64\r\n\r\n");
        assert!(rest.starts_with("[snap]"));
    }

    #[test]
    fn a_line_is_read_the_way_pve_reads_it() {
        assert_eq!(split_line("hostname: llm"), Some(("hostname", "llm")));
        assert_eq!(
            split_line("lxc.hook.mount: /usr/share/lxc/hooks/nvidia"),
            Some((HOOK_KEY, HOOK))
        );
        // The `=` spelling, which PVE accepts for raw lxc keys.
        assert_eq!(
            split_line("lxc.hook.mount = /usr/share/lxc/hooks/nvidia"),
            Some((HOOK_KEY, HOOK))
        );
        assert_eq!(
            split_line("lxc.hook.mount=/usr/share/lxc/hooks/nvidia"),
            Some((HOOK_KEY, HOOK))
        );
        assert_eq!(
            split_line("lxc.cgroup2.devices.allow: c 195:3 rwm\r"),
            Some((ALLOW, "c 195:3 rwm"))
        );
        assert_eq!(split_line("#a note"), None);
        assert_eq!(split_line("   "), None);
        assert_eq!(split_line("no-delimiter"), None);
        // Only raw lxc keys have the `=` spelling.
        assert_eq!(split_line("hostname = llm"), None);
    }

    #[test]
    fn the_managed_lines_are_found_and_nothing_else_is() {
        let lines = managed_lines(CONF, &Owned::new(Some(505), &[], &live()));
        assert_eq!(lines.len(), 7);
        assert!(lines.iter().all(|l| l.starts_with("lxc.")));
        assert!(lines.contains(&"lxc.cgroup2.devices.allow: c 195:3 rwm".to_string()));
        assert!(lines.contains(&"lxc.hook.mount: /usr/share/lxc/hooks/nvidia".to_string()));
        // A snapshot section is never read.
        assert!(!lines.iter().any(|l| l.contains("snaptime")));
        // The `=` spelling is the same line.
        let text = CONF.replace(
            "lxc.hook.mount: /usr/share/lxc/hooks/nvidia",
            "lxc.hook.mount = /usr/share/lxc/hooks/nvidia",
        );
        assert_eq!(
            managed_lines(&text, &Owned::new(Some(505), &[], &live())).len(),
            7
        );
    }

    #[test]
    fn only_a_major_this_node_used_is_ours() {
        // Last boot's major, remembered: ours, so it is replaced.
        let owned = Owned::new(Some(508), &[505], &live());
        assert!(is_managed("lxc.cgroup2.devices.allow: c 505:0 rwm", &owned));
        assert!(is_managed("lxc.cgroup2.devices.allow: c 508:1 rwm", &owned));
        // A major nobody here ever used stays, module loaded or not.
        assert!(!is_managed(
            "lxc.cgroup2.devices.allow: c 226:0 rwm",
            &owned
        ));
        assert!(!is_managed(
            "lxc.cgroup2.devices.allow: c 999:0 rwm",
            &owned
        ));
        // A guest that missed the pass where the major changed still carries
        // an older one; every major this node has used is ours, so that rule
        // is replaced rather than orphaned.
        let owned = Owned::new(Some(510), &[508, 505], &live());
        for line in [
            "lxc.cgroup2.devices.allow: c 510:0 rwm",
            "lxc.cgroup2.devices.allow: c 508:1 rwm",
            "lxc.cgroup2.devices.allow: c 505:0 rwm",
            "lxc.cgroup2.devices.allow: c 195:3 rwm",
        ] {
            assert!(is_managed(line, &owned), "{line}");
        }
        assert!(!is_managed(
            "lxc.cgroup2.devices.allow: c 226:0 rwm",
            &owned
        ));
        // With nvidia-uvm unloaded, no UVM line is ours: a rule this pass
        // cannot replace is one it leaves alone.
        let owned = Owned::new(None, &[505], &live());
        assert!(!is_managed(
            "lxc.cgroup2.devices.allow: c 505:0 rwm",
            &owned
        ));
        assert!(is_managed("lxc.cgroup2.devices.allow: c 195:3 rwm", &owned));
        // Everything else, as ever.
        assert!(!is_managed("lxc.cgroup2.devices.allow: c *:* rwm", &owned));
        assert!(!is_managed("lxc.cgroup2.devices.allow: b 8:0 rwm", &owned));
        assert!(!is_managed("lxc.hook.mount: /root/my-hook", &owned));
        assert!(!is_managed("lxc.environment: LANG=C", &owned));
        assert!(is_managed(
            "lxc.environment: NVIDIA_DISABLE_REQUIRE=1",
            &owned
        ));
    }

    #[test]
    fn rendering_is_what_the_hand_configured_container_has() {
        let cards = cards();
        let lines = render(&[&cards[0]], &["compute", "utility"], None, Some(505));
        assert_eq!(
            lines,
            [
                "lxc.cgroup2.devices.allow: c 195:3 rwm",
                "lxc.cgroup2.devices.allow: c 195:255 rwm",
                "lxc.cgroup2.devices.allow: c 505:0 rwm",
                "lxc.cgroup2.devices.allow: c 505:1 rwm",
                "lxc.hook.mount: /usr/share/lxc/hooks/nvidia",
                "lxc.environment: NVIDIA_VISIBLE_DEVICES=GPU-8ddd601a-494f-9489-46c6-d23129ccad16",
                "lxc.environment: NVIDIA_DRIVER_CAPABILITIES=compute,utility",
            ]
        );
        assert_eq!(
            managed_lines(CONF, &Owned::new(Some(505), &[], &live())),
            lines
        );
    }

    #[test]
    fn rendering_two_gpus_with_graphics_and_a_cuda_constraint() {
        let cards = cards();
        let lines = render(
            &[&cards[0], &cards[1]],
            &["compute", "graphics", "utility"],
            Some("12.8"),
            Some(505),
        );
        assert!(lines.contains(&"lxc.cgroup2.devices.allow: c 195:254 rwm".to_string()));
        assert_eq!(
            lines.last().unwrap(),
            "lxc.environment: NVIDIA_REQUIRE_CUDA=cuda>=12.8"
        );
        assert!(lines.iter().any(|l| l
            == "lxc.environment: NVIDIA_VISIBLE_DEVICES=GPU-8ddd601a-494f-9489-46c6-d23129ccad16,GPU-1f4d0e27-1111-2222-3333-444455556666"));
        // No UVM module, no UVM lines.
        let lines = render(&[&cards[0]], &["compute"], None, None);
        assert!(!lines.iter().any(|l| l.contains("505")));
    }

    #[test]
    fn splicing_keeps_everything_else_and_is_a_fixed_point() {
        let cards = cards();
        let owned = Owned::new(Some(508), &[505], &live());
        let desired = render(
            &[&cards[1]],
            &["compute", "utility"],
            Some("12.8"),
            Some(508),
        );
        let out = splice(CONF, &desired, &owned);
        assert_eq!(managed_lines(&out, &owned), desired);
        assert!(out.contains("hostname: llm\n"));
        assert!(out.contains("rootfs: local-zfs:subvol-423-disk-0,size=32G\n"));
        // The snapshot section is untouched, separator and all.
        assert!(out.ends_with("\n[before-gpu]\narch: amd64\ncores: 4\nhostname: llm\nmemory: 8192\nostype: debian\nrootfs: local-zfs:subvol-423-disk-0,size=32G\nsnaptime: 1757000000\nunprivileged: 1\n"));
        assert_eq!(splice(&out, &desired, &owned), out);
        // The 505 lines of the previous boot went with the rewrite.
        assert!(!out.contains("505"));
    }

    #[test]
    fn splicing_nothing_removes_the_lines_and_leaves_the_rest() {
        let owned = Owned::new(Some(505), &[], &live());
        let out = splice(CONF, &[], &owned);
        assert!(!out.contains("nvidia"));
        assert!(!out.contains("NVIDIA"));
        assert!(out.contains("unprivileged: 1\n"));
        assert!(out.contains("[before-gpu]"));
        assert_eq!(splice(&out, &[], &owned), out);
    }

    #[test]
    fn lines_go_to_the_end_of_the_main_section_when_there_are_none() {
        let cards = cards();
        let owned = Owned::new(Some(505), &[], &live());
        let text = "arch: amd64\nhostname: x\nunprivileged: 1\n\n[snap]\narch: amd64\n";
        let out = splice(
            text,
            &render(&[&cards[0]], &["compute"], None, Some(505)),
            &owned,
        );
        assert!(out.starts_with("arch: amd64\nhostname: x\nunprivileged: 1\nlxc.cgroup2"));
        assert!(out.contains("\n\n[snap]\narch: amd64\n"));
    }

    #[test]
    fn refusals_are_seen_in_the_text() {
        assert!(is_unprivileged(CONF));
        assert!(!is_unprivileged("arch: amd64\nhostname: x\n"));
        assert!(is_unprivileged(
            "arch: amd64\nlxc.idmap: u 0 100000 65536\n"
        ));
        assert!(nvidia_dev_entries(CONF).is_empty());
        let text = "arch: amd64\ndev0: /dev/nvidia0,uid=0\ndev1: /dev/ttyUSB0\n";
        assert_eq!(nvidia_dev_entries(text), ["dev0"]);
        assert_eq!(hostname(CONF), Some("llm"));
    }
}
