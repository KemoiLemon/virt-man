# Plan: Replace libvirt LXC containers

## Current status

Replacement is **not implemented**. Existing containers remain managed by
libvirt's LXC driver through the `/containers` API. The direct-QEMU backend is
VM-only and must not be presented as a container runtime. `GET /features`
reports this boundary. No migration or rootfs conversion has been performed;
do not start a second runtime against an existing container rootfs.

## Goal

Replace libvirt LXC with a container runtime that safely manages root filesystems, networking, lifecycle, and console access. This is a separate follow-on from the initial direct-QEMU VM milestone.

## Stages

1. **Choose the runtime and compatibility target.** Evaluate kernel primitives directly against `systemd-nspawn` and `runc`, including user namespaces, cgroup v2, console/pty support, networking, and migration of existing containers. Libvirt LXC XML is not a portable container specification.
2. **Define a backend-neutral container model.** Include UUID, rootfs path, title/hostname, uid/gid mapping, resource limits, interfaces, startup command, and console settings. Keep it separate from VM/QEMU process management.
3. **Preserve rootfs safety and format.** Do not rewrite image contents during migration. Keep mounts in a traversable runtime directory, constrain operations to the recorded rootfs, and use no-follow ownership changes when re-owning files.
4. **Implement lifecycle and reconciliation.** Add create, start, stop, delete, and recovery after daemon restart. Mount and namespace setup must be explicit, with partial failures reported clearly.
5. **Integrate console and metrics.** Provide a PTY console and metrics for supported cgroup controls. Privileged containers must not be an owner-grantable capability.
6. **Migrate one container at a time.** Require a stopped container, capture its libvirt XML and rootfs metadata, validate the supported subset, and retain the original definition and rootfs until the replacement is verified.
7. **Defer templates and advanced features** until basic lifecycle, isolation, resource limits, and restart recovery are reliable.

## Acceptance criteria

- A migrated unprivileged container starts with the expected uid mapping.
- It reaches the configured network and has a working console.
- Restart recovery does not duplicate or orphan the container.
- Deletion is constrained to its recorded rootfs and does not affect unrelated files.

## Migration and rollback

Back up container metadata and rootfs first. Never run both runtimes against the same rootfs simultaneously. Keep the libvirt definition intact until the replacement has passed verification; document how to stop the new runtime and restore the old definition.
