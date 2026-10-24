# Direct QEMU management

## Goal and status

The daemon controls VMs directly through QEMU and QMP. It does **not** use an
external VM manager: no daemon, socket, connection, network or storage driver is
read, and the daemon packages do not depend on one. Containers are a separate
runtime on LXC's own tools (`src/lxc.rs`, see `AGENTS.md`), not libvirt's.

Direct QEMU is the only VM path. Host networking, IPAM, the firewall, storage
pools and UUID identity are the daemon's own (see `PLAN-NETWORKING.md` and the
storage notes in `AGENTS.md`).

## Paths

- `/etc/virt-man-daemon/` — daemon configuration, token, TLS material, and the
  firewall/IPAM configuration.
- `/var/lib/virt-man-daemon/` — durable daemon metadata and state that must
  survive restarts (VM definitions, pools, networks, history).
- `/var/lib/virt-man-daemon/qemu/` — the QEMU VM state root (configurable as
  `qemu_state_root`): one directory per guest holding its QMP/guest-agent
  sockets, UEFI variable store, TPM state and swtpm socket/pid. It is a
  subdirectory of the daemon data directory, not a second data root.
- `/run/virt-man-daemon/` — sockets and other ephemeral daemon runtime files.

This host is greenfield: nothing is imported and no hypervisor state is
migrated from an older daemon data path.

## Lifecycle

A guest's hardware is the libvirt-shaped domain XML the SPA writes; the launch
argv, hotplug paths and host taps are derived from it. The manager launches
QEMU with an explicit argument vector, never a shell string, and tracks each
VM by UUID, process identity (pid + start ticks + executable), QMP socket and
runtime directory in durable metadata.

- **Start**: validate the stored disks, create the per-guest runtime directory,
  seed UEFI NVRAM from its template, start the guest's swtpm when the
  definition has a `<tpm>`, create taps, launch QEMU, then confirm it is alive
  through QMP. If any step fails, the taps and the TPM emulator are cleaned up
  with it.
- **TPM**: a `<tpm>` guest gets a per-guest `swtpm socket` process. The daemon
  runs `/usr/bin/swtpm` with `--tpm2` for a 2.0 TPM and without it for 1.2,
  state at `<runtime>/tpm`, control socket `<runtime>/swtpm.sock` and pid file
  `<runtime>/swtpm.pid`. Stop, force-stop, delete and reconcile stop that
  process for a dead guest; a running guest's swtpm is never stopped. Delete
  removes the state directory with the rest of the runtime. A pid is signalled
  only after `/proc/<pid>/cmdline` names this guest's state and socket.
- **Stop / force stop**: ask QEMU to power down (or SIGKILL it), wait for the
  process to exit, then remove its taps and its swtpm.
- **Reconcile**: on startup and on every sweep, a recorded guest whose process
  identity no longer matches is treated as stopped: its stale taps and swtpm
  are removed, while a live guest's are left alone.
- **Delete**: refused while QEMU is running. Runtime files (QMP socket, NVRAM,
  TPM state) go; disk files never do.

## Boundary (current)

The lifecycle is implemented. It is covered at unit level — argv, QMP wire
shapes, UUID/pid identity, reconcile and swtpm cleanup, serde and validation —
but not by a live QEMU start in the automated suite. libvirt is gone: no
definition, socket or helper is part of this path.

1. **Configuration transition** — daemon-owned files moved under
   `/etc/virt-man-daemon`, with an idempotent migration that never overwrites
   new configuration.
2. **Direct QEMU process and QMP backend** — explicit argv, UUID/pid identity,
   QMP state and power operations, startup reconcile, safe cleanup of a crashed
   process.
3. **Lifecycle** — list/inspect, create, start, graceful shutdown, force stop,
   delete, process state and basic resource usage. Console support is VNC plus
   a virt-viewer handoff for SPICE.
4. **Disk handling** — disks are host paths, validated and canonicalised;
   deleting a VM removes its runtime but never its disk files.
5. **QEMU-managed hardware** — hotplug for disks and NICs, the hardware editor
   including video and TPM, and the per-guest swtpm lifecycle described above.

## Not in scope

- imported definitions, pools, networks or firewall rules;
- migration between hosts;
- guest IPv6 networking: network, IPAM and firewall are IPv4-only.

## Acceptance

The direct backend is accepted when a QEMU-managed VM completes the supported
lifecycle (including a TPM guest, whose swtpm starts and stops with it), and a
daemon restart neither duplicates nor orphans a guest or its swtpm. Unit tests
pin the argv, identity and cleanup paths; a live QEMU run is still manual.
