# Virt-Man — Client, WebUI, and daemon install script

> **Historical document.** This is the original build plan, kept for context;
> all of its steps are done. It is not maintained against the current code, and
> parts are now wrong — most notably the console, which no longer uses noVNC,
> spice-html5, or a WebSocket proxy. See `DESIGN.md` for the architecture as
> built and `AGENTS.md` for how to work in the repo.

## Context

The daemon (`daemon/`) is complete: Rust + axum, token-authenticated API over
**HTTPS** (`127.0.0.1:8443`, self-signed TLS) and a **Unix socket**. It drives
libvirt (`qemu:///system`) — domains, networks, storage pools, volumes/ISOs,
lifecycle, and a WebSocket VNC/SPICE proxy (`WS /domains/{name}/console?token=`).

`client/` (Tauri v2) and `web/` (Vite + TS) are still stock scaffolding. This
plan turns them into a real product plus an automatic daemon install script.

The **web UI is the central component**: a full-stack server that authenticates
users, stores daemon connections, and **proxies** all daemon traffic. Browsers
and the desktop app only ever talk to a web UI — never to a daemon's
self-signed TLS directly — so daemon certificates only need to be trusted on
the web UI.

## Decisions (confirmed with user)

1. **One shared UI** — the SPA lives in `web/`; the Tauri `client/` embeds it
   and runs the web UI **backend** as a local sidecar.
2. **Cert trust = pin the cert itself, not a CA** — every daemon connection
   stores the daemon's exact TLS certificate (pinned, with SHA-256 fingerprint
   confirmation). The **web UI** holds these pins and proxies daemon traffic
   for browser clients. The **desktop app is standalone with its own
   functionality**: it manages its own daemon connections and pins their
   certs **locally in its own data dir** — no system CA, no dependence on a
   remote web UI.
3. **"Connect to a webui"** — a stored remote web UI URL the app window
   navigates to (`location.href = url`).
4. **Daemon exposed remotely by default** — install script sets
   `http_addr = "0.0.0.0:8443"` (TLS is the whole point) and generates a
   self-signed cert whose SANs include the hostname + host IPs.
5. **User auth on the web UI** — username + password login (no email yet),
   add/remove users, roles + scope-based permissions (Read/Write per scope),
   per-user profile (nickname + cropped/zoomed avatar) pinned to the bottom of
   the sidebar, and **API keys** for programmatic access.
6. **Build system** — Vite + TypeScript frontend; Rust + axum backend
   (single binary the Tauri app can sidecar).
7. **Console** — VNC (noVNC) and SPICE (spice-html5) through the backend's WS
   proxy, selected by the daemon's graphics `kind`.
8. **Window chrome** — frameless custom titlebar (drag + min/max/close);
   browser build omits it. GPU workaround runs silently (no topbar badge).

## Approach

### Daemon (no code changes)

Already serves an externally-provided cert via `tls.cert`/`tls.key`; the
install script supplies one with proper SANs. rcgen self-signed remains only
as the non-root/dev fallback.

### Install script (`daemon/install.sh`, new)

Idempotent, run as root:
1. Check Rust + libvirt headers; `cargo build --release`.
2. Install binary to `/usr/local/bin/virt-man-daemon`.
3. Create `/etc/virt-man/`; generate token if missing
   (`umask 077; head -c 32 /dev/urandom | base64`), mode 0600.
4. Generate a self-signed **server** cert with `openssl req -x509` into
   `/etc/virt-man/tls/` — SANs: `DNS:localhost, IP:127.0.0.1,
   DNS:$(hostname), IP:<each host IP>` (hostname/IP overridable via flags).
5. Write `/etc/virt-man/daemon.toml`: `http_addr = "0.0.0.0:8443"`,
   `token_path`, `tls.cert`/`tls.key`.
6. Install + enable `virt-man-daemon.service` (`After=libvirtd.service`,
   `Restart=on-failure`).
7. Print token path + how to connect (and that the cert gets pinned on the web
   UI when adding the daemon). No CA, no system trust-store edits.

### Web UI backend (`web/server/`, new Rust crate `virt-man-web`)

Axum server (mirrors daemon patterns). Serves the built SPA and the JSON API.

- **Storage**: JSON file in a data dir (atomic temp+rename, like vm-man's
  store) — users, connections, API keys, settings. Passwords hashed with
  `bcrypt`; API keys stored SHA-256-hashed. *ponytail: swap for SQLite if
  multi-instance concurrency ever matters.*
- **Auth**: `POST /auth/setup` (first-run admin), `POST /auth/login`
  (username+password → HMAC-signed session cookie), `POST /auth/logout`,
  `GET /auth/me`. No email, no password reset yet.
- **Users + RBAC** (admin): `GET/POST /users`, `PUT/DELETE /users/{id}`.
  Roles `admin` / `operator` / `viewer` set defaults; each user also has
  per-scope overrides `read`/`write`/`none` over scopes: `connections`, `vms`,
  `networks`, `storage`, `users`, `settings`. Middleware enforces the required
  level per route.
- **Profile**: `GET/PUT /me` (nickname), `POST /me/avatar` (multipart),
  `GET /me/avatar` (static). Avatar crop/zoom is done client-side (canvas)
  then uploaded; stored as a file, served back.
- **API keys**: `GET/POST /me/keys`, `DELETE /me/keys/{id}`; admin can manage
  all users' keys. Programmatic access via `Authorization: Bearer <key>` or
  `X-API-Key`, carrying the same permissions as the owning user.
- **Daemon connections**: `GET/POST /connections`, `PUT/DELETE /connections/{id}`,
  `POST /connections/{id}/test`. Each stores `{name, url, token, pinned cert}`.
  On add, the backend fetches the daemon's TLS cert, shows its SHA-256
  fingerprint for confirmation, then pins the PEM (manual paste as fallback) —
  "trust the certificate itself", no CA.
- **Proxy**: `ALL /api/{conn_id}/...` forwards to the daemon with
  `Authorization: Bearer <token>`, TLS-pinned to that connection's cert
  (reqwest + rustls `RootCertStore` containing only the pinned cert).
  `WS /api/{conn_id}/console/domains/{name}` upgrades and pipes bytes between
  the browser WS (session-authenticated) and the daemon's
  `wss://…/console?token=` (TLS-pinned). The daemon token never reaches the
  browser.
- **Static**: serves `web/dist` (SPA fallback to `index.html`).
- **Config/TLS**: env vars (`VIRT_MAN_WEB_BIND` default `0.0.0.0`,
  `VIRT_MAN_WEB_PORT` default `8006` — Proxmox-style port, `VIRT_MAN_WEB_DATA`).
  Autogenerates a self-signed TLS cert on first run (rcgen, persisted like the
  daemon) and serves **HTTPS by default**. Loopback-HTTP mode is used only when
  spawned as the desktop app's local sidecar.

### Web UI frontend (`web/`) — the shared SPA

Replace the counter scaffold. Modules (Vite + TS):

- `src/api.ts` — backend client: session fetch (`credentials: same-origin`),
  error toast, `wsUrl()` for the console proxy.
- `src/xml.ts` — libvirt XML builders for the creation forms
  (`domainXml`, `networkXml`, `poolXml`, `volumeXml`).
- `src/task.ts` — async task queue (running/done/failed + messages), a
  Proxmox-style Task Log for long flows (create VM, ISO upload).
- `src/app.ts` / `src/ui.ts` — render + events.
- `src/console.ts` — noVNC (`@novnc/novnc`) for VNC, spice-html5 (vendored
  `@spice-project/spice-html5` dist into `public/spice/`) for SPICE; both point
  at `ws(s)://<webui>/api/<conn>/console/domains/<name>`.
- `src/styles.css` — port of vm-man theming (CSS variables, dark/light, square
  UI, JetBrains Mono, custom selects/checkboxes, modals, toasts).
- `index.html` — app shell.

Features:
- **Login screen**; first run shows the **setup wizard** (first admin user).
- **Sidebar**: resource tree (Datacenter → servers → VMs/Networks/Storage) and
  the **user profile pinned to the absolute bottom** (avatar + nickname →
  profile dialog with nickname + avatar upload + crop/zoom).
- **Add server dialog**: name, URL, and a **token mode toggle — "paste
  existing key" vs "generate new"** — so an already-running daemon is adopted
  without regenerating its token; cert fingerprint confirmation on add.
- **VM view**: list with state dot, start/shutdown/reboot/destroy/reset/delete
  (delete with `remove_storage`), Summary + XML tabs, Console tab.
- **Network view**: list, create-from-form (name, forward nat/bridge/none,
  bridge, IP/netmask, DHCP range), start/stop/delete.
- **Storage view**: pools (create-from-form: name, type dir/logical/fs, target,
  source), start/stop/refresh/delete; volumes list, create-from-form
  (name, size GiB, format raw/qcow2), ISO upload (file input → streaming
  upload through the proxy).
- **Create VM form** → domain XML (name, vCPUs, RAM, disk size → volume in
  selected pool, existing volume/ISO attach, network, graphics VNC/SPICE,
  firmware), run as a multi-step task.
- **Users admin view** (admin): add/remove users, set role + per-scope
  permissions. **API keys** managed from the profile dialog.
- **Polling**: refresh active resource state every 3s.

### Desktop client (`client/`)

- The app is **standalone with its own functionality**: it embeds the SPA and
  the `virt-man-web` backend. On launch it spawns that backend as its own
  sidecar process bound to **loopback HTTP** (`127.0.0.1:<free port>`) — no TLS
  needed for the local webview — with its **own data dir**
  (`~/.local/share/virt-man/`), where daemon connections and their **pinned
  certificates** live locally. All daemon traffic is app → local backend →
  daemon (TLS-pinned), so the app manages daemons itself, independent of any
  remote web UI.
- "Connect to a webui" is an additional option: a stored remote web UI URL
  (default `https://host:8006`) the window navigates to.
- `src-tauri/src/main.rs`: `apply_gpu_workarounds()` before WebKitGTK init
  (same three env vars as vm-man: `WEBKIT_DISABLE_DMABUF_RENDERER`,
  `WEBKIT_DISABLE_COMPOSITING_MODE`, `__NV_DISABLE_EXPLICIT_SYNC`, NVIDIA
  detection, `VIRT_MAN_GPU_WORKAROUND` override). No user-visible badge.
- Frameless window (`decorations: false`) + custom titlebar
  (`data-tauri-drag-region`, min/max/close via `getCurrentWindow()`, guarded by
  `window.__TAURI__`).
- `src-tauri/capabilities/default.json`: window permissions for the titlebar.
- Dev: Vite dev server (5173) with `/api` + WS proxy to a loopback-HTTP
  backend (8787); prod/deployed: backend serves `web/dist` over HTTPS on
  `0.0.0.0:8006`; app sidecar: loopback HTTP on a free port. Delete the
  client's stock Vite scaffold
  (`client/src/`, `index.html`, `vite.config.ts`, `tsconfig.json`, assets) and
  trim `client/package.json` to `@tauri-apps/cli` + `tauri` script.

## Files to modify

- `daemon/install.sh` *(new)* — automatic install script.
- `daemon/virt-man-daemon.service` *(new)* — systemd unit.
- `web/server/Cargo.toml`, `web/server/src/*` *(new)* — `virt-man-web` backend
  (auth, users/RBAC, API keys, profile, connections, proxy, static).
- `web/package.json` — add `@tauri-apps/api`, `@novnc/novnc`,
  `@spice-project/spice-html5`.
- `web/index.html`, `web/src/main.ts`, `web/src/styles.css` — replace scaffold.
- `web/src/api.ts`, `xml.ts`, `task.ts`, `app.ts`, `ui.ts`, `console.ts` *(new)*.
- `web/public/spice/` *(new)* — vendored spice-html5 dist.
- `client/src-tauri/tauri.conf.json`, `src-tauri/src/main.rs`,
  `src-tauri/src/lib.rs`, `src-tauri/capabilities/default.json`,
  `src-tauri/Cargo.toml` — sidecar spawn + GPU workaround + window chrome.
- `client/package.json` — trim; delete `client/src/`, `client/index.html`,
  `client/vite.config.ts`, `client/tsconfig.json`, assets.
- `.gitignore` — ignore `daemon/target/`, `web/server/target/`,
  `web/node_modules/`, `web/dist/`, `client/node_modules/`.
- `README.md` — install + usage.

## Reuse

- **Daemon API unchanged** — the backend proxies it; no daemon code edits.
- vm-man `ui/base.css` theme variables + `apply_gpu_workarounds()` — copy and
  adapt (explicitly requested).
- axum + tokio patterns already proven in `daemon/` (config, middleware,
  WS pipe) for the backend.
- `@novnc/novnc` + `@spice-project/spice-html5` (no custom VNC protocol code).
- Existing Vite + TS scaffold in `web/`.

## Steps

- [x] 1. `daemon/install.sh` + systemd unit + `.gitignore` + README.
- [x] 2. Backend skeleton: config, data store, static serving, `/health`.
- [x] 3. Backend auth: setup, login/logout, session cookie, bcrypt.
- [x] 4. Backend users/RBAC middleware + profile (nickname + avatar).
- [x] 5. Backend API keys + connections (pin cert, fingerprint, test).
- [x] 6. Backend daemon proxy (HTTP + WS console, TLS-pinned).
- [x] 7. Frontend shell: login/setup, sidebar, profile at bottom, theming.
- [x] 8. Frontend: add-server dialog (paste/generate token), resource tree.
- [x] 9. Frontend: VM list/detail/lifecycle + create-VM form (`xml.ts`).
- [x] 10. Frontend: networks + pools + volumes forms + ISO upload.
- [x] 11. Frontend: console tab (noVNC + spice-html5) + task log + polling.
- [x] 12. Client: sidecar spawn, GPU workaround, frameless titlebar, webui-connect.
- [x] 13. End-to-end verification.

## Verification

- `sudo daemon/install.sh` → `systemctl status virt-man-daemon` active;
  `curl -k https://<host>:8443/health -H "Authorization: Bearer $(cat
  /etc/virt-man/token)"` → 200; bound on `0.0.0.0`.
- Backend (`cargo run -p virt-man-web`): serves HTTPS on `0.0.0.0:8006` with
  an autogenerated cert → setup admin → login → add a daemon (paste existing
  token, confirm cert fingerprint) → VMs listed through the proxy.
- `pnpm --dir web dev`: same flow in browser; create VM via form (disk in a
  pool) as a task; create network + pool via forms; upload a small ISO; open
  SPICE console (existing Alpine VM) and a VNC console; task log runs→done.
- RBAC: create a `viewer` user — read-only everywhere, write actions rejected;
  scope override grants storage write only; revoke works.
- API key: `curl -k -H "Authorization: Bearer <key>" https://127.0.0.1:8006/api/
  <conn>/domains` returns data; revoked key → 401.
- Profile: set nickname, upload + crop/zoom an avatar; it renders at the
  sidebar bottom and follows the theme.
- `pnpm --dir client tauri dev`: frameless window drags/minimises/maximises/
  closes; same flows through the local sidecar; GPU env vars set under NVIDIA
  (no badge); "connect to a webui" navigates the window to a remote web UI.
- Negative: wrong daemon token → 401 toast; wrong login → rejected; form
  validation blocks empty names / bad sizes before any request.
