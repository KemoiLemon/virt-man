# AGENTS.md

Working notes for coding agents in this repo. Read `DESIGN.md` for *why* the
system is shaped the way it is; this file is *how to work in it without
breaking things*.

## What this is

Two parts, one product:

| Path | Build | Role |
| --- | --- | --- |
| `daemon/` | `cargo` | Rust + axum over direct QEMU. Runs on the hypervisor host. Token auth, HTTPS `[::]:8443`. |
| `web/` | `pnpm` + `cargo` | `web/src` is the SPA; `web/server` is `virt-man-web` (users, ACL permissions, ownership, quotas, API keys, TLS-pinned daemon proxy). HTTPS `[::]:8006`. |

Nothing in `daemon/` links libvirt. A VM is a direct QEMU process; a container
is run by LXC's own tools (`lxc-start`, not libvirt's LXC driver). The SPA still
writes libvirt-shaped XML for both, which the daemon parses into an explicit
argv and QMP calls, or an LXC config, without handing it to another manager.

### Where the SPA code lives

`web/src/main.ts` is only imports, hook wiring, the global keys listener and
`initIndicators(); boot();`. The rest is in these modules (`web/src/`):

| Module | Holds |
| --- | --- |
| `state.ts` | the shared `state` object, tree caches, `shown*` helpers |
| `util.ts` | pure helpers |
| `dom.ts` | `$`, `setHtml`, page/tab slide animations, theme |
| `dialogs.ts` | `toast`, `openModal`, `askText`, `askMultiline`, `askConfirm` |
| `nav.ts` | `syncUrl`, `currentRoute`, `applyRoute`, `renderCurrent`, `beginRender`/`stillCurrent`, `clearResourceSelection`, `leaveCurrentView`, `fastLoad`, drawer |
| `hooks.ts` | callbacks that break import cycles |
| `boot.ts` | boot, auth, app shell, offline panel, `canAdmin` |
| `admin-window.ts` | `openAdmin`, `closeAdmin`, `adminCtx` |
| `tree.ts`, `sidebar.ts` | sidebar tree and `select*Row`; sidebar chrome |
| `menus.ts`, `palette.ts` | `vmMenu`/`netMenu`/`poolMenu`/`serverMenu`/`rootMenu`, `menuAt`, context-menu and long-press listeners; Ctrl+K palette |
| `actions.ts` | delete/rename |
| `vm-page.ts`, `connection.ts`, `datacenter.ts`, `pool-page.ts`, `network-page.ts`, `containers.ts` | the view renderers (`renderVm`, `renderVmBody`, server view, ...) |
| `polling.ts`, `scroll-busy.ts` | the 1 s poll; scroll-busy gate |
| `transfers.ts`, `profile.ts` | uploads and URL downloads; profile |
| `access-logs.ts`, `firewall-editor.ts`, `hardware-editor.ts` | access logs, firewall and hardware editors |
| `create-vm.ts`, `create-resource.ts`, `network-form.ts`, `connection-dialog.ts`, `vm-form.ts`, `resource-common.ts` | create/edit forms and their shared helpers |

- A moved module **never imports `main.ts`**.
- A view needed across an import cycle is reached through `hooks`, which
  `main.ts` wires once.
- Shared reassignable state lives on `state` in `state.ts`; state private to one
  section stays in its module.

## Agent tools

These are Claude Code's tools. Use the dedicated one; **Bash is only for what no
other tool does**: running builds, tests, git and servers. It is never the way
to read, search, write or edit a file, to make a one-off script that edits
source, or to wait.

| Need | Tool | Not |
| --- | --- | --- |
| Read a file (or a range: `offset`/`limit`), or a log | `Read` (absolute path) | `cat`, `head`, `tail`, `sed -n` |
| Change part of a file | `Edit` (`old_string` must be unique and exact; `replace_all` for renames) | `sed -i`, `awk`, Python or heredoc rewrites |
| Create or fully replace a file, including a throwaway script | `Write` (an existing file must be `Read` first) | `echo >`, `cat <<EOF` |
| Run a build, test, git or one-shot command | `Bash` | — |
| Run a server, watcher or anything that does not exit | `Bash` with `run_in_background: true` | `cmd &`, `(cmd &)`, `nohup`, `setsid` |
| Wait for a background task or a condition | its exit notification, or `Monitor` (load it with `ToolSearch`) with an until-loop | `sleep` polling, re-reading its log in a loop |
| Stop a background task | `TaskStop` | `kill`, `pkill -f <name>` |
| Broad search across many files | `Agent` with `Explore` | chains of `grep`/`find` |
| Where a symbol is defined or used (TS and Rust) | `LSP` (load it with `ToolSearch`) | grepping for the name |
| Design a non-trivial change before touching code | `Agent` with `Plan`, or `EnterPlanMode` | starting to edit |
| Launch the app and see a change working | `Skill` `run` | hand-built server and browser commands |
| Review or tidy a finished change | `Skill` `code-review`, `simplify` | eyeballing the diff |
| Write or review Rust in `daemon/` or `web/server/` | `Skill` `rust-skills` | — |
| A decision that is the user's, not the code's | `AskUserQuestion` | guessing, or a prose question mid-task |
| Look up external docs or a package | `WebFetch`, `WebSearch` (load with `ToolSearch`) | `curl` |
| Many parallel agents | `Workflow`, only when the user asks for it in their own words | spawning agents unprompted |

A tool listed as deferred has no schema until `ToolSearch` loads it; calling it
before that fails.

- `Read` before `Edit`/`Write` on the same file, in the same session, or the call fails.
- Strip the line-number prefix `Read` shows when copying text into `old_string`.
- Independent tool calls go in one message so they run in parallel.
- Reference code as `path:line`.
- Prefer a narrow `Read` range over a whole file; several files here are tens of KB.
- Long-lived processes (the loopback `virt-man-web`, a stub daemon, a headless
  browser) are Bash calls with `run_in_background: true`, so Claude Code tracks
  them and `TaskStop` can end them. A process started any other way is
  invisible to it and ends up orphaned. Never stop one by name:
  `pkill -f virt-man-web` also matches the real service on `:8006`. Say what you
  started, and stop it before you finish. See "Live suites need a running
  backend" below.
- Do not `git commit` or `git push` unless the user asks in that message. An
  iterative fix is not finished work.

## Commands

```bash
# frontend: typecheck, build, unit tests (225, incl. the mobile CSS contract)
cd web && npx tsc --noEmit && npx vite build && pnpm test

# the test command is two halves: `test:build` (20 Vite SSR bundles) then
# `test:unit`. For one module, build just it and run its file:
cd web && npx vite build --ssr src/listbox.ts --outDir test/_build --logLevel error \
  && node --test test/listbox.test.mjs
# when test/_build is current, the suite itself is ~30 s:
cd web && pnpm test:unit

# web backend (157 tests: permissions/roles/key caps, routes, xmlcheck, shares...)
cd web/server && cargo clippy && cargo test

# live suites — drive the real app in jsdom against a running backend
#   arg is the base URL; defaults to http://127.0.0.1:8799
cd web && node test/live-ui.mjs      http://127.0.0.1:<port>
#   also: live-layout, live-tabs, live-hw, live-dc-vm, live-console, live-route,
#   live-admin-head, live-admin-res (the admin panel's per-section action in the
#   header, and its resource create/delete controls),
#   live-vmnet (needs sudo: checks tc and the IPAM store; boots its own guest),
#   live-ipam (needs sudo + the Alpine live ISO: a real udhcpc against our DHCP,
#   and the access log of what that guest then fetched),
#   live-history (needs the server to have been sampling for ~30 s),
#   live-empty (no args: runs its own server against a stub daemon with no resources),
#   live-ops (needs sudo: defines a SPICE guest, stops/starts the daemon),
#   live-ct (needs a root daemon with the lxc package and a NAT network it can
#   start: makes an Alpine container, drives its pages and console, deletes it),
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
cd daemon && cargo clippy --all-targets && cargo test --bins   # 290 tests

# web backend
cd web/server && cargo clippy --all-targets

# install (builds as $SUDO_USER so repo artifacts stay user-owned)
sudo ./install.sh daemon|web|all      # apt or apk: packages, rustup, pnpm, then the below
sudo ./daemon/install.sh && sudo ./web/install.sh
```

Live suites need a running backend. Start a throwaway loopback instance:

```bash
VIRT_MAN_WEB_LOOPBACK_HTTP=1 VIRT_MAN_WEB_BIND=127.0.0.1 VIRT_MAN_WEB_PORT=8799 \
VIRT_MAN_WEB_DATA=/tmp/vm-live VIRT_MAN_WEB_DIST=$PWD/web/dist \
  ./web/server/target/debug/virt-man-web
```

**Start it as a BACKGROUND task, never as a plain foreground command.** The
server runs until it is killed, so a foreground invocation blocks the agent
forever and swallows the rest of the turn — the suite it was meant to serve
never runs. Call `Bash` with `run_in_background: true` and the same env (not
`&` or a subshell: Claude Code must own the process), then check it is
listening (`curl -sf http://127.0.0.1:8799/ -o /dev/null`, or a `Monitor`
until-loop) before pointing a suite at it, and end that task with `TaskStop`
when the suites are done. The same applies to any watcher, dev server, stub
daemon or headless browser.

**Run live suites one after another, not together**, with a short wait between
them. They attach real console sessions, and back-to-back runs collide. Make the
wait a `Monitor` until-loop or the previous task's exit notification, not a
`sleep` call.

## Hard constraints

These are not style preferences. Violating them produces broken builds or
silently broken UI.

**No native OS chrome.** `<select>` is `display:none` behind a themed`.select-trigger`/`.select-popover` listbox (`web/src/listbox.ts`). Never use
`window.prompt`/`confirm`/`alert` — use `askText`/`askConfirm` in `dialogs.ts`.
The browser's right-click menu is replaced on resource surfaces: every sidebar
row, and in the content pane the cards, dashboard rows, tabs and page header
open a `.ctxmenu` (`web/src/ctxmenu.ts`). One builder per resource (`vmMenu`,
`netMenu`, `poolMenu`, `serverMenu`, `rootMenu` in `menus.ts`) serves every
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

**No mini-notes.** Do not put explanatory prose beside a control, above a table
or under a field (`<p class="hint">`, `.dsub`, `.hw-note` used as a note): no
"People you share with see this", no "The server downloads the image itself",
no restating what the labels already say. It is clutter that pushes the real
content down and has to be re-translated, re-styled and kept true. A panel
explains itself through its labels, its place in the app and the actions on it.
If something genuinely cannot be inferred, fix the labels instead. What stays:
dynamic readouts (live state, progress, errors, in-use lists) and an empty
state's own line — those carry information, not narration.

**A pane render that awaits must check it still owns the pane.** Bump
`beginRender()` at the top of any renderer, and `if (!stillCurrent(gen)) return;`
just before painting. The 1s poll can start a network/pool render and the user
can then click elsewhere before it resolves; without the guard the stale
response paints over the view they switched to ("the network page just
returned"). Every path that leaves a resource page must also run
`clearResourceSelection()`, or the poll re-renders the page they left.

**`setHtml()` is the only way HTML enters the DOM** (`web/src/dom.ts`). It
runs `DOMPurify.sanitize` *and* `enhanceSelects`. Assigning `innerHTML`
directly skips both, so the listbox silently stops working.

**A repaint of the same view patches the DOM, it does not replace it.**
(`web/src/morph.ts`, used by `setHtml` when the container was last painted at
the same `location.pathname`.) The 1 s poll used to rebuild whole panes, which
killed hover, CSS transitions, scroll and focus. Now a node that is still there
is the same node. Rules that follow from it:

- **A kept node is wiped of its handlers before you bind again** (`on*`
  properties and anything added with `addEventListener`, which `morph.ts`
  records), so bind after every `setHtml` as before. A listener that must
  outlive repaints goes through `addPermanentListener`.
- **Form controls read as the template says**, except the focused one, which
  keeps its value and focus.
- **Nodes the app inserts itself** (`.select-trigger`, `.sel-bar`) are skipped by
  the diff; a new kind needs adding to `owned()`.
- A different path starts from scratch (`innerHTML`), since nothing in it
  corresponds to the old view.

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

**Storage pools are daemon-owned directories.** (`daemon/src/pools.rs`,
`daemon/src/api/pools.rs`, `volumes.rs`.) A pool is a host directory plus
metadata in `<data_dir>/pools.json` (mode 0600, atomic temp+rename), keyed by
the canonical UUID the UI addresses it by, with a display `title`, `autostart`
and `active`. Only directory pools exist; LVM and filesystem pool types are
refused with "not implemented natively". A root is an
arbitrary absolute host directory, canonicalised and created on explicit
create/start, refused when it overlaps another pool or falls in a small deny
list (`/proc`, `/sys`, `/dev`, `/run`, `/etc`, `/root`, `/boot`, `/tmp`,
`/var/tmp`, the daemon data dir, and the web server's `/var/lib/virt-man`;
`/home` is allowed). **The stored root is revalidated on every use**: a root
that is now a symlink, or resolves somewhere else, is refused with "re-point the
pool" rather than followed — otherwise a link planted in its place would redirect
scans, uploads and deletes.

A read is a directory scan, so a file dropped in by `cp`, aria2 or a sync is
visible immediately and there is nothing to refresh. Scans list regular files
only (a symlink is skipped, never followed), skip dot-prefixed names (the
atomic-write temporaries) and names the API could not address. `capacity` is the
qcow2 virtual size from the header, falling back to the file length;
`allocation` is `st_blocks*512`; pool capacity/available come from `statvfs`. The
sidebar's poll uses a header-free block walk, not the volume list. Startup never
creates a missing autostart root — an absent filesystem must not look like an
empty pool — it only leaves it inactive; an explicit start recreates it.

**Pool-used figures share the filesystem's scope.** `allocation` counts only the
files directly in the pool root, while `capacity`/`available` describe the
whole filesystem, so the two cannot be shown in one used/free/total row. Every
surface uses `capacity - available` (`web/src/pool-usage.ts`), and chart history
samples the same figure (`available` travels in `/metrics`); showing allocation
beside filesystem total made a 931 GiB disk with 222 GiB used read as 0.0%.

Volumes are files: raw ones are sparse (`set_len`), qcow2 ones are made by
`qemu-img` with the target pre-created 0600, so its own 0644 default never lands.
Upload and fetch write a dot-prefixed temp **in the pool root**, hash while
copying, fsync and rename over the target, so a failed transfer leaves an
existing volume untouched; an existing name is never overwritten (409). Deleting
a pool removes only its definition, never its files, and is refused while a
guest still holds one of its volumes. Pools are not imported: this host is
greenfield, and `pools.json` is the only source of truth.

**The URL is the view.** `web/src/route.ts` maps views to paths (`/`,
`/servers`, `/guests`, `/networks`, `/storage`, `/<node>`,
`/<node>/vms|networks|storage|firewall`,
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
or is a Datacenter tab (`route.ts` `DC_TABS`: `/guests`, `/networks`,
`/storage`) — then by id, or a reload would get an admin page or
the app's own Datacenter tab. **Keep `RESERVED` in
`route.ts` in step with the server's top-level routes and `DC_TABS`.**

**The Datacenter lists every server's resources, and creating one does not
select a server.** Guests, Networks and Storage are tabs over the
tree's per-server caches, each headed by the same `quickStats` cards, and each
card carries its own owning server (`data-*-conn`), so an action on a card
names the daemon it belongs to instead of the app's `selectedConn`. The create
buttons there pass an explicit `conn` to the form (`withDaemon`) and the view
stays put: a create is not a request to switch the whole app to that daemon.
The card builders (`netCard`, `poolCard`) are shared with a server's own tabs,
which is why they take the owning connection as an argument.

**The admin panel creates and deletes resources too.** (`web/src/admin.ts`,
`web/src/admin-window.ts` `adminCtx`.) The VM, Network and Storage Pools sections are
each tabbed by daemon, and each row carries a delete action; Storage Pools also
has a create button (pools are administrator-defined and own no user, so no
"for user" step). Every action goes through the resource's *own* form or delete
with the connection named explicitly and an `after` callback that repaints the
admin page — it never navigates to the resource. Things that bite:

- **`AdminCtx` is the only surface `admin.ts` has into the app.** A new
  create or delete needs a method there (`createPool`, `deleteResource`) and a
  binding in `adminCtx`, not an import back into the page code.
- **`deleteResource` reuses `deleteVmOn`/`deleteNetworkOn`/
  `deletePoolOn`/`deleteVolumeOn`**, which are the same functions the guest,
  network and pool views call, so the two surfaces cannot drift — including the
  running-guest stop below.
- **A pool has no `AdminResource.owner`**, so `/resources` skips it in the
  ownership annotation; it is listed from the daemon's own pool list, which is
  why a freshly created pool shows up without an ownership record.

**Deleting a running guest stops it first.** (`daemon/src/api/domains.rs`
`delete`, `daemon/src/qemu.rs` `powerdown`/`force_stop`.) A delete asks a
running guest to power down, force-stops it if it does not, and only then
removes the definition and cleans up storage. The delete response carries
`stopped` so the UI can say a running guest was stopped. The SPA wraps the
guest delete in `runTask` because that stop can take seconds.

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
the VM/network lists, and XML. The stats series ignore a repeated
`sampled_at_ms` for the same reason a cached-then-fresh paint would otherwise
push a false 0% point.

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

- Rows are keyed by the QEMU manager's **UUID**, so a renamed guest keeps its
  history and a new guest reusing a deleted one's name starts empty.
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
- **The admin panel is a window, not a view** (`web/src/admin.ts`,
  `admin-window.ts` `openAdmin`/`closeAdmin`). The admin button opens a modal that grows from it
  over the page that was open; that page keeps its place, polling and URL, and
  the window has no URL of its own. The button toggles it. Sections are listed
  in the window's `.admin-nav` (`ADMIN_GROUPS`; a new section must go in one)
  and paint into `#admin-area`, reading on demand with their own generation
  (`adminGen`), independent of `beginRender()`. An old `/admin/<section>[/<tab>]`
  link still parses (`route.ts`) and opens the window over the datacenter.
  Servers are managed there (Daemons); VMs, Networks and Storage Pools are
  tabbed by daemon. Creating one for a user opens the normal form above the
  window with an explicit `conn` and an `after` callback, and never navigates.
  Tests look for the topmost `.modal-back.open`, since the window is one too.
  Section changes slide vertically and tab changes sideways (`renderAdmin`).
  The slide starts from a `MutationObserver` on the first real paint, not when
  the painter's promise resolves, or a frame shows the page in place before the
  animation restarts; the pending direction (`adminEnter`, `adminTabDir`) is
  cleared only when a slide plays; and `sk-reveal` is removed first, since it
  animates the same transform.

**Guests, networks and pools are named by their UUID.** A new one's
manager id is its UUID and what the user typed is its `<title>` (a pool has no
definition of its own: its title lives beside it in `pools.json`). URLs name
them by UUID; old name links still work. Things that bite:

- **Every daemon route takes a UUID or a name.** Each handler resolves the
  stored key itself (a guest by id or title, a pool or network by UUID or
  title), so no routing middleware rewrites the path.
- **The web proxy resolves a UUID to its name before the permission check**
  (`api/proxy.rs` `canonical_path`). Grants, ownership and suspension are keyed
  by name; checking the UUID path instead let a user carved out of a guest
  reach it by UUID. `test/live-uuid.mjs` pins it.
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
looked fine in code review and were only caught by measuring: a console that
connected but never rendered (decoded the frame bytes), a graphics parser that
silently fell back to loopback (asserted the parsed address).

**Run the narrow check first.** `pnpm test` rebuilds 20 Vite bundles before it
runs a test; that build is the slow half. For a change to one module, build
that module and run its test file, as shown under Commands. Use
`pnpm test:unit` once `test/_build` is current, and keep `pnpm test` for the
final gate.

**Long runs write to a log, in the background.** Redirect output to a file
(`pnpm test > /tmp/test.log 2>&1`) and start it as a background task, then read
the log as it grows. Piping a live run through `tail` hides the current test
and every result until exit, so a runner that finished but never exited looks
identical to a slow one.

**A jsdom test that boots the SPA must close its window on failure too.** Push
each `JSDOM` window onto a list and close them in a file-level
`after(() => windows.forEach((w) => w.close()))`. The app schedules 1 s timers;
a failed assertion that skips `window.close()` leaves those timers running and
Node waits forever after the last test (see `profile-limits.test.mjs`).

**Browser layout checks need a real engine.** jsdom has no layout. Serve a
harness page with the real CSS bundle and the Vite-built module, screenshot it
with headless Firefox (`--profile /tmp/vm-ffprofile`), and decode the PNG to
compare rectangles. Firefox takes the screenshot at the load event: hold that
event with a slow **image** and cache-bust its URL. A slow `<script src>` in
`<head>` blocks parsing, so the page never renders; an image loads without
blocking. To drive the real app, build to a throwaway `--outDir`, run a
loopback instance against the live daemon with its own `VIRT_MAN_WEB_DATA`,
and load the route plus open the control inside a same-origin iframe.

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
- **Commit and push** finished work — do not leave it uncommitted in the
  working tree. Branch `dev` on
  `ssh://git@git.kemoi.top:2222/kemoi/virt-man.git`. Check `git remote -v` and
  `git status -sb` before pushing: this clone tracks `github/dev`, and the
  git.kemoi.top host is not reachable from every environment.
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

**A VM's networking lives on its Network tab, and applies live.** Interfaces,
their IPAM reservations and rate limits, and the guest firewall are one tab
(`/<node>/vms/<vm>/network`; the old `/firewall` path redirects). The daemon
applies every change to the running guest *and* its definition
(`daemon/src/api/nics.rs`, `networks.rs` `put_host`/`delete_host`):

- Rate limits are `<bandwidth>` in KiB/s (taken as Mbit/s), applied to the
  running guest and its definition. They are enforced at once — the daemon
  installs an htb class and an ingress policer on the host tap; `live-vmnet`
  reads them back with `tc`, not from the XML.
- IPAM changes only the daemon's store (below); nothing is restarted.
  `live-vmnet` checks the bridge's ifindex is unchanged.
- Hot-add goes through QMP. A NIC is `netdev_add` then `device_add`, a disk is
  `blockdev-add` then `device_add`; the request names no PCI address, so QEMU
  picks the slot. A failed add rolls back the host resources it made, and a
  failed persist rolls the live device back out.
- Hot-remove is a *request* to the guest OS (ACPI eject). The API reports
  `pending` while the guest still holds the device; never claim it is gone.
- The hardware editor rebuilds every `<interface>`, so it carries `<bandwidth>`
  through (`devicesFromXml`/`deviceXml`), or a hardware save would lift limits.

**The video adapter is a device, and its model decides its QEMU options.**
(`web/src/hardware.ts` `videoXml`, `HW_CATALOG` "video"; `daemon/src/domainxml.rs`.)
It is the one `<video>`, added like any other device, so the model and its
options are editable without the XML tab; a guest with none gets a `virtio` one
seeded when the editor opens, so the device list stays complete. `domainxml`
maps the model to a QEMU device and keeps only the attributes that device
accepts — `ram`/`vgamem`/`vram64` are QXL's, `device`/`blob` are virtio's — and
refuses a mismatch when the definition is saved rather than at start, so the
form hides fields by model rather than writing whatever is set. Two things that
cost real time:

- **`<acceleration>` is a child of `<model>`, not a sibling.** Read as a
  sibling it is ignored, leaving a `virtio-vga-gl` device defined without 3D
  while the XML says otherwise. `videoXml` writes it inside `<model>`.
- **A `*-gl` device and `accel3d='yes'` are one setting.** The form moves them
  together (choosing a gl device turns 3D on, and turning 3D on upgrades the
  device), and `domainxml` refuses a definition where they disagree.

`rendernode` is a host DRI path and a `vhost-user-*` device reads a host
chardev, so both stay administrators' (`xmlcheck`'s `host_ref` scan and the
`device` allowlist); every model and every guest-side option is a non-admin's.
`accel3d` is harmless on any display, but a `*-gl` guest needs a GL-capable
display (`egl-headless`, or SPICE with `<gl enable='yes'/>`) to actually start;
this app writes neither, so a -gl guest defines and then fails at QEMU start
with its own reason.

**Guests are given disks by path, not by pool+volume.** (`daemon/src/domainxml.rs`,
`web/src/xml.ts`/`hardware.ts`.) The manager launches QEMU itself and opens every
`<disk>` by its `<source file=...>`, so a disk's source is an absolute file path
and nothing else. `<disk type='volume'>`, a `<source pool=... volume=...>` and a
block-device source are refused by the parser with "attach it by its absolute
path instead" rather than guessed at; the SPA fills the path from the volume
API's `path` (it creates the volume, then names it) or from the volumes list
when one is picked. `driver type` travels with the disk — guessing qcow2 for a
raw image makes QEMU parse it as the wrong format.

`devicesFromXml` **claims file-backed disks by path**. It used to skip them,
which silently deleted them: the hardware editor strips every `<disk>` block and
re-adds only what that function returns. For the same reason the in-use checks
(`daemon/src/disks.rs`) compare a file with a volume reference as the same
backing store (`labels`/`plan_cleanup_connected`), or deleting a volume would
take a running guest's disk when the two guests spell it differently.

**Networks are the daemon's own; dnsmasq is their DHCP and DNS server.**
(`daemon/src/ipam/`, `daemon/src/hostnet.rs`, `daemon/src/api/networks.rs`.)
Nothing is read back from another network manager: the store at
`<data_dir>/networks.json` (default `/var/lib/virt-man-daemon/networks.json`,
mode 0600) is the source of truth for mode, subnet, range, DNS and
reservations.

- **One bridge per network, named from its id.** A managed network gets
  `vbr` + the first 12 hex of its UUID (`hostnet::bridge_name`), 15 characters,
  the Linux limit; `Mode::Bridge` borrows the operator's bridge and is only
  checked, never created or deleted. Two ids that would derive one bridge are
  refused by the API (`check_bridge`), or one Start would flush the other's
  address.
- **dnsmasq runs per active network**, config in
  `/run/virt-man-daemon/dnsmasq-<id>.conf`, bound to the bridge. DHCP is
  static-only — `dhcp-range=<start>,static`; `start,end,<lease>,static` is
  rejected by dnsmasq ("bad dhcp-range") — so only `dhcp-host` entries are
  handed out, and only for MACs of guests whose NIC is on *that* bridge. Every
  named reservation still answers DNS (`host-record`). `hostnet`'s test runs
  `dnsmasq --test` over the generated file.
- **Every address is permanent.** A supervisor (every 5 s) gives each guest
  appearing on a network the lowest free address as a `dynamic` reservation
  tied to its VM UUID, drops a reservation whose guest is gone, and folds
  dnsmasq's lease file into `seen_ms` (expiry − lease time). Deleting a
  reservation by hand means the guest gets a new one at the next pass.
- **A stopped network answers nothing.** No bridge means the supervisor stops
  its dnsmasq, and the firewall renders no rules for it. Networks are never
  removed from the store by the supervisor; only the API deletes.
- **Only what runs reprograms a network.** `PUT …/xml` stops and starts the
  bridge only when the bridge, subnet or mode changed (decided by
  `bridge_up`, not dnsmasq health). A range, forwarder, domain or reservation
  edit just restarts dnsmasq, so a guest is not unplugged for a settings
  change. A failed start puts the old definition back.
- **The editor still speaks libvirt-shaped XML.** `netxml::render` writes it
  (name and uuid are the id, the derived bridge is included, `parse` validates,
  `merge` keeps autostart, the lease and reservations the editor did not see).
  A title must be unique and not equal to another network's id, so a
  display-name link resolves to exactly one network.
- **The access log's DNS half is read from dnsmasq's query log**
  (`daemon/src/ipam/dnslog.rs`). Each pass tails `<data_dir>/dnsmasq-<id>.log`
  for every served network by byte offset, pairs a `reply`/`cached` with the
  oldest pending `query[A]` for that name, records the first answer as a
  `kind=dns` row, and remembers the name→address mapping a following connection
  is attributed by. It starts at the end of a log it first sees, so a restart
  does not replay old lookups; a shorter file is a rotation, read from the
  start. A second A record names an address without a second row.

**A guest's NIC names its network's bridge.** (`daemon/src/api/nics.rs`,
`web/server/src/xmlcheck.rs`, `web/server/src/guard.rs`.)
A VM's interface is
`<interface type='bridge'><source bridge='vbr…'/></interface>` — there is no
separate network object to attach to, only the bridge the network runs — so
`nics::add` resolves the key a request names (a network's id or title) through
`Ipam::bridge_for`, and `GET …/interfaces` maps the bridge back to the network
key. `xmlcheck` records the source bridge in `DomainSpec.bridges`; the proxy's
`check_network_bridges` then requires `Network.Use` on **any** network using
that bridge and refuses an unknown bridge outright, so an arbitrary host bridge
is unreachable for a non-admin. A `Mode::Bridge` network may share its operator
bridge, so a grant on any network on it is enough, and `network_for_bridge`
answers the lowest stored key so a guest reads back the same network every time.

**The `virt-man` nftables table owns NAT, isolation and port forwarding.**
(`daemon/src/firewall.rs`.) There is no other firewall backend: the daemon
renders one IPv4 table for its own networks and applies it as a single `nft -f`
transaction (`add table`, `flush table`, then the rendered block), so a failed
script leaves the previous rules in force rather than an empty table.

- **Base rules follow the bridge, not dnsmasq's health.** For an active NAT
  network: accept from its bridge, accept established/related and `ct status
  dnat` back in, drop the rest, and masquerade its subnet out of any interface
  but its own (excluding the subnet itself and 224.0.0.0/24). An isolated
  network drops both directions with `drop` (timeouts, not ICMP), but still
  accepts `ct status dnat`, so explicit port forwards reach their guests. A
  `Bridge` network gets no rules: the operator owns that bridge and its policy.
- **Cross-network traffic is guarded, not ordered.** A flat chain would let one
  network's `iifname ... accept` end evaluation before another network's
  `oifname ... drop` is reached. Every accept for our own traffic therefore
  excludes the set of all managed bridges (`oifname != { "vbr1", "vbr2" }`), so
  guest-to-guest traffic between two networks is dropped whatever order the
  definitions are stored in.
- **A network whose bridge is down emits nothing** — no rules, no policy
  defaults, no DNAT ports, no masquerade. The network API must call
  `api::firewall::apply_store` on every lifecycle change; nothing else
  re-installs a network's rules.
- **Forwarding is enabled, never cleared.** `ip_forward` is host-wide and shared
  with Docker and other services; `apply_store` writes
  `1` when an active NAT network exists and otherwise leaves it alone.

**Containers are LXC, driven directly.** (`daemon/src/lxc.rs`, `ctxml.rs`,
`ct.rs`, `api/containers.rs`, `api/ct_console.rs`.) No libvirt: the daemon
runs the stock `lxc-start`/`lxc-stop`/`lxc-console` (the `lxc` package; the
installer disables its `lxc-net` and `lxc.service`). Things that bite:

- **`<data_dir>/containers.json` is the definition; the LXC config is
  derived.** Each record is the `<domain type='lxc'>` XML the SPA edits
  (`ctxml` parses and renders it) plus `autostart`. `lxc.rs` renders
  `/run/virt-man-daemon/lxc/<id>/config` from it at *every* start and never
  reads it back. Edit the definition, never the config.
- **The rootfs is a raw ext4 image in a pool, `ct-<uuid>.raw`**, loop-mounted
  by the daemon at `/run/virt-man-daemon/ct/<uuid>` (0711 all the way down,
  or a mapped root cannot pivot into it) and given to LXC as `dir:`. The web
  server records storage by that volume name. The in-use checks see it as a
  `<disk>` (`Manager::disk_definitions`), so a volume delete refuses it.
- **Unprivileged = an `<idmap>` onto a whole 65536-id block at or above
  2^30**, unique per container, allocated from the other definitions. The
  daemon refuses any other mapping (`target='0'` would be host root); no
  `<idmap>` is privileged, which only `CT.Privileged` may set.
- **Running = the monitor pid `lxc-start -p` wrote (checked by its `[lxc
  monitor] <path> <id>` title) has a child in `lxc.payload.<id>`.** The pid
  file holds the *monitor*, not init — measured. Containers survive a daemon
  restart (`KillMode=process`, LXC's own top-level cgroups).
- **Shutdown signals init by what `/proc/<init>/exe` is**: busybox ignores
  LXC's SIGPWR and powers off only on SIGUSR2 (an Alpine container otherwise
  sat out the whole timeout), systemd gets SIGRTMIN+3. Force is `lxc-stop -k`.
- **No NIC means `lxc.net.0.type = empty`.** Without any `lxc.net` entry LXC
  shares the *host's* network namespace.
- **Limits are cgroup v2 files, so they all apply live** (`memory.max`,
  `memory.swap.max` when swap is accounted, `cpu.max`, `cpuset.cpus`); the
  hardware PUT reports them `applied`, never `pending`. A NIC add/remove is
  `pending` on a running container (veths are made at start); its rate limit
  is live, on the host veth `vc<11 hex>`.
- **A console resize is written to the container's own `/dev/console`**, the
  fd the LXC monitor holds (matched by dev+inode). `lxc-console` does not
  carry our pty's size through its proxy pty: `stty size` stayed 24 80.

**The daemon shell is a host login, and only an administrator's.** (`daemon/src/api/shell.rs`.) A Shell tab on a server page opens a login shell on the hypervisor host. Things that bite:

- **The pty runs the host's `login(1)`; the terminal is the login UI.** The daemon never sees a password and runs no PAM conversation of its own — it only starts `login` in a pty. `login` prints `login:`/`Password:`, authenticates through the host's PAM and execs the account's shell. So the daemon must run as **root** (its normal mode): `login` refuses a non-root caller with "for superuser only", and a non-root daemon cannot `setuid`/`setgroups` to the target account anyway. Do not reintroduce a credentials form in the SPA.
- **`/connect` takes no body fields but the optional size.** If a `login` prompt never appears, check the daemon is root before anything else; a non-root daemon starts `login`, `login` exits, and the session closes immediately with "the session ended".
- **Every `/shell` route is `Special::AdminOnly`** in `web/server/src/routes.rs` on `Sys.Modify`. There is no narrower privilege, on purpose — nothing in the ACL honestly means "a host shell". A route there that is missing from the table is refused for everyone.
- **A session belongs to the page.** A random id names it; `pagehide` in `terminal.ts` beacons a disconnect, `leaveCurrentView()` and `renderConn()`'s `connTab !== "shell"` guard close it on any navigation, and an unread session closes after `IDLE` (30 s). The pump `kill(-pid, SIGHUP)`s the whole process group, so background jobs die with the page too.
- **The child after `forkpty` only calls async-signal-safe functions** (`close_range`, `dup2`, `fcntl`, `execve`, `write`, `_exit`) and parks its error pipe at fd 3 before sweeping the rest, so a failed `execve` is reported rather than looking like a clean start. `dup2` clears close-on-exec on fd 3, so it is set back: otherwise the program inherits the pipe and `connect` blocks until it exits. The pty machinery is `daemon/src/pty.rs`, shared with the container console.
- **`daemon/src/lib.rs` exists for integration tests.** A shell test needs a root daemon, so there is no runnable-by-default suite here; drive `/connect` against a root daemon and read back the `login:` prompt.

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

**A guest has one `<graphics>` device, either VNC or SPICE.** `domainxml`
refuses a second display adapter, so the definition's one device decides the
console: the built-in viewer for VNC, the `.vv` handoff for SPICE. `pick_graphics`
still selects by kind, preferring SPICE when no kind is named.

**SPICE has no browser console.** Do not try to add one — the last attempt is
documented in `DESIGN.md`. SPICE guests get a virt-viewer `.vv` file.

**A `.vv` file names a short-lived relay, not the guest's display.** Guest
displays listen on 127.0.0.1, so a file naming them only works on the
hypervisor. `GET …/console/vv` opens a relay port (`[::]:0`) that accepts
connections for 60 s, and only from the address that asked, splicing each to the
display (`console.rs` `relay_once`). It keeps accepting rather than taking one
connection: SPICE opens a channel per socket (main, display, inputs, …), and a
one-shot relay left virt-viewer unable to display anything. Channels already
open when the minute ends keep working; new ones stop. The web proxy sets
`client` from the browser's socket address and `host` to the name the viewer
can reach, and strips any the browser sent: a client-chosen `client` would open
the relay to anyone. The SPA fetches the file on click (`downloadVv`), never
via a link, because the relay is gone a minute later. The host firewall must
allow ephemeral TCP ports from viewers for this to work off-host.

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
server. The download spools to `/var/tmp` and is copied into a new volume only
once complete: short of `Content-Length`, empty, or failing an optional SHA-256,
it stores nothing, and the copy is a dot-prefixed temp in the pool root that is
renamed into place (a failure removes the temp, never a target). Existing
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
