# DESIGN.md

Why virt-man is shaped the way it is. `AGENTS.md` covers how to work in the
repo; `README.md` covers installation. This file is the reasoning, including
the decisions that were reversed and why.

## The problem

Proxmox is too heavy for a personal hypervisor; raw QEMU is too light. This
is the middle: a real UI over direct QEMU, usable from a browser on the LAN,
without running a distro-sized management stack.

## Shape

```text
browser ──HTTPS(8006)── virt-man-web ──HTTPS(8443, pinned cert)── virt-man-daemon ── QEMU
            (auth, RBAC,             (on the hypervisor)
             daemon proxy)
```

Two processes, two deliberate choices:

**The daemon owns QEMU, the host's networks and its storage pools, and nothing else.** No users, no sessions, no
policy — one token, one hypervisor. That keeps the privileged component small
enough to reason about. It listens on `[::]:8443` because remote management
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

## Backends

**A VM is a direct QEMU process.** The daemon launches QEMU with an explicit
argument vector and drives it over QMP. There is no libvirt daemon or library in
the path. The SPA still writes libvirt-shaped domain XML, and that XML is the
guest's durable definition: it is parsed into the argv, hotplug calls and device
list, not handed to another manager. A guest is keyed by its manager UUID, its
runtime lives under `<data dir>/qemu/<uuid>` (QMP and guest-agent sockets, UEFI
variable store, swtpm state), and its swtpm is started and stopped with it.
NICs and disks hot-add and hot-remove over QMP; a removal is reported `pending`
until QEMU confirms it.

**Networks and storage are the daemon's own.** `<data dir>/networks.json` holds
mode, subnet, range, DNS and reservations; a managed network owns a bridge
named from its UUID and one dnsmasq. NAT, isolation and port forwarding are
rendered into the daemon's IPv4 nftables table, and a direct-QEMU NIC reaches
the network through a daemon-created tap, so DHCP/DNS, per-VM firewall rules and
`tc` rate limits apply to it. IPv6 guest networking is out of scope. Pools are
directories plus `<data dir>/pools.json`; a volume is a file, a read is a
directory scan, and there is no storage driver cache to invalidate.

**Metrics and capture come from the guests themselves.** Host figures are read
from `/proc`; per-guest counters come from QEMU and the guest agent, with the
definition supplying every guest's NIC and disk list so a failed read never
looks like a removed device. Packet capture is an `AF_PACKET` socket on the
guest's tap, polled only while someone watches, and nothing is stored.

## Certificate pinning, not a CA

Every daemon connection stores that daemon's exact certificate, confirmed by
SHA-256 fingerprint when it is added.

A CA would mean provisioning trust before a daemon is useful. Pinning means an
already-running daemon can be adopted by pasting its token and confirming a
fingerprint — no PKI, no system trust store, and no "just accept the warning"
step that trains the user to ignore TLS errors. Only the web backend holds
pins, so browsers never see a self-signed certificate at all.

## One port, both protocols

The web UI listens once, on 8006, and decides per connection.

Plaintext HTTP to a TLS port gets a raw TLS alert record back — bytes that mean
"decode_error" to a TLS client and nothing at all to a browser, which is what
made `http://host:8006` look broken rather than merely wrong. The fix is a
one-byte peek: every TLS connection opens with `0x16` (handshake), and no HTTP
method does, so anything else is plaintext and earns a 308 to the `https://`
URL. `peek()` leaves the bytes in the socket, so a real TLS handshake reaches
rustls untouched.

A second listener on port 80 was the alternative. It was rejected because it
needs another privileged bind, another firewall hole, and it still would not
help the person who typed the port number — which is the actual mistake being
made. 308 rather than 302 so a redirected POST keeps its method and body.

## Auth and permissions

Proxmox-style (`web/server/src/perm.rs`). A **privilege** is one action
(`VM.PowerMgmt`, `Network.Use`, `Datastore.AllocateSpace`, `Bridge.Use`, ...);
a **role** is a set of them; an **ACL entry** grants one role to one user or
group at one path. Paths form a tree, and everything a daemon has lives under
its node, so one grant on `/nodes/<conn>` covers a whole server:

```
/                                       everything
/nodes/<conn>                           one daemon
/nodes/<conn>/{vms,networks,storage,bridges}[/<name>]
```

The **deepest** path carrying an entry for the user decides, and at one level
the user's own entries beat their groups'. So "admin at `/`, NoAccess on this
guest" is expressible, as is carving one user out of a group grant. Entries
propagate down unless told not to. Anything not granted is denied.

**Ownership** sits on top of the ACL. Whoever creates a VM, network or volume
owns it (`Store::resources`) and holds every privilege on it. That is what lets
the default `User` role grant *create* (`VM.Allocate`) without *read*
(`VM.Audit`): a user sees exactly what they made, and sharing is an ACL entry
on the resource's path. Anything a daemon has with no record — guests from
before ownership, or made outside the app — belongs to the first administrator.
A **suspended** resource leaves everyone but administrators read-only,
including its owner.

"Administrator" is **derived from `Permissions.Modify` at `/`**, not from a
role name. Managing accounts, groups, roles and grants is deliberately that one
privilege: anyone who can edit accounts can reset an admin's password, so a
narrower-looking "User.Modify" would be a lie. Every change that could remove
the last administrator (a grant, a group membership, a role edit) is simulated
and refused if none survives. Built-in roles are fixed and re-seeded on load;
custom access goes in a custom role.

**The proxy classifies every daemon route** (`routes.rs`) to one privilege at
one path, and **refuses a route it does not know** — the reverse of the old
path-prefix mapping, where a new daemon endpoint was reachable by default.
Lists (guests, networks, pools, bridges) are filtered per user, including
answers served from the read cache.

**What a non-admin may ask the daemon to define is an allowlist**
(`xmlcheck.rs`). The daemon launches QEMU as root, so guest XML is a request to
the host: a `<hostdev>` hands over a device, a `<disk>` can name `/etc/shadow`, a
`<qemu:commandline>` runs anything. So non-admin XML may contain only what the
app's own forms write, and everything it names must be the user's to use
(`guard.rs`): networks need `Network.Use`; a writable disk must be a volume
they own, a read-only one any volume in a pool they can read (ISOs); and a disk
may be qcow2 only if it was created empty here — a qcow2 header can name any
host file as its backing store, and qemu opens it. A bridge-mode network needs
`Bridge.Use` on that bridge. Pools are defined by administrators; users
allocate space in them. Administrators skip these checks: they hold every privilege.

**Limits** (`perm::Quota`): VMs, vCPUs, memory, storage and networks, applied
per daemon or pooled across all of them, with per-daemon overrides. Usage is
read live from the daemons at check time, so a guest resized outside the app is
counted as it is.

An API key carries an optional privilege list, intersected with its owner's:
a key can narrow its owner but never exceed them. Keys may be created unnamed;
the server generates a readable `adjective-noun-hex4` label.

A store from before the ACL migrates once (`Store::migrate`): each user keeps
their old access at `/`, as their built-in role or a `legacy-<name>` role.

The local loopback mode (`VIRT_MAN_WEB_LOOPBACK_HTTP=1`) disables auth
exactly because a single-user loopback instance gains nothing from a login
screen; it is what the live test suites run against, and doubles as a dev
mode. The remote HTTPS service always has auth on.

## The daemon shell

A shell on the hypervisor host, offered as a **Shell tab on a server page**.
The PTY runs the host's own **`login(1)`**, so the terminal *is* the login UI:
the browser shows a plain terminal, `login` prints `login:` and `Password:`,
reads them with echo off, authenticates through the host's PAM stack and execs
the account's login shell. The daemon holds no credentials, runs no PAM
conversation of its own and decides nothing about who may log in — the host's
account policy does. There is no credentials form in the app.

Three decisions in it are load-bearing:

- **`login(1)`, not a PAM conversation of our own.** `login` is the host's own
  interactive front end to PAM: it already knows the account policy, the
  echo-off prompt, the retries and the messages. Reimplementing that in the
  daemon would mean owning a password prompt and its edge cases to no benefit,
  and it would have to run as root anyway for `setuid`/`setgroups` to the
  target account. Because `login` is setuid-root on the distributions that ship
  it that way, the shell also works when the daemon itself is not root.
- **The daemon is the only part that can offer it, and it runs the process as
  root.** The daemon is the privileged process; the web backend never sees a
  host password on this path. The `virt-man-daemon.service` unit already runs
  as root, which is what lets `login` do its job.
- **The shell belongs to the page.** A random id names the session, its pty is
  private, and it dies when the browser leaves: tab switching, Back, or a
  closed tab (`pagehide` beacons a disconnect) all tear down the process
  group, and a session nobody reads closes after 30 s. An abandoned login
  shell must not outlive the page that asked for it.

It is **administrator work**, gated in `routes.rs` as `Special::AdminOnly`:
`Sys.Modify` on the node plus the admin flag. The ACL exists to delegate
VM and network resources, and deliberately has no privilege for "a shell on the host"
— nothing narrower than administrator would be honest about what it grants.

Two smaller ones, each because the obvious alternative is wrong:

- **The pty pump runs on `std::thread`, and the fork is a `forkpty`+`execve`
  with an async-signal-safe child.** The rest of the daemon is async, but the
  shell is a long-lived syscall loop per session, and a sync `std` thread is
  what the console pump already uses for exactly that.
- **The child parks its error pipe at fd 3 before `close_range(4, …)`.** The
  pipe must outlive the fd sweep or a failed `execve` would look like a clean
  one; fd 0/1/2 are the pty `forkpty` already pointed at the child. Everything
  else is CLOEXEC.

The pty is a real controlling terminal, so interactive programs, job control
and terminal resizing all behave.

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
usable client. A guest's definition carries one display adapter, so a SPICE
guest gets a **virt-viewer `.vv` file** instead of a console that silently does
nothing, and a VNC guest gets the built-in viewer. virt-viewer handles SPICE
better anyway — audio, clipboard, USB.

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
Icons are inline Phosphor *fill* SVGs using `currentColor`, imported one file
per glyph so only the icons in use reach the bundle.

Native OS chrome is avoided throughout (custom listbox, no `window.prompt`)
so the controls match the square, monospace theme instead of the host OS.

It is one SPA for desktop and phone, not two products: below 820 px the same
DOM gets a single-column shell, an off-canvas sidebar, and touch gestures that
map onto the existing mouse and keyboard paths rather than replacing them. See
`MOBILE.md` for that viewport's decisions and the iOS behaviours behind them.

## State and storage

JSON files, not a general database. The web backend keeps users, connections,
and keys in `store.json` in its data dir (`/var/lib/virt-man` as root,
`~/.local/share/virt-man` as a regular user); time-series history and access
logs are SQLite (`history.db`, `access.db`) because they are not hand-edited
records. The daemon keeps configuration, token and TLS material in
`/etc/virt-man-daemon`, and durable VM definitions, pools and networks under
`/var/lib/virt-man-daemon`. A personal hypervisor manager has single-digit users
and single-digit daemons — a general database would be infrastructure to back
up, migrate, and corrupt for no benefit.

Live hypervisor state is the only truth: the SPA polls through the proxy, and
the proxy's short-lived read cache is a paint aid, never a source of truth, so
there is no invalidation logic to get wrong.

**Storage pools are the daemon's own directories.** A pool is a host directory
plus metadata — `title`, `path`, `autostart`, `active` — in
`<daemon data dir>/pools.json`, keyed by the UUID the UI addresses it by. A read
is a directory scan, so it is always current and there is no cache to
invalidate. Roots are arbitrary absolute directories an administrator chooses,
canonicalised, kept apart from each other and from a deny list of system and
daemon paths, and revalidated on every use (a root that has become a symlink is
refused, not followed). Volumes are ordinary files: raw images are sparse,
qcow2 images are created by `qemu-img` with mode 0600, and uploads and fetches
place a dot-prefixed temp in the pool root and rename it into place so a failed
transfer cannot damage an existing volume. Pools are not imported: this host is
greenfield and `pools.json` is the only source of truth.

## Bugs that shaped the design

Kept here because each one changed how the code is written, not just what it
does.

**`parse_graphics` took the first `<graphics>` element.** Adding a VNC device
alongside SPICE silently changed which server every caller got. It now picks by
kind, per caller intent.

**`attr()` matched an attribute name inside a *value*.** In `<listen
type='address' address='10.0.0.5'/>` it found the `address` in `type='address'`,
failed to parse, and fell back to loopback — so a guest listening on a real IP
was silently unreachable. Caught by a test written for the dual-graphics case.

The pattern: every one of these was invisible in code review and obvious under
measurement. Hence the testing rules in `AGENTS.md`.

## Deliberately not built

- **Clustering / migration / HA** — one hypervisor is the use case.
- **Containers** — removed; direct QEMU is VM-only and there is no replacement
  runtime.
- **A database** — see above.
- **VNC passwords, ZRLE/Tight** — noted ceilings, add when needed.
- **SPICE in the browser** — tried twice; virt-viewer is the answer.
- **Snapshots, backup scheduling, cloud-init** — not needed yet; the XML editor
  is the escape hatch for anything the UI does not expose.
