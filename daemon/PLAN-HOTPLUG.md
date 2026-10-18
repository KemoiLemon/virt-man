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

## Acceptance criteria

- A supported device can be added and removed live.
- Status reflects guest acknowledgement, including pending removal.
- The device remains correctly configured after VM restart.
- Failed operations do not leak host resources or report false success.
