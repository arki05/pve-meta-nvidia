# pve-meta-nvidia

NVIDIA GPUs for unprivileged LXC guests, described by the `gpu` subtree of the
guest's [pve-meta](https://github.com/arki05/pve-meta) document and written into
the container's config by a loop on the node it runs on. Nothing is installed
inside the guest: LXC's own `nvidia` hook bind-mounts the host's driver at start.

Reference for using it. Why it is shaped this way: [`docs/DESIGN.md`](docs/DESIGN.md).

## How it works

Each node publishes the cards it has as its own entry in the cluster `gpu`
prefix file, `nodes.<node>` inside `/etc/pve/meta.d/prefixes/gpu.yaml`, so
the guests on a GPU node get typed `gpu` rows in
the Metadata tab — by UUID, with the model and bus location as the row's
description — and the guests everywhere else get none:

```yaml
# in /etc/pve/meta.d/prefixes/gpu.yaml, written by this tool
nodes:
  jarvis:
    schema:
      type: object
      properties:
        devices:
          type: object
          description: GPUs this container gets, by UUID
          properties:
            GPU-8ddd601a-494f-9489-46c6-d23129ccad16:
              type: boolean
          default: false
          description: Tesla T10 16 GB · 0000:0a:00.0
    ...
```

A container's document then says which of them it gets:

```yaml
# pve-meta document of CT 423, subtree `gpu`
gpu:
  devices: { GPU-8ddd601a-494f-9489-46c6-d23129ccad16: true }
  capabilities: { compute: true, utility: true }   # optional; this is the default
                                                   # (compat32, display, graphics, video too)
  require_cuda: "12.8"                             # optional
```

and its config gets exactly these lines, and nothing else:

```text
lxc.cgroup2.devices.allow: c 195:3 rwm     # /dev/nvidia3, the selected card
lxc.cgroup2.devices.allow: c 195:255 rwm   # /dev/nvidiactl
lxc.cgroup2.devices.allow: c 505:0 rwm     # /dev/nvidia-uvm       (major from /proc/devices)
lxc.cgroup2.devices.allow: c 505:1 rwm     # /dev/nvidia-uvm-tools
lxc.hook.mount: /usr/share/lxc/hooks/nvidia
lxc.environment: NVIDIA_VISIBLE_DEVICES=GPU-8ddd601a-494f-9489-46c6-d23129ccad16
lxc.environment: NVIDIA_DRIVER_CAPABILITIES=compute,utility
lxc.environment: NVIDIA_REQUIRE_CUDA=cuda>=12.8
```

The hook (`lxc-pve` ships it) runs `nvidia-container-cli --user configure
--no-cgroups` at container start: it bind-mounts the host driver's libraries and
binaries into the rootfs, creates the device nodes there and runs the host's
`ldconfig` on it. So `nvidia-smi` works inside a plain Debian container with no
driver installed, and a host driver upgrade needs nothing in the guest.

**Why not `devN:`.** PVE's own device passthrough looks like the obvious way and
cannot be combined with the hook: `lxc-pve-autodev-hook` mknods the same nodes
*after* the nvidia hook already created them, and the container fails to start
with `Could not mknod .../dev/nvidia-uvm: File exists`. A guest with a `devN:`
entry for an NVIDIA device is refused, whole, with the `pct set --delete` that
fixes it. Raw cgroup rules plus the hook is the combination that works.

**A change applies at the container's next start.** LXC reads the config when it
starts the container; nothing here ever restarts a guest. `pve-meta-nvidia status
<vmid>` shows the lines the config has and the lines it should have, so it is
clear when a restart is owed.

## Requirements

* PVE 9 (Debian trixie), `pve-meta` 0.2 or newer (the `nodes:` override), `pve-container`, `lxc-pve`.
* The NVIDIA driver on the node, installed any way you like (the `.run`
  installer, `nvidia-driver` from Debian, a DKMS build). It is not a package
  dependency and is checked for at runtime: a node with no `/proc/driver/nvidia`
  publishes no GPUs and **touches no guest at all**, and keeps the prefix file it
  already wrote — a driver mid-upgrade must not take the GPU lines away from
  every container on the node, nor blank the editor's rows for a few seconds.
  When a card is pulled or the driver uninstalled for good, that entry stays
  until you remove it: `pve-meta merge prefixes/gpu --text 'nodes: {<node>: null}'`.
* `nvidia-modprobe` (part of every driver install), because `nvidia-uvm` is
  loaded lazily: at boot nothing has opened `/dev/nvidia-uvm` yet, so the module
  is not in `/proc/devices` and its major cannot be written into a config. Each
  inventory pass runs `nvidia-modprobe -u -c 0` when the driver is there and
  that module is not, then reads `/proc/devices` again. Until it is loaded, the
  UVM rules already in a config are left exactly as they are.
* `libnvidia-container-tools`, which the hook calls. It comes from NVIDIA's own
  repository:

  ```sh
  curl -fsSL https://nvidia.github.io/libnvidia-container/gpgkey \
      | gpg --dearmor -o /usr/share/keyrings/nvidia-container-toolkit-keyring.gpg
  curl -fsSL https://nvidia.github.io/libnvidia-container/stable/deb/nvidia-container-toolkit.list \
      | sed 's#deb https://#deb [signed-by=/usr/share/keyrings/nvidia-container-toolkit-keyring.gpg] https://#g' \
      > /etc/apt/sources.list.d/nvidia-container-toolkit.list
  apt update && apt install libnvidia-container-tools
  ```

* **Unprivileged containers only.** libnvidia-container's LXC hook refuses
  anything else (`FIXME: This hook currently only works in unprivileged mode`),
  so a privileged guest with a `gpu` key is refused and left untouched.

## Install

On every GPU node, after pve-meta:

```sh
apt update && apt install pve-meta-nvidia      # from apt.arki05.com
```

Installs `/usr/sbin/pve-meta-nvidia` and the `pve-meta-nvidia.service` loop
(enabled and started). Nothing else is configured: the first pass writes this
node's prefix file, and no guest is touched until one has a `gpu` key.

## Commands

```sh
pve-meta-nvidia inventory [--json]           # this node's GPUs; write its `gpu` prefix file
pve-meta-nvidia reconcile [<vmid>] [--json]  # write the managed lines: one guest, or every guest here
pve-meta-nvidia status [<vmid>] [--json]
pve-meta-nvidia daemon                 # what the unit runs
-v on any verb echoes every command run
```

Turning a GPU on for a container is an edit of its document — the Metadata tab,
or:

```sh
pve-meta merge 423 gpu --text 'devices: {GPU-8ddd601a-494f-9489-46c6-d23129ccad16: true}'
pct reboot 423
```

Turning it off again is `pve-meta delete 423 gpu` (or setting the device to
`false`): the lines go at the next poll, and the container loses the GPU when it
next starts.

## What the loop does

One loop per node, on the containers whose config lives there. Every 10 seconds
it stats each local guest's document (`/etc/pve/meta/<vmid>.yaml`), reads the
ones whose stamp moved through the `pve-meta` CLI, and plans every local
container from its config as it is now — so a document change, a `pct set`, a
snapshot rollback or a hand edit all converge. The node's GPUs are read at the top of every
poll, which is what catches a driver update without a reboot; every 5 minutes
every document is read again anyway and the prefix file is refreshed.

**The first pass runs before `pve-guests.service`.** `/dev/nvidia-uvm`'s major is
allocated dynamically and can differ after every boot, so the managed lines are
rewritten with this boot's major before PVE starts the containers that autostart.
That is also why the unit is `Type=notify`: it reports ready when that pass is
done, and never holds up a boot when it fails. Being ordered first is a promise
to be quick, so that pass has a budget of its own (a minute), takes no container
config lock it cannot have at once, and leaves whatever is left to the poll ten
seconds later. What being ordered first promises is exactly that much: the
node's GPUs were read and the first minute of guests was reconciled.

Nothing unknown is treated as fact: a vmlist that cannot be read ends the poll,
an inventory that could not be read keeps the last good one (and is tried again
at the next poll) and skips the reconcile entirely, and a single document that
does not parse costs that one guest — the rest of the node is reconciled as
usual and the guest's config is left alone. Every command the loop runs has a
deadline and is killed with its process group if it misses it, and the unit
carries a systemd watchdog, so a wedged GPU tool costs a log line and a restart
rather than a node that has quietly stopped reconciling.

Nothing about what was applied goes into the store. The document is intent; the
container's config file is the applied state.

## Things to know

* **Migration.** `devices` is a node-scoped selector: a UUID names a card of the
  node the guest is on. The document travels with the guest, the cards do not.
  The target node needs this package and GPUs with the *same UUIDs*; a selected
  UUID it does not have is skipped with one log line, and **the guest silently
  starts without a GPU** — the application inside it is what notices. Its config
  on the new node gets that node's majors.
* **Nothing arbitrates.** Two containers may select the same card, and both get
  it; the driver time-slices and they share its memory. That is the same thing
  two processes on the host do, and this operator adds no scheduler of its own.
* **The node's prefix entry is this node's to write.** A hand edit of
  `nodes.<node>` in `prefixes/gpu` is overwritten at the next inventory pass, within
  five minutes. Edit a guest's `gpu` subtree, never the node's entry.
* **MIG is not supported.** A MIG instance is not a device of
  `/proc/driver/nvidia/gpus`, so it could be neither offered nor turned into a
  device rule; a `MIG-…` id in `devices` is refused rather than half-honoured.
* **Guests this tool does not own.** A guest whose document has no `gpu` key is
  never touched, whatever its config contains — a container you configured by
  hand keeps its lines. The moment its document gets a `gpu` key, the managed
  lines are this tool's: it replaces every `lxc.hook.mount` line naming the
  nvidia hook, every `lxc.environment` line naming an `NVIDIA_` variable, and
  every `lxc.cgroup2.devices.allow` line for major 195 or the UVM major.
* **Removing the key.** The node remembers which guests it wrote lines into
  (`/var/lib/pve-meta-nvidia/managed/<vmid>`) and the last few `nvidia-uvm`
  majors it wrote (`/var/lib/pve-meta-nvidia/uvm-major`), so removing the `gpu`
  key removes the lines and a new boot's major replaces the old rule instead of
  piling up, even for a guest that missed a pass or two. A
  key removed while the daemon was not running on that node leaves the lines;
  `pve-meta-nvidia status` shows it. Nothing here deletes anything on its own:
  it records what this node wrote, and is read only to answer "was this line
  mine?".
* **Trust.** Write access to a guest's `gpu` subtree is enough to attach one of
  the host's GPUs to that container. Under pve-meta 0.2 that is
  `VM.Config.Options` on the guest — a grant over host hardware, not a label —
  and to be given accordingly.
* **Uninstalling.** Removing the package leaves this node's `gpu` entry and
  the managed lines where they are; merging `nodes: {<node>: null}` into
  `prefixes/gpu` removes the entry.

## A related operator

`pve-compose`, `pve-meta-guest-files` and this one are the same shape: a document
subtree, a per-node loop driven by pve-meta's version token, a per-guest lock,
`Kind::Lxc` from the vmlist, a "log this state once" helper. Whether that
plumbing becomes a shared crate is an open question; three copies is where it
becomes worth answering, and the copies are small enough to keep honest until
then.

AGPL-3.0-or-later.
