# DESIGN.md

Why virt-man is shaped the way it is. `AGENTS.md` covers how to work in the
repo; `README.md` covers installation. This file is the reasoning, including
the decisions that were reversed and why.

## The problem

Proxmox is too heavy for a personal hypervisor; raw `virsh` is too light. This
is the middle: a real UI over libvirt, usable from a browser on the LAN,
without running a distro-sized management stack.

## Shape

```text
browser ──HTTPS(8006)── virt-man-web ──HTTPS(8443, pinned cert)── virt-man-daemon ── libvirt
            (auth, RBAC,             (on the hypervisor)
             daemon proxy)
```

Two processes, two deliberate choices:

**The daemon owns libvirt and nothing else.** No users, no sessions, no
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
/nodes/<conn>/{vms,containers,networks,storage,bridges}[/<name>]
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

**What a non-admin may ask libvirt to define is an allowlist**
(`xmlcheck.rs`). libvirt defines as root, so guest XML is a request to the
host: a `<hostdev>` hands over a device, a `<disk>` can name `/etc/shadow`, a
`<qemu:commandline>` runs anything. So non-admin XML may contain only what the
app's own forms write, and everything it names must be the user's to use
(`guard.rs`): networks need `Network.Use`; a writable disk must be a volume
they own, a read-only one any volume in a pool they can read (ISOs); and a disk
may be qcow2 only if it was created empty here — a qcow2 header can name any
host file as its backing store, and qemu opens it. A bridge-mode network needs
`Bridge.Use` on that bridge. Pools are defined by administrators; users
allocate space in them. Administrators skip these checks: they have all of
libvirt, as before.

**Containers have their own privileges** (`CT.Audit`, `CT.Allocate`,`CT.Config`, `CT.PowerMgmt`, `CT.Console`) and roles (`CTUser`, `CTAdmin`),
beside the VM set rather than folded into it: a VM grant reaches no container
and the reverse, so an admin can hand someone guests of one kind only. The
daemon enforces the split too: `/domains` never sees a container.

**`CT.Privileged` is granted, never owned.** A privileged container has no
user namespace, so its root is the host's root. Ownership gives a container's
owner every `CT.*` privilege *except* this one, or anyone who can create a
container could make it privileged by editing it. It is checked on create
(`privileged: true`) and on any XML write that drops or changes the
`<idmap>`, and an owner cannot share a privileged container (its console is a
root shell on the host). The only record of "privileged" is the absence of
`<idmap>` in the definition: the web server reads it back from the daemon
rather than keeping a flag that could drift from what runs.

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
It is deliberately a different thing from a guest console: the browser sends a
host username and password, the daemon verifies them through the host's own
PAM, and only then starts a login shell in a pty **as that exact user** — uid,
gid, supplementary groups, home and login profile included. Authenticating as
root gives a root shell; authenticating as an ordinary host user gives that
user. `daemon/install.sh` installs the `/etc/pam.d/virt-man-daemon` service
(the ordinary `pam_unix` password + account check), because "the shell runs as
the account it authenticated as" is the whole security property.

Three decisions in it are load-bearing:

- **PAM, not the web app's own identity.** The web server has users and hashed
  passwords of its own (`store.json`), but those authorise *libvirt actions*
  and are unrelated to host accounts. A host shell has to be a host login, and
  only the host's PAM knows whether an account may log in. So a session is a
  fresh login every time; nothing is cached, no ticket is minted, and the
  password is sent once and never stored.
- **The daemon is the only part that sees a host password.** It is the
  privileged process; forwarding credentials through the proxy would spread
  the secret. The browser talks to `/proxy/<conn>/shell/…`, the proxy attaches
  the daemon token and nothing else, and the daemon runs PAM.
- **The shell belongs to the page.** A random id names the session, its pty is
  private, and it dies when the browser leaves: tab switching, Back, or a
  closed tab (`pagehide` beacons a disconnect) all tear down the process
  group, and a session nobody reads closes after 30 s. An abandoned login
  shell must not outlive the page that asked for it.

It is **administrator work**, gated in `routes.rs` as `Special::AdminOnly`:
`Sys.Modify` on the node plus the admin flag. The ACL exists to delegate
libvirt resources, and deliberately has no privilege for "a shell on the host"
— nothing narrower than administrator would be honest about what it grants.

Two smaller ones, each because the obvious alternative is wrong:

- **The pty pump runs on `std::thread`, and the fork is a `forkpty`+`execve`
  with an async-signal-safe child.** The rest of the daemon is async, but the
  shell is a long-lived syscall loop per session, and a sync `std` thread is
  what the container console already uses for exactly that.
- **PAM is bound by hand**, not through a crate: four calls (`pam_start`,
  `pam_authenticate`, `pam_acct_mgmt`, `pam_end`) plus one conversation
  function, in `daemon/src/pam.rs`. The daemon already owns other small C
  bindings (`libc`), so a dependency for this would be more surface than the
  thing it saves.

A source checkout that has not run the installer falls back to the host's
`login` PAM stack, so a hand-built daemon still works; a packaged install
always has the dedicated file. The pty is a real controlling terminal, so
interactive programs and job control behave.

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
Icons are inline Phosphor *fill* SVGs using `currentColor`, imported one file
per glyph so only the icons in use reach the bundle.

Native OS chrome is avoided throughout (custom listbox, no `window.prompt`)
so the controls match the square, monospace theme instead of the host OS.

It is one SPA for desktop and phone, not two products: below 820 px the same
DOM gets a single-column shell, an off-canvas sidebar, and touch gestures that
map onto the existing mouse and keyboard paths rather than replacing them. See
`MOBILE.md` for that viewport's decisions and the iOS behaviours behind them.

## State and storage

JSON files, not a database. The web backend keeps users, connections, and keys
in its data dir (`/var/lib/virt-man` as root, `~/.local/share/virt-man` as a
regular user); the daemon keeps a token and TLS material in `/etc/virt-man`.
A personal hypervisor manager has single-digit users and single-digit daemons
— a database would be infrastructure to back up, migrate, and corrupt for no
benefit.

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

## Containers

Containers are **libvirt LXC** guests (`lxc:///system`), chosen over Incus or
LXD because libvirt's network, storage and nwfilter drivers are shared by the
whole host: a container's `<interface type='network'>` joins the same bridges
as the VMs, so the daemon's DHCP/DNS, the firewall, port forwards, rate limits
and access logs apply to it unchanged. The one rule that makes that true is
that **nothing lists guests from one driver**: `libvirt::all_domains` walks
both, and every caller that treats guests as a set (IPAM, firewall, metrics,
tap reattachment, the in-use check) uses it. The `/containers` routes are the
`/domains` handlers run with the LXC driver selected (`DRIVER` task-local), so
the two cannot drift apart.

**Templates are pool volumes**, Proxmox-style: a rootfs tarball stored like an
ISO and recognised by its extension, so it can arrive by download, upload, or a
file copied into the pool. The catalog merges linuxcontainers.org and
TurnKey's index, both over HTTPS, every download checked against its published
digest. Proxmox's own index is left out: its HTTPS certificate does not match
its host, so it is reachable only over plain HTTP, where the index and the
digests in it can be swapped together.

**The rootfs is a raw ext4 image in a pool** (quotas, deletion logging and the
in-use check treat it as a disk), mounted by the daemon, not by libvirt.
Measured on libvirt 12.7: libvirt's own loop-mounted `<filesystem
type='file'>` cannot be combined with an `<idmap>` — libvirt_lxc creates its
mount point as the *mapped* root, which is denied. So the daemon loop-mounts
each image under `/run/virt-man/ct/<uuid>` (every directory above it must be
traversable by the mapped uid, hence `/run`, not the 0700 data dir) and remounts
them at start, before starting autostart containers libvirt could not.

**A template is unpacked inside the container's own uid mapping**
(`unshare --map-users`), so the kernel shifts ownership and nothing runs as
host root against a tree the template controls: a planted `evil -> /etc`
symlink resolves inside the namespace, where `/etc` is not writable
(measured). Switching a container between privileged and unprivileged changes
the mapping but not the files, so the daemon re-owns the rootfs by the
difference before the next start — with `lchown`, never following a link.

**Things the LXC driver cannot do**, measured, and how each surfaces: it
refuses to change a running container's NIC, so a rate limit is saved and
reported `pending` until the next start rather than claimed; it refuses
`VIR_DOMAIN_CONSOLE_FORCE`, so a console session is shared by every viewer of
one container instead of stolen; and it has no bulk stats call, which the
metrics sampler never used.

## Deliberately not built

- **Clustering / migration / HA** — one hypervisor is the use case.
- **A database** — see above.
- **VNC passwords, ZRLE/Tight** — noted ceilings, add when needed.
- **SPICE in the browser** — tried twice; virt-viewer is the answer.
- **Snapshots, backup scheduling, cloud-init** — not needed yet; the XML editor
  is the escape hatch for anything the UI does not expose.
- **Container backups, snapshots, mount points beyond the rootfs, and turning
  a container into a template** — add when needed. Separate container quotas
  too: a container counts against the VM, vCPU, memory and storage limits.
