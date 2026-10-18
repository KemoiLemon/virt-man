# Plan: Replace libvirt with direct QEMU management

## Goal and scope

Replace libvirt as the VM control plane with direct QEMU process management. The daemon continues to run as root. Start with a deliberately small VM feature set; do not attempt to replace every libvirt subsystem in the first milestone.

This host has no existing VMs, storage pools or networks, so every resource
created after the switch is managed natively. There is no conversion path from
libvirt definitions: an existing guest would remain libvirt's until it is
deleted, and none exist here.

Keep configuration, durable daemon state, runtime files, and QEMU VM state separate:

- `/etc/virt-man-daemon/` — daemon configuration, token, TLS material, firewall/IPAM/id-map configuration, and other configuration currently under `/etc/virt-man`.
- `/var/lib/virt-man-daemon/` — durable daemon metadata and state that must survive restarts.
- `/run/virt-man-daemon/` — sockets and other ephemeral daemon runtime files.
- `/var/lib/libvirt-daemon/` — configurable QEMU VM state root for per-VM state and temporary files, as requested. Verify this path against target distributions' QEMU packaging conventions before finalizing it; do not use it as the general daemon data directory.

## Stage 1: Configuration and installer transition

1. Change daemon defaults, examples, installer paths, and service documentation from `/etc/virt-man` to `/etc/virt-man-daemon`.
2. Migrate token, TLS material, firewall, container id-map, and other daemon-owned files. Preserve token and certificate bytes so existing web-server authentication and certificate pins remain valid.
3. Make migration idempotent: prefer an existing new-path file, copy/move a legacy file only when the destination is absent, preserve permissions, and report actions. Never overwrite new configuration with legacy data.
4. Keep a compatibility fallback for legacy `/etc/virt-man` installations for a defined transition period; document rollback.
5. Remove the daemon's libvirt service dependency only when the direct-QEMU backend no longer needs libvirt for the enabled feature set. The initial transition should not uninstall or disable libvirt.

## Stage 2: Direct QEMU process and QMP backend

1. Add a backend boundary so API handlers depend on VM operations rather than libvirt types.
2. Launch QEMU using an explicit argument vector, never a shell command string.
3. Track each managed VM's UUID, PID/process identity, QMP socket, console endpoints, state directory, and configuration in durable daemon metadata.
4. Use QMP for state queries and basic power operations. Validate socket ownership and permissions.
5. On daemon restart, reconcile recorded VMs against live QEMU processes; avoid duplicate launches and detect stale metadata or untracked processes.
6. Define safe cleanup behavior for a crashed or unexpectedly exited QEMU process. The daemon must distinguish its managed processes from unrelated QEMU instances.
7. Build configuration under the QEMU state root, but keep authoritative persistent metadata under `/var/lib/virt-man-daemon`.

## Stage 3: Minimal usable VM lifecycle

Support only an explicitly documented subset:

- list and inspect VMs managed by this daemon;
- create a VM with disk path, memory, vCPU count, and one supported networking mode;
- start, graceful shutdown, force stop, and delete;
- report process state and basic resource usage;
- provide console access only if it can be implemented and tested reliably in this stage.

Retain existing API routes where their semantics are valid. Return explicit unsupported-feature errors for operations not yet implemented; do not silently approximate libvirt behavior. A minimal backend should not expose misleading network, storage, XML, or hotplug controls.

## Stage 4: Disk handling

1. Treat disks as host paths initially; do not emulate libvirt pools in this milestone.
2. Define and validate the allowed disk-path policy for the first release.
3. Keep existing disk files in place and do not reformat or rewrite them.
4. Preserve in-use checks and require explicit intent before deleting disk data with a VM.
5. Defer volume creation, upload/fetch, pool capacity, templates, and storage quotas to the separate storage-pools plan.

## Features explicitly deferred

These have separate plans or require replacement designs and are not implied by basic QEMU lifecycle support:

- LXC containers and templates (`PLAN-LXC.md`);
- storage pools and volumes (`PLAN-STORAGE-POOLS.md`);
- interfaces, bridges, DHCP/DNS, and NAT (`PLAN-NETWORKING.md`);
- device hotplug (`PLAN-HOTPLUG.md`);
- firewall policy and enforcement (`PLAN-FIREWALL.md`);
- backend-independent UUID identity (`PLAN-UUIDS.md`);
- libvirt XML editing, nwfilters, SPICE relay discovery, and libvirt-specific migration behavior.

Until their replacements are available, retain libvirt packages, definitions, networks, and pools for resources that still depend on them. Clearly distinguish QEMU-managed VMs from libvirt-managed resources in daemon responses and errors.

## Rollout and acceptance criteria

1. Back up `/etc/virt-man` and relevant data before upgrading. Do not treat volatile libvirt runtime files as backups.
2. Install the new configuration layout and confirm token/certificate continuity before changing VM backends.
3. Keep libvirt installed and operational while validating the direct-QEMU backend; containers still use its LXC driver.
4. Create one QEMU-managed VM, verify boot, disk access, state reporting, and recovery after daemon restart, then create further VMs as needed.
5. Remove libvirt dependencies only in a later release, after required container, networking, storage, and firewall capabilities have replacement implementations.

The first milestone is accepted when the new config layout migrates safely, a QEMU-managed VM can complete the supported basic lifecycle, and daemon restart does not duplicate or orphan it.
