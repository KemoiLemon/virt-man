# AGENTS.md

Working notes for coding agents in this repo. Read `DESIGN.md` for *why* the
system is shaped the way it is; this file is *how to work in it without
breaking things*.

## What this is

Two parts, one product:

| Path | Build | Role |
| --- | --- | --- |
| `daemon/` | `cargo` | Rust + axum over libvirt (`qemu:///system`). Runs on the hypervisor host. Token auth, HTTPS `[::]:8443`. |
| `web/` | `pnpm` + `cargo` | `web/src` is the SPA; `web/server` is `virt-man-web` (users, RBAC, API keys, TLS-pinned daemon proxy). HTTPS `[::]:8006`. |

## Commands

```bash
# frontend: typecheck, build, unit tests (70)
cd web && npx tsc --noEmit && npx vite build && pnpm test

# web backend (24 tests: permissions/roles/key caps + redirect URL building)
cd web/server && cargo clippy && cargo test

# live suites — drive the real app in jsdom against a running backend
#   arg is the base URL; defaults to http://127.0.0.1:8799
cd web && node test/live-ui.mjs      http://127.0.0.1:<port>
#   also: live-layout, live-tabs, live-hw, live-dc-vm, live-console
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
The browser's right-click menu is suppressed inside the sidebar too: the
resource tree opens its own `.ctxmenu` (`web/src/ctxmenu.ts`) with the row's
actions. Outside the sidebar the native menu is left alone, so copy/paste
still works in the XML and log panes.

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

**Icons: named imports only** (`web/src/icons.ts`). Importing the whole Lucide
set takes the bundle from 81 kB to 670 kB. Stroke is defined once in `.ico >
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
cannot be forged without the `users` scope. Progress during an operation is
still hand-rolled per call site (`runTask`'s `set`); there is intentionally no
client-side task list to keep in sync.

**Destructive daemon calls are logged by the proxy, not the SPA.** Every
`DELETE` proxied to the daemon is recorded with a human label, its outcome, and
a detail line — for a VM delete that includes what happened to its storage
(`removed 1 disk(s); kept 1 (iso)`). It lives in `web/server/src/api/proxy.rs`
so that *every* caller is covered, including a direct `curl` or an API key, not
just the buttons we happen to render. A failed deletion keeps the daemon's own
reason, so "who deleted this volume, and did it work" is answerable after the
fact. Volume deletions previously left no trace at all, which made a missing
ISO impossible to explain.

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

**Read `VIR_DOMAIN_XML_INACTIVE` for firewall/filterref work.** A running
guest's live XML omits filterrefs added since boot, so reading live XML loses
them on the next write.

**A domain can have two `<graphics>` devices** (one VNC, one SPICE). Never take
the first one: `parse_graphics` prefers VNC (the only protocol the built-in
viewer speaks) and the `.vv` endpoint prefers SPICE.

**SPICE has no browser console.** Do not try to add one — the last attempt is
documented in `DESIGN.md`. SPICE guests get a virt-viewer `.vv` file.

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
