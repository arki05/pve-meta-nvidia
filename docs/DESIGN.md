# pve-meta-nvidia: design

What is true of the code, and the decisions behind the parts that are not
obvious. The README says how to use it.

## The hook, not `devN:`

Two mechanisms can put `/dev/nvidia*` into a container, and only one of them
also brings the driver's userspace in:

* **LXC's `nvidia` hook** (`/usr/share/lxc/hooks/nvidia`, from `lxc-pve`) runs
  `nvidia-container-cli --user configure --no-cgroups` as a mount hook. It
  bind-mounts the host's libraries and binaries into the rootfs, creates the
  device nodes, and runs the host's `ldconfig` on it. The container needs no
  driver, no version pinning and no post-upgrade step.
* **PVE's `devN:` passthrough** creates the device nodes from
  `lxc-pve-autodev-hook` and nothing else.

They cannot be combined. The autodev hook runs *after* the mount hook and mknods
nodes that are already there, so the start fails with `Could not mknod
.../dev/nvidia-uvm: File exists` — measured, not guessed. So this tool writes raw
`lxc.cgroup2.devices.allow` rules (the hook's `--no-cgroups` leaves them to
whoever configures the container) plus the hook line, and refuses a guest whose
config carries a `devN:` entry for an NVIDIA device rather than silently
producing a container that will not start.

The hook itself refuses a privileged container (`in_userns` must say `yes`), so a
privileged guest with a `gpu` key is refused here too, with that reason.

## The node publishes its own hardware

Which GPUs a host has is not the cluster's to state, which is what pve-meta's
node-level prefix files are for (its decision 020). This tool writes the whole
`gpu` schema into `/etc/pve/nodes/<node>/meta.d/prefixes/gpu.yaml`, one
`devices.properties.<uuid>` row per card, named and sized from `nvidia-smi`.
Consequences that are the point:

* a guest on a node without GPUs is offered no `gpu` rows at all, so the editor
  shows GPU rows exactly where they mean something;
* a guest that migrates gets the other node's set, and its document is untouched;
* there is no packaged prefix, so there is nothing to shadow, nothing to keep in
  step with the node files, and uninstalling leaves one removable file per node.

The file is written only when its content differs, compared as a document rather
than as text: every write moves pve-meta's version token and wakes every operator
polling it, including this one.

A node whose driver is **loaded** and has no cards has no file: a node that can
serve a GPU and has none should not offer one. A node whose driver is **not
loaded keeps the file it has**, and no guest's config is touched either —
stripping every container's GPU lines, or blanking every editor row, because a
driver package is mid-upgrade is exactly the accident this operator exists to
avoid. The trade-off is stated rather than solved: when a card is pulled for
good, or the driver uninstalled for good, the file stays until someone removes
it with `pve-meta delete nodes/<node>/prefixes/gpu`. A file that outlives its
cards costs a stale row in an editor; a file that vanishes for thirty seconds
costs every operator in the cluster a version-token wake-up and every guest on
the node its rows.

The file is this node's to write, so a hand edit of it is overwritten at the
next inventory pass, within five minutes. `devices` is likewise node-scoped: a
UUID names a card of the node the guest is on, and a guest migrated to a node
without that card starts without a GPU and says so only in this node's log.
Nothing arbitrates between guests either — two containers may select the same
card, and the driver time-slices them, as it does two processes on the host.

## The UUID is the name of a card

A document selects cards by GPU UUID. The minor (`/dev/nvidia3`) is what the
config line needs, but it is an enumeration order, not an identity: it changes
when a card is added, removed or reseated. The bus location is stable and is not
unique across nodes. The UUID is both, it is what `nvidia-container-cli --device`
takes, and it is what a migrated document can be checked against on another node.
So: UUIDs in the store, minors in the config, resolved on every pass.

## Every external command is bounded, and the loop has a watchdog



`nvidia-smi` against a GPU in a bad state sits in uninterruptible sleep; a
`pve-meta` write waits for whoever holds the document's cluster lock. An
unbounded wait on either is not a slow pass but a pass that never ends, and it
lands in the two worst places: before the first tick reports ready it holds
`pve-guests.service` until the start timeout kills the unit, and after it the
loop stops reconciling for good with nothing in the journal.

So every call has a deadline — `nvidia-smi` 10s, `nvidia-modprobe` 30s,
`pve-meta` 30s — and the whole mechanism is one loop: both pipes are
non-blocking and are read inside the same poll that waits for the child. That
is what makes the bound real and, more importantly, makes a **short answer
unrepresentable**: either the command finished and the output is all of it, or
the call failed. A truncated `pve-meta get` is still valid YAML with fewer
`devices:` entries in it, and acting on that would take a card away from a
running container — the one invariant every other rule here defends.

A child that misses its deadline is killed by **process group**, which takes
the tools' own children with it, and is then not waited for: a process in `D`
state never takes `SIGKILL`, and waiting would reproduce the hang the deadline
exists to prevent. It goes on a short list the next call clears with
`try_wait`, so a kill that worked costs nothing and one that did not costs one
entry — no threads, nothing that grows with the number of wedges, which matters
because the motivating case (a bad card) recurs every poll for as long as the
card is bad. The caller gets its error at once, with the tail of what the
command last said on stderr, which is usually what names the wedge.

The watchdog is the other half. `WatchdogSec=180` is comfortably above the
longest single bounded step, and a ping is sent **when each bounded step
returns** — inside `cmd`, not only between guests, because the inventory is
several bounded calls in a row with no guest between them and a watchdog that
kills a daemon for being slow rather than stuck is worse than no watchdog.
Pinging on the way *out* and not on the way in is deliberate: a ping then means
"a bounded step finished", so a daemon stuck inside one — in an uninterruptible
`pmxcfs` syscall of its own, say, which no deadline of ours can reach — is
recycled rather than counted as alive.

## The node publishes only what it read

The two halves of an inventory pass fail apart. Reading the GPUs is what every
reconcile depends on; writing the prefix file is a courtesy to the editor, and a
pmxcfs hiccup there must not discard a good read. Likewise a read either
succeeds or fails: an unreadable `/proc/devices`, an unlistable GPU directory or
an `information` file that is there and will not be read is an error, never a
shorter list of cards. The failure mode this avoids is precise — a short
inventory reads as "that GPU is gone", and the next pass takes it away from the
container that has it. When a read fails the daemon keeps the last inventory it
really had and skips reconciling altogether that tick.

`Inventory::default()` is therefore not "a node with no GPUs"; `driver` says
whether `/proc/driver/nvidia` was there at all, and `plan` does nothing for any
guest of a node whose driver is not loaded. A card whose directory is there and
whose `information` names no UUID is an error for the same reason: that is what
a GPU in reset looks like, and dropping it with a note would shorten the card
list and take that card off the container that has it.

A failed read is retried at the **next poll**, not at the next five-minute
interval: the timestamp that paces the inventory is set only when a read
succeeds. At boot that difference is the whole boot.

The prefix file follows the same rule from the other side: it is removed only
when the driver is loaded and really has no cards. A driver package upgrade
takes `/proc/driver/nvidia` away for a few seconds, and deleting the document
there would blank the editor's rows for every guest on the node and move
pve-meta's version token twice, waking every operator in the cluster, for a node
that is about to have the same cards again.

## Majors are read every pass, and the first pass is the boot pass

`/dev/nvidia` is the registered major 195; `nvidia-uvm` is allocated dynamically
and can differ after every boot or module reload. A config line carrying last
boot's major grants nothing, and CUDA fails inside the container with no obvious
cause.

So the daemon's first pass is ordered `Before=pve-guests.service` and
`After=pve-cluster.service`, and it is a `Type=notify` unit: PVE starts the
autostart containers only after the managed lines carry this boot's majors. It
notifies ready whether the pass succeeded or not — a GPU that is missed is worth
a log line, never a boot that hangs.

`nvidia-uvm` makes this sharper than it looks. The module is loaded lazily, by
`nvidia-modprobe`, when something first opens `/dev/nvidia-uvm` — so at boot,
with the driver perfectly healthy, it is simply not in `/proc/devices` yet. Two
rules follow, and together they are the boot pass:

* an inventory pass that finds the driver and no UVM major runs
  `nvidia-modprobe -u -c 0`, the same call the driver's own container tooling
  makes, and reads `/proc/devices` again;
* while there is no UVM major, **no UVM rule is owned** — not even the one this
  node wrote last boot. A rule this pass cannot replace is a rule it leaves
  alone. Without that, the pass whose whole purpose is to put this boot's major
  in would delete last boot's and write nothing, which is worse than doing
  nothing at all.

A line with a **stale** UVM major is recognised by memory, not by inference:
`/var/lib/pve-meta-nvidia/uvm-major` holds the majors this node has written
lines with, newest first, and those, 195 and the current UVM major are the only
majors this tool owns — minus any that `/proc/devices` now lists for something
else, because dynamic majors are reused and the number this node wrote in March
can be a Coral TPU's by June. The file is replaced through a rename, never
truncated in place: an empty one would read as "this node has written nothing"
and orphan every rule it ever wrote. It is a short list rather than one value because a guest
can miss the pass where the major changes — locked for a backup at boot, its
document unreadable that minute — and one remembered major later its rule would
belong to nobody and stay for good. Inferring "stale" from "no driver claims this major" was the first
version, and it is wrong in one case that matters: an allow rule for a device
whose module happens to be out — a Coral TPU between driver builds — would be
deleted for good.

## What the loop does every ten seconds, and what it does not

The change signal is a `stat`. Every poll the loop reads the vmlist and stats
each local guest's document; a document whose mtime or size moved is read again
through the `pve-meta` CLI, which owns every rule about what a document means,
and every five minutes each of them is read again anyway. pmxcfs reports
whole-second mtimes, so two writes inside one second with the same size look
like one: the stamp is a hint about *when* to read, never a value, and the slow
full pass is what bounds a miss. The first version polled `GET /meta/version`
through `pvesh`, as pve-compose does. That forks the whole Perl API tree every
ten seconds on every node — and with three operators doing it, three times — to
learn something two `stat` calls already say.

One more rule keeps that cheap without making it a guess: a guest with **no
document file** has no `gpu` key, so it costs a `stat` and no process at all —
except when this node's record says it wrote lines into that guest, because
there "no key" means "remove the lines", which is not a conclusion to draw from
a path. A store that is not where the stamps look needs no rule of its own:
every managed guest is read through the CLI anyway, `pve-meta` reports what is
wrong with the store per guest, and a guest whose document cannot be read is
skipped with its config untouched.

There is no boot-safe batch read to use instead: `GET /meta/guests --has gpu`
would answer it in one call, but it needs the API, and the first pass runs
before `pve-guests.service` and cannot depend on pveproxy being up. So a full
pass costs one `pve-meta` call per guest that has a document or that this node
manages — a handful on a GPU node, and bounded by the same timeout as
everything else.

Planning runs every poll for every local container: it is a config-file read and
a few string comparisons, and it is what catches a `pct set`, a snapshot
rollback or a hand edit with no second timer and no memo that can go stale.
Nothing is written when the lines are already what they should be, so a node at
rest writes nothing.

An unknown is never fact, and the three unknowns are separated on purpose:

* **the vmlist** cannot be read: the poll ends, nothing is reconciled — pmxcfs
  that is not mounted must never read as "no guest wants a GPU" (pve-meta's
  decision 022);
* **the inventory** could not be read: the last good one is kept and the whole
  reconcile phase is skipped;
* **one document** cannot be read or does not parse: that guest is skipped and
  every other guest of the node is reconciled as usual. A typo in one guest's
  metadata must never freeze a node, least of all during the boot pass.

States that hold (a refusal, a lock, a skipped UUID, a document that does not
parse, a store that is away) are logged once, when they begin, one key per
condition; every write is logged.

The inventory — a walk of `/proc` and one `nvidia-smi` — runs at the top of
every poll, so a driver update without a reboot is noticed within ten seconds
rather than five minutes; a clock of its own was one more thing to reason about
than the cost it saved. This node's prefix file is written afterwards, and only
when the cards changed or the slow pass comes round, because that write costs
two `pve-meta` calls and moves the version token.

## The boot pass is budgeted, because something waits for it

`Before=pve-guests.service` means every second the first pass takes is a second
before the containers start — and if the pass ever took longer than the unit's
start timeout, systemd would kill it, the ordering would be satisfied anyway,
and the containers would start with last boot's major: the exact failure the
ordering exists to prevent. Being ordered first is therefore a promise to be
quick, and the promise is kept explicitly:

* the pass has a budget (60s of guests, checked between guests in both the
  document phase and the reconcile phase), and **the budget starts after the
  inventory**, which is bounded but not quick — a lazy `nvidia-modprobe` and an
  `nvidia-smi` are forty seconds of legal worst case, and a budget that began
  before them could be spent by the time the first guest came round, skipping
  every one of them silently;
* it takes no container config lock it cannot have at once, so a guest PVE is
  busy with is skipped rather than waited for, and that is the calm path, not
  an error;
* writing this node's prefix file happens **after** ready: it is a courtesy to
  the editor and not something any reconcile depends on, so it has no business
  standing between a booted node and its containers;
* whatever is left over is done by the ordinary poll ten seconds later, which
  nothing is ordered after;
* `TimeoutStartSec=300` is then only a backstop, since the worst honest case is
  the inventory, the budget, and one outstanding bounded call.

So `Before=pve-guests.service` promises exactly this: the node's GPUs were
read, and the first minute of guests was reconciled. A container whose guest
was not reached starts without its GPU until it is restarted, and the journal
says so in those words.

## Writes take PVE's lock and are PVE's writes

A write takes `flock` on `/run/lock/lxc/pve-config-<vmid>.lock` — what
`PVE::LXC::Config::config_file_lock` names and `PVE::AbstractConfig::lock_config`
takes around every read-modify-write of a container config — reads the config
*inside* the lock, plans again from that text, and replaces the file through a
sibling `<file>.tmp.<pid>` created `O_EXCL` and renamed over the target, which is
`PVE::File::file_set_contents` and what pmxcfs sees as one atomic replacement. A
`pct set` and a pass of this tool therefore take turns instead of losing each
other's changes.

Only the main section is ever read or written. A `[snapshot]` section is a past
config; rewriting it would rewrite history, and a rollback is caught by the next
poll anyway. A guest whose config PVE has locked (`lock: backup`, a snapshot, a
migration) is left alone until it is not.

## Writes are ordered so that nothing is orphaned

The record of what this node wrote is set **before** the lines are written and
cleared **after** they are removed. A record with no lines is a no-op the next
pass corrects; lines with no record are orphans nothing would ever clean up,
because the config of a guest with no `gpu` key is not this tool's to read.

The rename that replaces a config re-checks that the config is still there
first: a guest destroyed while a pass was planning must not have its file put
back. That narrows the window rather than closing it — a destroy between the
check and the rename still wins — and the guest's own config lock, held around
the whole read-plan-write, is what makes the window small.

## The one thing the store cannot answer

A guest whose document has no `gpu` key is one of two things: a container this
tool wrote lines into whose key was just removed, or a container someone
configured by hand that must not be touched. A document holds intent, and a key
that is gone says nothing at all; writing status back into the store is not an
option (it would move the version token and fight the human editing the same
document).

So the node keeps a record of the guests it wrote lines into, one empty file per
vmid under `/var/lib/pve-meta-nvidia/managed`. It is the same idea as
pve-meta-publish's manifest — only replace or remove what you wrote — kept on the
node because the config it describes is the node's. A vmid is dropped from the
record when it is no longer one of this node's containers, so a destroyed or
migrated guest leaves nothing.

The limit is honest and documented: a key removed while this node's daemon was
not running leaves the lines behind, and `status` shows the guest with lines and
no key.

This is not [pve-meta's decision 009] in reverse. That decision refuses a
*sweeper* over the store — a process that walks documents and deletes what looks
orphaned. Nothing here deletes anything of its own accord: the record is written
by the pass that writes lines, read to answer "was this line mine?", and dropped
when the guest is no longer this node's. It is host-side because the artifact it
is about — the container's config file — is a host file.

[pve-meta's decision 009]: https://github.com/arki05/pve-meta/blob/main/docs/decisions/009-no-sweeper.md

## Three other ways to mark the lines, and why none of them is used

Ownership by line kind plus a small host-side record is the third design. The
two that look neater do not survive contact:

* **`lxc.include: /var/lib/pve-meta-nvidia/423.conf`** — one owned line in the
  guest's config, the rules in a file this tool alone writes, no record needed.
  It is rejected because liblxc treats a missing include as a fatal error: a
  guest restored (or migrated) onto a node without this package, or with the
  file not yet written, refuses to start. It also hides the rules from `pct
  config`, where an administrator looks first.
* **A `#` marker line** above the managed block, the way many tools mark their
  own output. PVE has no inert comment channel in a guest config: every `#` line
  is folded into the guest's `description`, so the marker would show up as the
  guest's notes in the UI and be rewritten by anything that edits them.
* **A packaged cluster `gpu` prefix with a generated node-level `gpu.devices`**,
  which is decision 020's own worked example. It is rejected for where this
  package is installed: only on GPU nodes. A packaged prefix file is read from
  whichever node answers the API, so the cluster-wide half of the schema would
  appear and disappear with the node that happens to serve the request. One
  node-level file per GPU node has no such ambiguity.

## Capabilities and `require_cuda` are passed through, not interpreted

`NVIDIA_DRIVER_CAPABILITIES` and `NVIDIA_REQUIRE_CUDA` are the hook's own
interface. The document's `capabilities` map is filtered against the six the hook
accepts (`compute`, `compat32`, `display`, `graphics`, `utility`, `video`) and
written in that order; an unknown one is refused when the document is parsed,
because the hook would fail the container's start instead. `require_cuda` is
validated as a version and becomes `NVIDIA_REQUIRE_CUDA=cuda>=<v>`, which makes
the hook refuse to start the container on an older host driver.

Two derived bits: the default is `compute,utility` when the document says
nothing, and `graphics` or `display` also get `/dev/nvidia-modeset` (195:254),
which nothing else needs. The modeset line is the one rendered line not verified
on live hardware.

The node's prefix file offers a row for every capability the parser accepts,
generated from the same list, so the editor can never offer a capability a write
would refuse — or hide one (`display`) that changes what is written.

## Nothing from a document reaches a shell or a config key unvalidated

A UUID becomes part of `lxc.environment: NVIDIA_VISIBLE_DEVICES=...` and is
validated as `GPU-` plus hex and dashes; `require_cuda` is digits and dots. The
generated prefix document is handed to `pve-meta set --text`, not written to a
staging file: a root process that writes a predictable path in `/tmp` is one
symlink away from truncating something else. Neither can carry a newline, a comma or a space, so a document can never
become a second config line or a second `--device` argument. A holder of a
pve-meta token scoped to `gpu` can attach the host's GPUs to a container — that
is the feature — and nothing more.
