# Plan: UUID identity independent of libvirt

## Goal

Make UUIDs the stable identity for VMs, containers, networks, and pools, independent of libvirt naming and process identifiers.

## Stages

1. **Define identity ownership.** Generate UUIDs in the daemon for new resources and persist them independently from display names and host process IDs.
2. **Keep names mutable.** Resolve API/UI names and titles to UUIDs. Names must not be the durable key for ownership, grants, or history.
3. **Add a compatibility resolver.** During migration, resolve legacy libvirt names and UUIDs to the same daemon identity. Detect collisions and report ambiguous names explicitly.
4. **Preserve web metadata.** Keep ownership, ACL grants, history, activity, and access logs attached to the same UUID through backend changes and renames.
5. **Import existing identities.** Prefer valid existing libvirt UUIDs. For resources without one, generate a UUID and record a legacy identifier mapping. Do not infer identity from an ambiguous title.
6. **Update rename and delete flows.** Resolve UUID before authorization and destructive operations. Retain mappings needed for audit and rollback.
7. **Test transitions.** Cover duplicate names across backends, renames, stale links, deletion/recreation with the same name, and UUID-based authorization.

## Acceptance criteria

- Renaming a resource does not change its identity, grants, or history.
- Reusing an old name does not inherit the prior resource's ownership or history.
- UUID and legacy-name paths resolve consistently while migration is enabled.
- Ambiguous legacy identifiers fail safely rather than selecting an arbitrary resource.
