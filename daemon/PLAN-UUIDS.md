# UUID identity

## Decision

A resource is named by the UUID it got at creation, not by anything a host
process or another manager assigned. Names are display data: a title can be
changed and old links still resolve, but ownership, grants, history and access
logs stay attached to the UUID. This host is greenfield, so there is no legacy
name to import and no migration mode: UUIDs exist from creation.

## Current identity

- **VM**: the manager UUID is the stored `<name>`, the `<uuid>`, the state
  directory `<data dir>/qemu/<uuid>` and the URL segment. The user's display
  name is `<title>`; routes resolve either, and an ambiguous title resolves to
  nothing.
- **Network**: the store key is a canonical UUID; `<name>` and `<uuid>` in the
  editor are that key, and `<title>` is display-only. A title must be unique and
  must not equal another network's key.
- **Pool**: `pools.json` is keyed by the canonical UUID; `<title>` lives beside
  it because a pool definition has no display field. A name that is not a
  canonical UUID is refused on create.
- **Volume**: a file name inside its pool, not a UUID. The pool UUID plus the
  name is what the API addresses, and in-use checks follow the file path.
- **Containers**: like a VM, the UUID is `<name>`, `<uuid>` and the key in
  `containers.json`; `<title>` is the display name. The rootfs volume is
  `ct-<uuid>.raw`, and the rootfs mount and uid range are keyed by the UUID.

## Keeping the web metadata attached

- The proxy resolves a UUID to its current name **before** the permission check
  (`api/proxy.rs` `canonical_path`), because grants and ownership are keyed by
  name.
- The web server follows renames every 30 s (`web/server/src/rekey.rs`): it
  remembers each UUID's name and moves ownership and grants when the name
  changes. A rename it did not see is matched by title only for a resource named
  by its own UUID, and never while the old name is still live.
- History is keyed by UUID, so a renamed guest keeps its chart and a new guest
  reusing a deleted name starts empty.

## Acceptance criteria

- Renaming a resource does not change its identity, grants, or history.
- Reusing an old name does not inherit the prior resource's ownership or history.
- A UUID and its current title resolve to the same resource; an ambiguous title
  fails rather than selects one.
