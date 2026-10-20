# Storage pools: native directories

## Decision

The daemon owns storage directly. A pool is a directory on the host plus a
little metadata. A read is a directory scan, so it is always true: a file that
arrives another way (aria2, `cp`, a Nextcloud sync) is visible immediately and
the space figures cannot go stale.

`pools.json` is the only source of truth. Nothing is imported or migrated from
an older storage manager.

## Model

`<data_dir>/pools.json`, mode 0600, written atomically (temp + rename):

```json
{"pools":{"<canonical-uuid>":{"title":"Fast SSD","path":"/srv/images",
                              "autostart":true,"active":true}}}
```

- The key is the pool's canonical UUID — its `name` on every wire endpoint.
- `title` is the display name (`?title=` on create); a pool definition has no
  `<title>`.
- `path` is the canonical directory. `active` and `autostart` are persisted.
- Only directory pools exist. `type` other than `dir` is refused with "not
  implemented natively" rather than silently ignored.

A volume is a regular file directly inside the root. There is no per-volume
record: `capacity` is the qcow2 virtual size from the header (magic `QFI\xfb`,
big-endian u64 at offset 24), falling back to the file length; `allocation` is
`st_blocks * 512`; the pool's capacity/available come from `statvfs` on the
root.

## Safety rules

- **Roots are validated at create/start and revalidated on every use.** The
  stored path is canonical; if it is now a symlink, or resolves elsewhere, the
  operation refuses with "pool root changed or is a symlink; re-point the pool".
  Overlapping roots are refused, as are the filesystem root and a deny list:
  `/proc`, `/sys`, `/dev`, `/run`, `/etc`, `/root`, `/boot`, `/tmp`, `/var/tmp`,
  the daemon data/config dirs, and the web server's `/var/lib/virt-man`
  (and `/var/lib/virt-man-web`). `/home` stays allowed: roots are arbitrary
  directories an administrator chose.
- **Scans never follow a symlink.** Only regular files are listed; a symlink is
  skipped. Dot-prefixed names are the atomic-write temporaries and are neither
  listed nor addressable. Names the API cannot address are skipped too, so
  everything listed can be deleted.
- **Startup never creates a missing autostart root.** An absent directory
  usually means its filesystem is not mounted, and creating an empty one would
  hide that. Startup leaves the pool inactive; an explicit start (or a create)
  may recreate it.
- **Deleting a pool deletes its definition only**, never its files, and is
  refused (409) while a guest still holds one of its volumes. Deleting a volume
  is refused while a guest references it, matching a file path and a
  pool+volume reference as the same backing store.

## Volume operations

- **Create**: name must be one safe component; `capacity` takes a number with
  an optional `B|KiB|MiB|GiB` unit (bytes when absent). Raw is a sparse
  `set_len` file; qcow2 is made by `qemu-img` with the destination pre-created
  mode 0600 (so qemu-img's own 0644 default never lands), with a temp-and-rename
  fallback for a qemu-img that refuses an existing destination. A partial file is
  removed on failure.
- **Upload and fetch**: the body/download is spooled to `/var/tmp` first. The
  daemon then copies it into a dot-prefixed temp **in the pool root**, hashing
  while copying, compares an expected digest, fsyncs, and renames over the
  target. A failure deletes the temp and leaves an existing target untouched.
  An existing name is never overwritten (409); an overwrite requires a regular,
  non-symlink file.
- **Hash**: streamed SHA-256 of the stored bytes.
- **Titles**: kept in `volume-titles.json` (daemon state, not the pool), keyed
  by pool UUID and volume name, and cleared when the volume or its pool goes.

## Wire compatibility

Routes and JSON are unchanged: `GET /pools` and `GET /pools/{p}` return
`{name,uuid,title,active,persistent,capacity,allocation,available}` with
`persistent: true`; volume list rows keep `{name,title,path,capacity,
allocation,used_by,content}`; create returns `{name,path}`; upload returns
`{name,uploaded,sha256,recv_bps,write_bps}`; metrics keep
`{uuid,name,capacity,allocation}`. `?title=` is unchanged.

## Non-goals

- No LVM/filesystem pool types.
- No client-side transfer state: staging and progress stay server-side.
