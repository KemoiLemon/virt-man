# Plan: QEMU device hotplug

## Goal

Add and remove supported VM devices through QMP while keeping persistent configuration synchronized with the live VM.

## Stages

1. **Publish a compatibility matrix.** Specify live and cold-plug support by QEMU version and machine type, starting with NICs and disks. Defer CPU/memory changes until separately validated.
2. **Use stable device identifiers.** Persist a device ID and use it consistently for QMP commands, configuration, and reconciliation.
3. **Implement add as a two-phase operation.** Validate and reserve host resources, issue the QMP command, confirm the device appears, and then commit persistent configuration. Clean up reservations on failure.
4. **Treat removal as asynchronous.** Guest acknowledgement may be required. Return pending until QMP confirms removal; acceptance of a request is not proof that the device is gone.
5. **Keep cold configuration authoritative.** Ensure hot-added devices appear in the next QEMU launch configuration. Reject operations that cannot be persisted safely.
6. **Test failure and recovery paths.** Cover daemon restart during a pending operation, QEMU exit, duplicate requests, and cleanup of taps or other host resources.
7. **Defer high-risk devices.** PCI passthrough, USB, mediated devices, and live memory changes need separate security and recovery designs.

## Boundary (current)

Implemented: **NICs** and **disks**, add and remove, for a stopped or running guest
(`/qemu/vms/{id}/nics`, `/qemu/vms/{id}/disks`, host-admin only).

- A disk is added as `blockdev-add` file node → `blockdev-add` format node →
  `device_add virtio-blk-pci`, and the same three arguments are rebuilt at the
  next launch, so cold configuration stays authoritative.
- A failed add rolls back the nodes it made; a failed persist rolls the live
  device back out. Removing a running guest's NIC returns `pending: true` and
  keeps its metadata, because `device_del` is a request and no
  `DEVICE_DELETED` event is observed. A running guest's disk returns the same
  `pending: true` while its config entry is dropped immediately, so a disk
  removed live does not come back at the next start.
- Device ids are one QEMU namespace: a NIC and a disk may not share one, and a
  disk path that QEMU's argv cannot spell as UTF-8 is refused up front.
- Host resources are not leaked: taps are derived and swept, block nodes are
  QEMU's own, and a disk file is never deleted by a removal.

Not yet: a live QEMU test. The disk/NIC paths are covered by pure tests (argv,
serde, validation, API wire) only; the QMP sequence has been reasoned about, not
executed on a real guest.

Still deferred (stage 7): PCI passthrough, USB, mediated devices, CPU/memory
changes.

## Acceptance criteria

- A supported device can be added and removed live.
- Status reflects guest acknowledgement, including pending removal.
- The device remains correctly configured after VM restart.
- Failed operations do not leak host resources or report false success.
