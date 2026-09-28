# AGENTS.md

Working notes for coding agents in this repo. Read `DESIGN.md` for *why* the
system is shaped the way it is; this file is *how to work in it without
breaking things*.

## What this is

Two parts, one product:

| Path | Build | Role |
| --- | --- | --- |
| `daemon/` | `cargo` | Rust + axum over libvirt (`qemu:///system`). Runs on the hypervisor host. Token auth, HTTPS `[::]:8443`. |
| `web/` | `pnpm` + `cargo` | `web/src` is the SPA; `web/server` is `virt-man-web` (users, ACL permissions, ownership, quotas, API keys, TLS-pinned daemon proxy). HTTPS `[::]:8006`. |

## Commands

```bash
# frontend: typecheck, build, unit tests (213, incl. the mobile CSS contract)
cd web && npx tsc --noEmit && npx vite build && pnpm test

# web backend (24 tests: permissions/roles/key caps + redirect URL building)
cd web/server && cargo clippy && cargo test

# live suites — drive the real app in jsdom against a running backend
#   arg is the base URL; defaults to http://127.0.0.1:8799
cd web && node test/live-ui.mjs      http://127.0.0.1:<port>
#   also: live-layout, live-tabs, live-hw, live-dc-vm, live-console, live-route,
#   live-vmnet (needs sudo: checks tc and the IPAM store; boots its own guest),
#   live-ipam (needs sudo + the Alpine live ISO: a real udhcpc against our DHCP,
#   and the access log of what that guest then fetched),
#   live-history (needs the server to have been sampling for ~30 s),
#   live-empty (no args: runs its own server against a stub daemon with no resources),
#   live-ops (needs sudo: defines a SPICE guest, stops/starts the daemon)
# auth-enabled suites need real credentials, since the account can be renamed
# through the feature under test. Point them at an HTTPS instance:
cd web && NODE_TLS_REJECT_UNAUTHORIZED=0 VM_USER=... VM_PASS=... \
  [VM_LOW_USER=... VM_LOW_PASS=...] node test/live-profile.mjs https://127.0.0.1:8006
# login path must show saved servers without a refresh (creates a connection):
cd web && NODE_TLS_REJECT_UNAUTHORIZED=0 VM_USER=... VM_PASS=... \
  node test/live-login.mjs https://127.0.0.1:8006

# transport-level: HTTP->HTTPS redirect on the single TLS port (raw sockets)
cd web && node test/live-redirect.mjs https://127.0.0.1:8006

# daemon
cd daemon && cargo clippy --all-targets && cargo test --bins   # 34 tests
cd daemon && cargo test --test fd_leak -- --ignored            # needs libvirt

# web backend
cd web/server && cargo clippy --all-targets

# install (builds as $SUDO_USER so repo artifacts stay user-owned)
sudo ./daemon/install.sh && sudo ./web/install.sh
```

Live suites need a running backend. Start a throwaway loopback instance:

```bash
VIRT_MAN_WEB_LOOPBACK_HTTP=1 VIRT_MAN_WEB_BIND=127.0.0.1 VIRT_MAN_WEB_PORT=8799 \
VIRT_MAN_WEB_DATA=/tmp/vm-live VIRT_MAN_WEB_DIST=$PWD/web/dist \
  ./web/server/target/debug/virt-man-web
```

**Run live suites with a gap between them** (`sleep 2`). They attach real
console sessions, and back-to-back runs collide.

## Hard constraints

These are not style preferences. Violating them produces broken builds or
silently broken UI.

**No native OS chrome.** `<select>` is `display:none` behind a themed
`.select-trigger`/`.select-popover` listbox (`web/src/listbox.ts`). Never use
`window.prompt`/`confirm`/`alert` — use `askText`/`askConfirm` in `main.ts`.
The browser's right-click menu is replaced on resource surfaces: every sidebar
row, and in the content pane the cards, dashboard rows, tabs and page header
open a `.ctxmenu` (`web/src/ctxmenu.ts`). One builder per resource (`vmMenu`,
`netMenu`, `poolMenu`, `serverMenu`, `rootMenu` in `main.ts`) serves every
surface, so a guest offers the same actions wherever it is right-clicked.
Every item carries an icon. Plain text, form fields and the XML and log panes
keep the native menu, so copy/paste still works there. Add Resource lives in
the Datacenter/sidebar-background menu and the Ctrl+K palette, not as a
sidebar button.

**Focus is handled once, in `web/src/focus.ts`.** One document-level Tab
handler keeps Tab / Shift+Tab inside the top layer (context menu, else the
topmost `.modal-back.open`), wraps at both ends, and pulls escaped focus back
in; `openModal` focuses the first field and returns focus to the opener on
close. So a new dialog needs no focus code: build it with `openModal`. A widget
that handles Tab itself (the palette) calls `preventDefault`, and the trap then
leaves it alone; the console stage keeps Tab for the guest. Clickable cards and
rows that are not buttons carry `tabindex="0"` and open on Enter/Space.
Tab moves between groups; arrows move within one (`ARROW_GROUPS`: context menu,
sidebar tree, tab bars, hardware list, card grids), wrapping, with Home/End.
Fields, `<select>` and the console keep their own arrows. A list that rebuilds
when an item is chosen (the tree, tab bars) wraps its `setHtml` in
`holdFocus(root, selected)`, or focus drops to `<body>` and the next arrow does
nothing; it only acts when focus was already in that list, so the poll never
steals focus.

**No inline event handlers.** `onclick="..."` in an HTML string is blocked by
the CSP. Assign handlers in JS after rendering.

**A pane render that awaits must check it still owns the pane.** Bump
`beginRender()` at the top of any renderer, and `if (!stillCurrent(gen)) return;`
just before painting. The 1s poll can start a network/pool render and the user
can then click elsewhere before it resolves; without the guard the stale
response paints over the view they switched to ("the network page just
returned"). Every path that leaves a resource page must also run
`clearResourceSelection()`, or the poll re-renders the page they left.

**`setHtml()` is the only way HTML enters the DOM** (`web/src/main.ts:18`). It
runs `DOMPurify.sanitize` *and* `enhanceSelects`. Assigning `innerHTML`
directly skips both, so the listbox silently stops working.

**Icons: one file per glyph** (`web/src/icons.ts`). Icons are Phosphor *fill*,
each imported as `@phosphor-icons/core/assets/fill/<name>-fill.svg?raw`; the
package's JS entry carries every icon in every weight, so importing from it
would pull all of them into the bundle. Fill colour is defined once in `.ico >
svg`; never set it per icon.

**Every colour comes from a CSS variable** on `html[data-theme=...]`. No
hardcoded hex outside the theme blocks. The one exception is the console
letterbox `#000` in `base.css`, which matches vm-man.

**Square aesthetic**: `border-radius: 0`, JetBrains Mono / DejaVu Sans Mono.
The theme is ported from `/home/kemoi/Projects/Personal/vm-man` — port its CSS
rather than inventing new styling.

**Preserve these or the product regresses**: auth + RBAC on the remote web UI;
auth-disabled loopback mode (used by the live suites and as a dev mode);
TLS-pinned daemon connections.

**ISO uploads stage on the web server before reaching the daemon**, in two
visible legs: browser → server (progress in the prompt) then server → daemon
(progress on the storage-pool window). The second leg runs as a detached
server-side task reading a local file, so it is **independent of the browser**:
a reload, or closing the tab, neither interrupts it nor loses track of it — a
freshly loaded page re-lists in-flight transfers from `GET /uploads`, because
the job lives on the server and not in the page. Keep it that way; making the
transfer depend on the client, or tracking progress client-side, breaks it.

A **web server restart no longer loses a staged upload**: jobs are in memory,
so a restart would otherwise drop them and the sweep would delete their bodies.
Instead `uploads::restore` runs at startup and brings back every upload whose
body **arrived in full**, as a `staged` job with a Send button on the pool
window — the bytes are already on disk, so resuming costs no re-upload.

Its sidecar (`<id>.json`, holding conn/pool/name/size/owner) is written only
after the body completes, and that is deliberate: the sidecar **is** the
completeness marker. Resuming a half-written body would send partial bytes, the
daemon would store exactly those, and the two hashes would **agree** — the hash
check proves the bytes travelled intact, not that the staged file was ever
complete, so a naive resume would report a truncated volume as verified. A
`.part` with no sidecar, a sidecar with no body, a size that disagrees with the
recorded one, or a sidecar naming a different id, are all deleted rather than
resumed. A restart is still fatal to a transfer **mid-body** (nothing complete
to resume), and to nothing else.

**A pool read refreshes the pool first.** `GET /pools/{name}` and
`GET /pools/{name}/volumes` call `virStoragePoolRefresh` before answering
(`daemon/src/api/pools.rs`, `pools::refresh_for_read`). libvirt caches a pool's
volume list and its space figures, and only re-reads them when the *volume API*
changed something — a file that arrives any other way (aria2, `cp`, `scp`) stays
invisible, and the space figures stay stale, until something refreshes. That is
the normal case here, not an edge case: the ISO pool directory is inside a
Nextcloud sync folder, so files appear and vanish without this daemon being
involved at all. Measured: a pool with 24 volumes and one with none both answer
in ~44 ms, because that time is the libvirt connection rather than the directory
scan — so it is cheaper to be accurate than to cache. The pool page polls every
second and would otherwise repeat the same wrong answer forever. The all-pools
list deliberately does not refresh every pool, keeping the sidebar's poll cheap.

**The URL is the view.** `web/src/route.ts` maps views to paths (`/`,
`/servers`, `/guests`, `/<node>`, `/<node>/vms|networks|storage|firewall`,
`/<node>/vms/<vm>[/console|hardware|options|firewall|xml]`,
`/<node>/networks/<net>`, `/<node>/storage/<pool>`) and is unit-tested. Every
view renderer calls `syncUrl()` right after `beginRender()`, which is the one
place all navigation passes through — so a new way to open a view needs no
routing code of its own. `syncUrl` pushes only when the path changes, so the 1s
poll never adds history. `renderCurrent()` (boot, Back/Forward) sets
`restoringRoute`, making the write a `replaceState`: a stale link that
normalises (a deleted node, pool or network) replaces the dead URL instead of
stacking it. A node is addressed by name unless that name is ambiguous or is a
top-level path the web server answers itself (`/users`, `/activity`, ...) —
then by id, or a reload would get JSON instead of the app. **Keep `RESERVED` in
`route.ts` in step with the server's top-level routes.**

**Pages paint from cache, then from the daemon.** A remote daemon costs a
round trip per read, so opening a view used to show nothing until it answered.
Three layers, each measured with 30 ms of RTT added to the daemon hop (`tc
netem`):

- The proxy keeps one `reqwest::Client` per connection (`daemon::http_client`).
  Building one per request threw away keep-alive and paid TCP + TLS every time:
  **164 ms → 33 ms** per call. The cache key includes the pinned cert, so
  re-pinning builds a new client rather than trusting the old one.
- The proxy remembers the last successful JSON read per (connection, path) and
  serves it to a request carrying `X-Virt-Man-Cache` (**~1 ms**). Any write
  through the proxy drops that connection's entries, so a delete cannot flash
  back. The SPA's `fastLoad` paints from that and then re-reads for real; the
  1 s poll never asks for the cache.
- Until either answers, the pane shows a `skeleton()` built from the real layout
  classes, so nothing jumps when content arrives.

`fastLoad` is only for views that are safe to paint twice, and **never** for a
view that shows statistics (dashboards, VM summary, pool usage): a cached CPU,
memory or disk figure is a wrong figure, and a chart that jumps as the fresh one
lands looks broken. Those wait on their skeleton. Nor for a view holding user
input (hardware, options, firewall editors): the second paint would discard what
they typed during the round trip. What remains is names and states — the tree,
the VM/network lists, filters, and XML. The stats series ignore a
repeated `sampled_at_ms` for the same reason a cached-then-fresh paint would
otherwise push a false 0% point.

**Finished transfers are forgotten, not kept.** A completed or failed upload
stays listed for 15 minutes (`FINISHED_TTL_MS`) and is then dropped, pruned when
`GET /uploads` is served. The activity log is the durable record of what
happened, so the transfer card is transient progress and dropping it loses
nothing — without the prune both the job map and the pool page grow for the life
of the process, and a page that lists every transfer ever run is worse than one
that lists current work.

**Activity is server-side and per-user.** Long operations report their outcome
to `POST /activity`, stored in `store.json` so it survives restarts. A
non-admin sees only their own rows; an admin sees all, and the attribution
cannot be forged by a non-admin. Progress during an operation is
still hand-rolled per call site (`runTask`'s `set`); there is intentionally no
client-side task list to keep in sync.

**Chart history is sampled server-side and deleted with its subject.**
`web/server/src/history.rs` polls every connection's daemon `GET /metrics`
(one call: host, pools, and every guest with its NICs and disks) every 10 s,
whether or not a browser is open, into `history.db` (SQLite, WAL) in the data
dir. Two weeks are kept; `GET /history` serves one series averaged into ~240
buckets. The range bar (Live/1h/6h/1d/7d/14d) sits on the datacenter and host
dashboards and the VM summary; Live stays the page's own 1 s poll.

- Rows are keyed by libvirt **UUID**, so a renamed guest keeps its history and
  a new guest reusing a deleted one's name starts empty.
- Deletion is **by absence**: a tick that no longer lists a guest, NIC or disk
  deletes its history (samples cascade), and removing a connection deletes all
  of its rows at once. So `/metrics` must fail rather than answer partially: a
  guest listed without its devices would erase their history. An unreachable
  daemon answers nothing and so deletes nothing.
- A missing bucket is a **gap**, never a zero: the chart breaks its line and
  the hover readout says "no data". Stitching an outage shut draws a lie.
- Hover is one document-level handler (`bindChartHover`); the chart's values,
  first timestamp and step ride on the SVG's data attributes, so it survives
  the 1 s re-render.

**Permissions are Proxmox-style; read DESIGN.md "Auth and permissions".**
Things that bite:

- **Every daemon route must be in `web/server/src/routes.rs`.** A route it
  does not classify is refused (403), by design. Adding a daemon endpoint
  without a line there makes it unreachable, not open.
- **Non-admin XML is an allowlist** (`xmlcheck.rs`), in step with what
  `xml.ts`/`hardware.ts` write. A new form field that emits a new element or
  attribute works for admins and fails for everyone else until it is added
  there — with a test that the form's output passes.
- **Ownership is recorded by the proxy** when the daemon accepts a create
  (`guard::after`), and renames carry it and the grants. Anything made outside
  the app belongs to the first administrator.
- **The admin page is a view** (`/admin/<section>`, `web/src/admin.ts`), not a
  modal: it replaces the sidebar, and the admin button becomes its return
  button. Every section has its own path, and a tabbed section its tab too
  (`/admin/dashboard/usage`; `route.ts` `ADMIN_TABS`); `/admin` is the
  dashboard. The sidebar groups sections under
  `ADMIN_GROUPS`, and a new section must go in one. Its sections read on
  demand; the 1 s poll skips it. Servers are managed there (Daemons), not on
  the Datacenter, and an old `/servers` link lands on that page. VMs, Networks
  and Storage Pools are separate sections, each tabbed by daemon
  (`/admin/vms/<node>`; `route.ts` `DAEMON_TABBED`). Creating one for a user
  opens the normal form over the admin page with an explicit `conn` and an
  `after` callback, and never navigates away from it.

**Guests, networks and pools are named by their UUID.** A new one's
libvirt name is its UUID and what the user typed is its `<title>` (a pool
cannot hold a title: the daemon keeps pool titles in
`/etc/virt-man/titles.json`). URLs name them by UUID; old name links still
work. Things that bite:

- **Every daemon route takes a UUID or a name.** A middleware
  (`daemon/src/api/mod.rs` `resolve_uuids`) swaps a UUID segment for the name
  before routing, so handlers and their name-keyed records are unchanged.
- **The web proxy resolves a UUID to its name before the permission check**
  (`api/proxy.rs` `canonical_path`). Grants, ownership and suspension are keyed
  by name; checking the UUID path instead let a user carved out of a guest
  reach it by UUID. `test/live-uuid.mjs` pins it.
- **The daemon migrates existing resources itself** (`daemon/src/migrate.rs`):
  a guest only while shut off, a network only when no running guest uses it, a
  pool only when no guest names it; the old name becomes the title. It is gated
  by `VIRT_MAN_MIGRATE_UUIDS=1` (off by default). `VIRT_MAN_MIGRATE_ONLY=<prefix>`
  limits it to matching names, for testing on a host that has real guests.
- **The web server follows the renames** (`web/server/src/rekey.rs`, every
  30 s): it remembers each UUID's name, and moves ownership and grants when the
  name changes. A rename it did not see is matched by title, but only for a
  resource named by its own UUID, never while the old name is still live.

**Owners share their own VMs** (`web/server/src/api/shares.rs`, `/shares`).
It is a non-admin write to the ACL, so it is narrow on purpose: the owner or
an admin only (decided by ownership, not by holding `VM.Allocate` -- the
default role has that at `/`), to a user, on exactly that VM's path, not
propagating, as `VMUser` or `VMAdmin` only. Neither can delete it or share it
onward. A suspended VM cannot be shared by its owner.

**`*.Allocate` means create on a collection and delete on a resource.** The
default role grants it at `/` so users can create; `Store::privs` drops it on
a resource path unless it comes from an admin, a grant below `/`, or
ownership. Without that, any user could delete anyone's VM by name.

**A long operation reports its own outcome.** `runTask` (`web/src/task.ts`)
takes `{ done, notify: toast }` and toasts success or the server's reason once
the work settles. Never toast "created" beside it: that is what made a create
refused by a quota say it succeeded. A failure toast stays until dismissed, and
the VM create deletes the volumes it made if the guest is then refused, or a
refused create would still count against the user's storage. Refusals are
descriptive by construction (`api::denied`, `perm::check_quota`); keep new ones
that way -- say what was missing, on what, and what to do.
`test/live-quota.mjs` drives an over-limit create through the real form.

**Destructive daemon calls are logged by the proxy, not the SPA.** Every
`DELETE` proxied to the daemon is recorded with a human label, its outcome, and
a detail line — for a VM delete that includes what happened to its storage
(`removed 1 disk(s); kept 1 (iso)`). It lives in `web/server/src/api/proxy.rs`
so that *every* caller is covered, including a direct `curl` or an API key, not
just the buttons we happen to render. A failed deletion keeps the daemon's own
reason, so "who deleted this volume, and did it work" is answerable after the
fact. Volume deletions previously left no trace at all, which made a missing
ISO impossible to explain.

## Phone support

One SPA, one code path. The phone layout is a media query on `max-width:
820px` (landscape on a 12 mini is 812 px, so a narrower breakpoint drops the
mobile layout when the phone is turned sideways), and **`MOBILE.md` explains
why** — read it before changing anything below.

- **`.mobile-only` is `display: none !important`** and mobile.css reveals it
  the same way. Without `!important` it lost to `.btn` (same specificity, later
  in the bundle) and the drawer and palette buttons showed on every desktop.
- **`web/src/styles/mobile.css` loads last, after `density.css`.** Sizing
  overrides belong in that layer; a rule added to an earlier file loses the
  cascade and the tweak silently does nothing.
- **The drawer closes in `syncUrl()` and nowhere else.** That function already
  returns early unless the path changed, which is the only reason a closer
  there cannot fight the 1 s poll. Closing it per call site goes stale the first
  time someone adds a way to navigate.
- **A long press opens the *same* menu a right-click does.** iOS WebKit never
  fires `contextmenu`, so `touch.ts`'s long press is the only route to Start,
  Stop, Delete and Add Resource on a phone. Build menus in the `*Menu`
  functions and reach them through `menuAt()`; a new action added straight to
  the `contextmenu` listener is invisible on a phone.
- **The console's touch input is deferred, not classified up front.** Nothing
  is sent at `pointerdown`; a press is sent only once the gesture is known. A
  press sent early is a phantom click in a guest.
- **16 px is the minimum field size under `(pointer: coarse)`.** Below it iOS
  zooms the page on focus and does not zoom back, leaving the whole app stuck
  at 1.3× from one tap. Fields reach 16 px only through `mobile.css`.
- **Key reporting stays on the `keydown` path in `console.ts`.** The soft
  keyboard is raised by a transparent textarea inside `.console-stage` that is
  never read from; a second reporting path would double-send.
- **`viewport-fit=cover` is required** for `env(safe-area-inset-*)` to report
  anything, and portrait insets are often `0` while landscape insets are real.

## Testing rules

**Verify by measurement, not assumption.** This repo has a history of bugs that
looked fine in code review and were only caught by measuring: an fd leak
(counted fds across 200 connections), a console that connected but never
rendered (decoded the frame bytes), a graphics parser that silently fell back
to loopback (asserted the parsed address).

Concretely:

- A test that prints nothing is **not** a passing test. Check the exit code and
  the output. Two live suites once "passed" silently while crashing.
- When you assert on rendered pixels or geometry, decode the PNG (Python) —
  screenshots cannot be viewed by the agent.
- Headless Firefox needs `--profile /tmp/vm-ffprofile` (the user runs Firefox).
- jsdom limits to know: skips `<script type="module">` (hence the `VM_IIFE=1`
  build), never substitutes `var()` in computed styles, and has no
  `ResizeObserver`, `WebSocket`, or canvas 2D context.

Non-trivial logic leaves one runnable check behind. Prefer a real
`assert` in `web/test/*.test.mjs` or a Rust `#[test]` over a manual note.

## Conventions

- **Conventional commits** (`feat(web):`, `fix(daemon):`, `style(daemon):`).
  Explain the *cause* in the body, not just the change.
- Branch `dev` on `ssh://git@git.kemoi.top:2222/kemoi/virt-man.git`.
- `rustfmt` and `clippy --all-targets` clean before committing Rust.
- Comments explain *why*, especially where the obvious approach is wrong.
  Several comments in this codebase exist to stop a future reader "fixing"
  something back into a bug — leave them.

## Gotchas that cost real time

**Add Server doubles as the daemon installer.** When `POST /connections` cannot
reach the daemon (fresh host, not installed yet) and the token was blank or
"generate", the save does not fail: it stores the connection with a freshly
generated token and returns an `install_script` that runs `daemon/install.sh`
with `VIRT_MAN_TOKEN` set to that same token (so the daemon and the stored
token match). The SPA shows the script and polls
`GET /connections/{id}/cert` every second until the daemon is up, then pins the
cert through the normal `PUT`. `install.sh` honours `VIRT_MAN_TOKEN` by writing
it to the token file instead of generating one.

**libvirt `Connect` has no `Drop`.** `with_conn` must call `close()` on both
the ok and error paths, or event fds leak and every call fails with "Too many
open files" after a few hundred requests (`daemon/src/libvirt.rs:45`).

**A VM's networking lives on its Network tab, and applies live.** Interfaces,
their IPAM reservations and rate limits, and the guest firewall are one tab
(`/<node>/vms/<vm>/network`; the old `/firewall` path redirects). The daemon
applies every change to the running guest *and* its definition
(`daemon/src/api/nics.rs`, `networks.rs` `put_host`/`delete_host`):

- Rate limits are `update_device_flags(AFFECT_LIVE|CONFIG)` with `<bandwidth>`
  in KiB/s (taken as Mbit/s). They are enforced at once — libvirt installs an
  htb class and an ingress policer on the host tap; `live-vmnet` reads them
  back with `tc`, not from the XML.
- IPAM changes only the daemon's store (below); nothing is restarted.
  `live-vmnet` checks the bridge's ifindex is unchanged.
- Hot-add needs a free PCIe root port. q35 gives each device its own and libvirt
  only creates as many as boot needs, so new guests are defined with
  `HOTPLUG_PORTS` (14, as virt-manager does; four were measured to be used up
  by boot devices). Older guests without spares get an explanatory error.
- Hot-remove is a *request* to the guest OS (ACPI eject). The API reports
  `pending` while the guest still holds the device; never claim it is gone.
- The hardware editor rebuilds every `<interface>`, so it carries `<bandwidth>`
  through (`devicesFromXml`/`deviceXml`), or a hardware save would lift limits.

**Guests are given disks by path, not by pool+volume.** `web/src/xml.ts` and
`hardware.ts` write `<disk type='file'><source file='/…/x.qcow2'/>`. A
`<disk type='volume'>` reference is resolvable only if the libvirt on that host
can translate it into a path, and **libvirt 11.3.0 cannot** — measured on a
Debian 13 host, where every volume-sourced disk, in any pool including a fresh
one at the standard `/var/lib/libvirt/images` path, failed with `internal error:
missing storage backend for 'volume' storage`. libvirt 12.7.0 resolves the same
XML fine, so this is not our XML being wrong — it is a shape that does not work
everywhere, and the failure is nasty because of where it lands:

- The disk is never resolved, so libvirt never writes the guest's per-domain
  AppArmor rules (`/etc/apparmor.d/libvirt/libvirt-<uuid>.files`, generated by
  virt-aa-helper from the disk paths). With no rules, qemu is denied the disk.
- qemu then fails with `Could not open '/…/x.qcow2': Permission denied`, and
  **that message is a decoy**: the file is `0600 root:root` at that moment
  because libvirt chowns a disk to the qemu user at start and *restores the
  original owner when the start fails*. Chowning it by hand changes nothing, and
  neither does an AppArmor path rule — the disk was never resolvable, so there
  was nothing to allow.

A path has no such dependency. So: `disk_path`/`iso_path` on `DomainForm`, and a
`path` field on the disk/cdrom device, filled from the volume API's `path` (the
SPA creates the volume, then names it) or from the volumes list when one is
picked. `deviceXml`/`domainXml` use the path when they have one and keep the
reference form when they do not, rather than inventing a path for a volume we
cannot confirm. `driver type` travels with the disk — guessing qcow2 for a raw
image makes qemu parse it as the wrong format.

`devicesFromXml` **claims file-backed disks by path**. It used to skip them,
which silently deleted them: the hardware editor strips every `<disk>` block and
re-adds only what that function returns. For the same reason the in-use checks
(`daemon/src/disks.rs`) compare a file with a volume reference as the same
backing store (`labels`/`plan_cleanup_connected`), or deleting a volume would
take a running guest's disk when the two guests spell it differently.

**The daemon is the DHCP and DNS server; libvirt runs no dnsmasq.**
(`daemon/src/ipam/`.) Any libvirt network with an IPv4 `<ip>` is taken over:
its `<dhcp>` range and hosts, `<dns>` forwarders and `<domain>` move into
`/etc/virt-man/ipam.json`, and the definition gets `<dns enable='no'/>` and no
`<dhcp>`, so libvirt starts no dnsmasq. A supervisor (every 5 s, and at start)
does the takeover, binds DHCP (`0.0.0.0:67`, `SO_BINDTODEVICE` per bridge) and
DNS (gateway `:53`, UDP+TCP) for each active managed network, and prunes.

- **Every address is permanent.** A guest with no reservation is given the
  lowest free address in the range at DISCOVER, and that becomes a `dynamic`
  reservation tied to its VM UUID. Release, reboot, lease expiry and daemon
  restarts never move it; deleting the VM removes it (by absence, on the next
  supervisor pass). Deleting a reservation by hand just means the guest gets a
  new one at its next DHCP. Only MACs of guests on this host are answered.
- **DNS** answers A for reservation names (bare and `name.<domain>`), PTR for
  the subnet, NXDOMAIN under the domain, and forwards everything else to the
  network's forwarders or the host's `/etc/resolv.conf` (never to itself).
- **The network editor still speaks libvirt XML.** `GET …/xml` puts the stored
  IPAM back in (`ipam::xml::inject`); `PUT …/xml` takes it out (`strip`,
  `merge`, which keeps a reservation's `dynamic`/`vm` when it comes back
  unchanged) and redefines libvirt **only if what libvirt runs changed**
  (`same_definition`). A NAT network's `<bridge name>` must survive the editor
  (`networkXml`), or every save looked like a new bridge and restarted it.
- **Restarting a network unplugs its guests.** Destroying it deletes the
  bridge; libvirt does not reattach running guests' taps to the new one
  (measured). Every restart path calls `networks::reattach_taps`. That missing
  step is why "delete a reservation, then restore it" left a guest with no
  address: the delete redefined the network.

**Port forwards must be admitted past libvirt's own network filter.**
(`daemon/src/firewall.rs` `allow_forwarded`.) Our DNAT is correct on its own
and still does not work, because libvirt renders, per network, a chain that
accepts only replies to guest-initiated flows and rejects everything else. On
its **iptables** backend that is `LIBVIRT_FWI`:

```text
-A LIBVIRT_FWI -d 10.74.0.0/24 -o virbr0 -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT
-A LIBVIRT_FWI -o virbr0 -j REJECT --reject-with icmp-port-unreachable
```

and on its **nftables** backend the same two rules live in
`ip libvirt_network guest_input` (`oif "virbr1" ... ct state established,related accept`
followed by `oif "virbr1" ... reject`). Which backend a host uses is not
visible from our config — `/etc/libvirt/network.conf` leaves `firewall_backend`
commented and libvirt picks — and the two hosts that have run this both differ:
one is iptables, the local host is nftables. So `allow_forwarded` tries both.

A forward's first packet is NEW, not established, so libvirt throws it away
after our DNAT already rewrote it — the guest never sees the connection and
the client is told "connection refused", since the reject answers with ICMP
port-unreachable. Three things make this hard to read from the symptom:

- **Nothing in our own table can fix it.** A verdict in one chain does not stop
  a later chain of the same hook from rejecting. Measured on both backends: an
  `accept` in our `vm-forward` (`priority filter - 1`) does not save the packet,
  and neither does a DNAT with it. The accept has to sit *inside libvirt's
  chain*, ahead of its reject — and libvirt has no way to declare a port forward
  of ours (`network.rng` has no such element), so nowhere else will do.
- **A packet capture misleads.** Filtered to the forward's own port it shows
  only the inbound SYN and no reply at all, because the answer is ICMP.
  Attribute it with libvirt's own counter instead: the reject rule counts
  packets, and it rises by exactly one per connection attempt (34 → 37 → 39
  across five attempts on the iptables host; 0 → 3 locally on nftables).
- **The two backends spell the matcher differently.** nftables wants
  `ct status dnat` and refuses `ct state dnat` ("Could not parse conntrack
  state"); iptables takes `--ctstate DNAT`. Do not unify them.

`allow_forwarded` inserts an accept scoped to translated traffic only — `ct
status dnat`/`--ctstate DNAT`, exactly the connections we DNAT'd, leaving
libvirt's filtering of everything else intact. It runs on every firewall apply,
and the supervisor re-asserts it every pass, because **starting or restarting a
network, and a libvirtd restart, make libvirt rewrite those chains without it**.
Anything that recreates them silently re-breaks forwarding until the next pass.

**Access logs record every guest connection, cheaply.** (`daemon/src/ipam/flows.rs`,
`web/server/src/access.rs`.) The daemon subscribes to conntrack NEW/DESTROY
netlink events (one message per *connection*, not per packet; NAT already makes
the kernel track them) and turns on `nf_conntrack_acct` for byte counts. Its
DNS server records each guest lookup and the A records it got, so a connection
is shown by the name that led to it. Records sit in a bounded ring (50k) behind
`GET /flows?after=<seq>`; the web server pulls every 5 s into `access.db` with a
per-connection cursor (reset when a daemon restarts and its sequence drops).

- A connection is sent at open (`closed_ms` 0) and again at close with the same
  `ct_id` + `opened_ms`; the store upserts, so long sessions show from the start.
- Kept 14 days; a removed server's rows go at once, a deleted VM's are kept
  (that is when the log is wanted). The oldest 10% are purged, repeatedly, while
  the DB exceeds 2 GiB or the disk has under max(5 GiB, 5%) free.
- Not seen, by design: guest-to-guest traffic on one bridge, bridge-mode
  networks, and names behind a guest's own encrypted DNS.
- Measured: a 300-connection burst cost the daemon 80 ms of CPU in total.
- UI: `openAccessLogs(conn, vm?)`, from the server Firewall tab, each network's
  firewall editor and the VM Network tab; the button carries `.al-open`, which
  pushes it to the right of its action row. A Records dropdown picks DNS
  lookups / connections / both (`kind=`), and Live re-reads the newest page
  every 2 s (only on the newest-first ungrouped view; changing to any other
  view turns it off).

**Live packet capture exists only while someone watches.**
(`daemon/src/api/capture.rs`.) The daemon side is complete and still served:
`POST /domains/{vm}/capture` opens one `AF_PACKET` socket per guest tap, the
caller polls `…/capture/{id}?after=N` (no streaming through the proxy, same
pattern as the console), a session stops when not polled for 20 s, after 30 min
regardless, or on `DELETE`, at most 8 run at once, each holding at most 5000
packets truncated to 1600 bytes, and `…/pcap` returns what is held as a pcap
file. Nothing is stored. **The SPA panel for it was removed** — the button that
opened it, and `openPacketCapture` with its `CaptureLine` type and `.pc-*`
styles, are in this file's history if it is wanted back.

Rows go into a capture table as a whole `<table>` through `setHtml`: bare
`<tr>` markup is dropped by the sanitizer outside a table, which once showed a
capture counting packets while displaying none.

**Read `VIR_DOMAIN_XML_INACTIVE` for firewall/filterref work.** A running
guest's live XML omits filterrefs added since boot, so reading live XML loses
them on the next write.

**A domain can have two `<graphics>` devices** (one VNC, one SPICE). Never take
the first one: `parse_graphics` prefers VNC (the only protocol the built-in
viewer speaks) and the `.vv` endpoint prefers SPICE.

**SPICE has no browser console.** Do not try to add one — the last attempt is
documented in `DESIGN.md`. SPICE guests get a virt-viewer `.vv` file.

**A `.vv` file names a one-shot relay, not the guest's display.** Guest
displays listen on 127.0.0.1, so a file naming them only works on the
hypervisor. `GET …/console/vv` opens a relay port (`[::]:0`) that accepts **one**
connection, only from the address that asked, within 60 s, and splices it to
the display (`console.rs` `relay_once`). The web proxy sets `client` from the
browser's socket address and `host` to the name the viewer can reach, and
strips any the browser sent: a client-chosen `client` would open the relay to
anyone. The SPA fetches the file on click (`downloadVv`), never via a link,
because the relay is gone a minute later. The host firewall must allow
ephemeral TCP ports from viewers for this to work off-host.

**Daemon reachability is probed server-side** (`web/server/src/health.rs`,
every 3 s). Two failures in a row mark a server offline and one success brings
it back, so a daemon restart does not flap. `GET /connections` carries
`online`/`offline_since_ms`/`offline_error`. While a server is offline, every
view of it shows one offline panel (`renderOffline`), the sidebar greys its
last-known rows, and the 1 s poll does not call its daemon (it would only paint
the raw transport error). Recovery re-renders the open view by itself.
Transitions go to the activity log as `system`.

**ISOs can be fetched from a URL by the daemon itself** (`daemon/src/api/fetch.rs`,
`POST /pools/{pool}/fetch`, `GET /fetches`). The bytes never touch the web
server. The download spools to `/var/tmp` and is written into a new volume
only once complete: short of `Content-Length`, empty, or failing an optional
SHA-256, it stores nothing, and a write that fails deletes the volume. Existing
volumes are never overwritten (409). A fetch needs `Datastore.AllocateSpace` on the pool, and is refused to a user with a storage limit (its size is unknown up front).

**Console frames are long-polled.** `GET …/console/frame` parks in the daemon
until pixels exist. Do not add a client-side fetch timeout, and do not
introduce a poll interval.

## Live environment

- `virt-man-daemon` — HTTPS `[::]:8443`, token at `/etc/virt-man/token`
  (root-owned, mode 0600; `sudo cat` to read it).
- `virt-man-web` — HTTPS `[::]:8006`, admin / `password123`. A leftover
  `viewer` / `password123` RBAC test account exists and may want removing.

**Live suites provision their own guest.** `web/test/_fixture.mjs` runs at the
start of every guest-driven suite: it reuses an existing domain when the host
has one, and otherwise defines a disposable `vmtest-suite` (small disk, one
NIC, VNC) that it tears down again — including the disk — when the run ends.
Never reintroduce a dependency on a particular guest existing here; that made
the suites fail on any host with no VMs, and made them inspect whatever guest
the user happened to have. A suite that needs a different shape of guest
should extend the fixture rather than poke at ambient state.

Node rejects the web UI's self-signed cert, so point live suites at a loopback
sidecar (HTTP) rather than `https://127.0.0.1:8006`. Suites that must
exercise login instead need `NODE_TLS_REJECT_UNAUTHORIZED=0` plus credentials
(the loopback sidecar runs auth-disabled, so it cannot test the login path).
For throwaway state, run a second instance with its own `VIRT_MAN_WEB_DATA`
and seed it through `/auth/setup`.

**HTTP on the TLS port redirects, it does not fail.** `http://host:8006` used
to return a raw TLS alert record; the listener now peeks the first byte of each
connection (TLS always starts `0x16`) and answers plaintext with a 308 to the
`https://` URL. Only the TLS branch is wrapped, so
`VIRT_MAN_WEB_LOOPBACK_HTTP=1` mode still serves plain HTTP directly.

**The wildcard bind is `[::]`, not `0.0.0.0`.** `localhost` resolves to `::1`
alone on many systems (including this one — see `/etc/hosts`), so an IPv4-only
wildcard refuses `https://localhost:8006` while `127.0.0.1` works. Both
services map the `0.0.0.0` wildcard to `[::]`, which serves both families on
one socket given `net.ipv6.bindv6only=0` (the Linux default). An **explicit**
address is never rewritten, so a `127.0.0.1` bind stays loopback-only. `curl`
masks this by falling back to IPv4; a browser does not, so test with `curl -6`
or `ss -tlnp` (a dual-stack listener shows as `*:8006`).
