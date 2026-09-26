# DESIGN.md

Why virt-man is shaped the way it is. `AGENTS.md` covers how to work in the
repo; `README.md` covers installation. This file is the reasoning, including
the decisions that were reversed and why.

## The problem

Proxmox is too heavy for a personal hypervisor; raw `virsh` is too light. This
is the middle: a real UI over libvirt, usable from a browser on the LAN and from
a desktop app, without running a distro-sized management stack.

## Shape

```text
browser ──HTTPS(8006)──┐
                       ├── virt-man-web ──HTTPS(8443, pinned cert)── virt-man-daemon ── libvirt
desktop app ──HTTP─────┘   (auth, RBAC,                              (on the hypervisor)
  (loopback sidecar,        daemon proxy)
   auth disabled)
```

Three processes, three deliberate choices:

**The daemon owns libvirt and nothing else.** No users, no sessions, no
policy — one token, one hypervisor. That keeps the privileged component small
enough to reason about. It listens on `0.0.0.0:8443` because remote management
is the point; TLS is what makes that acceptable.

It also serves the same API on a Unix socket (`/run/virt-man/daemon.sock`) for
local tooling that should not go through TLS. The socket is mode 0666, because
the **token is the gate, not the file permissions** — both listeners share one
router and one auth middleware, so an unauthenticated request over the socket is
401 exactly as it is over HTTPS. (Verified, not assumed.)

**The web backend owns identity.** Users, roles, API keys, and the set of known
daemons live here. It is also the only thing that talks to daemons, so **the
daemon token never reaches the browser** — the backend attaches it server-side
on every proxied call.

**The desktop app is not a thin client.** It embeds the same SPA but runs its
own `virt-man-web` as a loopback sidecar with auth disabled, storing daemon
connections in `~/.local/share/virt-man`. So the app is standalone: it manages
hypervisors without any remote web UI existing. "Connect to a webui" simply
navigates the window to a remote instance.

### One frontend, two hosts

`web/src` builds once and serves both. The alternative — a shared component
library with two shells — doubles the surface for a UI this size. The cost is
that no code may assume it is in a browser *or* in Tauri; the few places that
care go through `isTauri()`.

## Certificate pinning, not a CA

Every daemon connection stores that daemon's exact certificate, confirmed by
SHA-256 fingerprint when it is added.

A CA would mean provisioning trust before a daemon is useful. Pinning means an
already-running daemon can be adopted by pasting its token and confirming a
fingerprint — no PKI, no system trust store, and no "just accept the warning"
step that trains the user to ignore TLS errors. Only the web backend (or the
app's sidecar) holds pins, so browsers never see a self-signed certificate at
all.

## Auth and RBAC

Scope-based, with a per-user override on top of the role:

| Role | vms / networks / storage / connections | users / settings |
| --- | --- | --- |
| Admin | write | write |
| Operator | write | none |
| Viewer | read | none |

A per-user scope entry beats the role, so an admin can be deliberately
restricted. `enforce(state, user, scope, write)` is the single check, and the
proxy derives the scope from the request path — so a new daemon endpoint is
governed automatically rather than needing its own rule.

The local app disables auth entirely (`auth_disabled`). A single-user desktop
app gains nothing from a login screen, and the sidecar is loopback-only.

## The console: the most-revised decision

This went through three designs. The final one is a port of
`/home/kemoi/Projects/Personal/vm-man`.

### What failed

**noVNC in the browser over a WebSocket byte pipe.** Worked for VNC, but the
daemon had to proxy raw TCP, and it put a full VNC client in the page.

**spice-html5 for SPICE guests.** Never rendered. Debugging found three real
bugs — an unnegotiated `binary` subprotocol (the client failed the connection
per spec), a sizing conflict with the library's own canvas height, and a
`sendCtrlAltDel()` called with an argument it does not take. Fixing all three
got SPICE as far as streaming display-channel frames, and it still was not a
console anyone would want to use. spice-html5 also has *no clipboard
implementation* — it defines the `VD_AGENT_CLIPBOARD` constants and never uses
them.

The lesson was not "fix harder". It was that a browser is a bad place for a
remote-framebuffer client.

### What it is now

**The RFB client lives in the daemon** (`daemon/src/rfb.rs`, ported near
verbatim from vm-man: RFB 3.8, `std` only, no VNC crate). It holds the guest
framebuffer in memory and hands out **only the rectangle that changed**, as raw
BGRA. The browser base64-decodes that into a canvas with `putImageData`. There
is no VNC client, and no WebSocket, in the page.

Three properties make this fast, and all three were measured on a live guest:

- **Damage tracking.** QEMU's Raw encoding resends the whole screen when one
  character changes, so the daemon compares against what it already has and
  reports only differing rows. An idle guest sends `8x2` rectangles of ~0.1 kB
  instead of 3 MB screens.
- **Long polling, not an interval.** `GET …/console/frame?since=N` parks on a
  condvar inside the daemon until a newer frame exists, so pixels leave on a
  request that was *already waiting*. One hop per frame, no poll delay.
- **No compression.** The daemon runs on the hypervisor, so the socket to the
  guest is local. Compressing cost more time than sending raw.

Scaling is **local only**: the canvas backing store stays at guest resolution
and CSS scales it, so the guest is never asked to resize. Fit never upscales
past 1:1, so a small guest stays crisp.

### SPICE

Not supported natively, by choice. SPICE is not RFB, and the browser has no
usable client. Those guests get a **virt-viewer `.vv` file** instead of a
console that silently does nothing. A guest offering both servers is the common
case: the built-in viewer takes VNC, the `.vv` takes SPICE (virt-viewer handles
it better — audio, clipboard, USB).

The trade-off: SPICE users leave the browser. That is honest, and it is better
than the two previous designs, which pretended to work.

## Why a hand-written RFB client

It is ~830 lines of `std` with no dependency, implementing exactly three
encodings (Raw, CopyRect, and the DesktopSize pseudo-encoding). A crate would
bring a general-purpose client where this needs a loopback-only viewer tuned for
latency. It also came pre-tested from vm-man — the port kept all 9 unit tests.

Known ceilings, deliberate: **security type `None` only** (no VNC password),
and no ZRLE/Tight. Both are fine against a local QEMU and both would be
straightforward to add.

## Frontend

No framework. `web/src/main.ts` renders with template strings through a single
`setHtml()` choke point that sanitises (DOMPurify) and swaps native `<select>`
elements for a themed listbox. For a UI of this size, a framework would be more
machinery than the thing it manages. The bundle is ~111 kB.

The theme is ported from vm-man: square, monospace, every colour resolved from
CSS variables on `html[data-theme=...]` so dark/light is one attribute flip.
Icons are inline Lucide SVGs using `currentColor`, imported by name — the whole
set costs 670 kB against 81 kB for named imports.

Native OS chrome is avoided throughout (custom listbox, no `window.prompt`)
because a frameless desktop app with OS-styled widgets inside it looks broken.

## State and storage

JSON files, not a database. The web backend keeps users, connections, and keys
in its data dir (`/var/lib/virt-man`, or `~/.local/share/virt-man` for the app);
the daemon keeps a token and TLS material in `/etc/virt-man`. A personal
hypervisor manager has single-digit users and single-digit daemons — a database
would be infrastructure to back up, migrate, and corrupt for no benefit.

Nothing is cached: the SPA polls the daemon through the proxy. Live hypervisor
state is the only state, so there is no invalidation logic to get wrong.

## Bugs that shaped the design

Kept here because each one changed how the code is written, not just what it
does.

**libvirt `Connect` has no `Drop`.** Event fds leaked on every request; the
count climbed until every call failed with "Too many open files". `with_conn`
now closes on both the ok and error paths. Two `--ignored` tests guard it: one
asserts the fd count is flat across 200 connections, and a companion
*demonstrates* the leak without `close()` so the guard cannot be "fixed" by
deleting it. *Found by measuring, not reading.*

**`parse_graphics` took the first `<graphics>` element.** Adding a VNC device
alongside SPICE silently changed which server every caller got. It now picks by
kind, per caller intent.

**`attr()` matched an attribute name inside a *value*.** In `<listen
type='address' address='10.0.0.5'/>` it found the `address` in `type='address'`,
failed to parse, and fell back to loopback — so a guest listening on a real IP
was silently unreachable. Caught by a test written for the dual-graphics case.

**Live XML omits filterrefs added since boot.** Firewall reads and writes use
`VIR_DOMAIN_XML_INACTIVE`, or saving loses rules.

The pattern: every one of these was invisible in code review and obvious under
measurement. Hence the testing rules in `AGENTS.md`.

## Deliberately not built

- **Clustering / migration / HA** — one hypervisor is the use case.
- **A database** — see above.
- **VNC passwords, ZRLE/Tight** — noted ceilings, add when needed.
- **SPICE in the browser** — tried twice; virt-viewer is the answer.
- **Snapshots, backup scheduling, cloud-init** — not needed yet; the XML editor
  is the escape hatch for anything the UI does not expose.
