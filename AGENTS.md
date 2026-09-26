# AGENTS.md

Working notes for coding agents in this repo. Read `DESIGN.md` for *why* the
system is shaped the way it is; this file is *how to work in it without
breaking things*.

## What this is

Three parts, one product:

| Path | Build | Role |
| --- | --- | --- |
| `daemon/` | `cargo` | Rust + axum over libvirt (`qemu:///system`). Runs on the hypervisor host. Token auth, HTTPS `[::]:8443`. |
| `web/` | `pnpm` + `cargo` | `web/src` is the SPA; `web/server` is `virt-man-web` (users, RBAC, API keys, TLS-pinned daemon proxy). HTTPS `[::]:8006`. |
| `client/` | `pnpm tauri` | Tauri v2 desktop shell. Embeds the same SPA, runs `virt-man-web` as a loopback sidecar with auth disabled. |

**One frontend build serves both browser and desktop.** There is no
desktop-only UI code path. If you add a browser-only assumption, the app
breaks, and vice versa.

## Commands

```bash
# frontend: typecheck, build, unit tests (68)
cd web && npx tsc --noEmit && npx vite build && pnpm test

# web backend (19 tests: permissions/roles/key caps + redirect URL building)
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
cd daemon && cargo clippy --all-targets && cargo test --bins   # 26 tests
cd daemon && cargo test --test fd_leak -- --ignored            # needs libvirt

# web backend
cd web/server && cargo clippy --all-targets

# install (builds as $SUDO_USER so repo artifacts stay user-owned)
sudo ./daemon/install.sh && sudo ./web/install.sh && sudo ./client/install.sh
```

Live suites need a running backend. The desktop app starts one on a random
loopback port; find it with:

```bash
ss -tlnp | grep virt-man-web | grep -v :8006
```

**Run live suites with a gap between them** (`sleep 2`). They attach real
console sessions, and back-to-back runs collide.

## Hard constraints

These are not style preferences. Violating them produces broken builds or
silently broken UI.

**No native OS chrome.** `<select>` is `display:none` behind a themed
`.select-trigger`/`.select-popover` listbox (`web/src/listbox.ts`). Never use
`window.prompt`/`confirm`/`alert` — use `askText`/`askConfirm` in `main.ts`.

**No inline event handlers.** `onclick="..."` in an HTML string is blocked by
the desktop app's CSP. Assign handlers in JS after rendering.

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
auth-disabled local app mode; TLS-pinned daemon connections; working titlebar
drag and window controls.

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
- Test VM `alpinelinux3.23` — running, 1 vCPU, 768 MB, **both** VNC (5900) and
  SPICE (5901), network `internet`.

Node rejects the web UI's self-signed cert, so point live suites at the app's
loopback sidecar (HTTP) rather than `https://127.0.0.1:8006`. Suites that must
exercise login instead need `NODE_TLS_REJECT_UNAUTHORIZED=0` plus credentials
(the loopback sidecar runs auth-disabled, so it cannot test the login path).
For throwaway state, run a second instance with its own `VIRT_MAN_WEB_DATA`
and seed it through `/auth/setup`.

**HTTP on the TLS port redirects, it does not fail.** `http://host:8006` used
to return a raw TLS alert record; the listener now peeks the first byte of each
connection (TLS always starts `0x16`) and answers plaintext with a 308 to the
`https://` URL. Only the TLS branch is wrapped, so the desktop app's
`VIRT_MAN_WEB_LOOPBACK_HTTP=1` mode still serves plain HTTP directly.

**The wildcard bind is `[::]`, not `0.0.0.0`.** `localhost` resolves to `::1`
alone on many systems (including this one — see `/etc/hosts`), so an IPv4-only
wildcard refuses `https://localhost:8006` while `127.0.0.1` works. Both
services map the `0.0.0.0` wildcard to `[::]`, which serves both families on
one socket given `net.ipv6.bindv6only=0` (the Linux default). An **explicit**
address is never rewritten, so the desktop sidecar's `127.0.0.1` bind stays
loopback-only. `curl` masks this by falling back to IPv4; a browser does not,
so test with `curl -6` or `ss -tlnp` (a dual-stack listener shows as `*:8006`).
