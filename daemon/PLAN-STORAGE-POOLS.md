# Plan: Replace libvirt storage pools

## Goal

Replace libvirt storage pools and volumes with daemon-managed metadata over ordinary host directories and files, without moving or rewriting existing image files.

## Stages

1. **Choose the storage model.** Define a pool as a configured root directory, purpose, and allowed operations. Decide whether metadata is discovered from disk or stored under `/var/lib/virt-man-daemon`.
2. **Add read-only discovery/import.** Keep existing files in place. Import pool paths, titles, and volume metadata without moving bytes; verify access and ownership before enabling writes.
3. **Define safe volume operations.** Specify create, list, inspect, hash, upload, fetch, and delete semantics. Canonicalize paths and prevent escaping a pool root through `..` or symlinks.
4. **Preserve in-use protections.** Track VM/container backing paths and refuse deletion while referenced. Compare canonical backing paths, not only display names.
5. **Reimplement capacity and quotas.** Use filesystem measurements for pool availability and file sizes for usage. Decide how to account for sparse files, backing files, and apparent versus allocated size.
6. **Preserve transfer recovery.** Store transfer metadata and staged-upload completeness markers under daemon state, not transient QEMU runtime directories.
7. **Enable writes only after import is verified.** Keep pool creation/deletion and volume mutation separate from the initial VM lifecycle milestone.

## Acceptance criteria

- Existing disk paths remain valid and files are not rewritten by import.
- Pool reads reflect external file changes.
- Deletion refuses to remove a disk in use.
- Upload/fetch recovery preserves the existing complete-body guarantees.
- Path traversal and symlink escapes are rejected.

## Migration and rollback

Export libvirt pool definitions and volume metadata, import them read-only, and compare the discovered files against the export. Keep libvirt pools available during compatibility testing. Rollback must not require moving or reconstructing image data.
