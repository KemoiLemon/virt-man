# Virt-Man Daemon — HTTP/UNIX control daemon

## Context

Virt-Man is a personal VM manager (Proxmox is too heavy, raw libvirt too light).
The repo has three parts: `client/` (Tauri desktop app), `web/` (Vite webapp),
`daemon/` (Rust — this plan). Both frontends will talk to the daemon; this plan
covers **only the daemon**.

The daemon must:

- Be controllable via an **HTTP API** (self-generated TLS certificate) and a
  **UNIX socket** (token-authenticated).
- Require a **pre-generated auth token** supplied by an outside panel app
  (panel is out of scope).
- Use the **libvirt** backend (`qemu:///system`) to control: domain XML,
  networking, storage pools, ISOs, VM create/delete, power on/off, and expose
  VNC/SPICE through the API.
- Run as root or a user in the `libvirt` group.

### Environment facts (verified)

- Rust 1.98.1, crate `virt-man-daemon` (edition 2024), currently `Hello, world`.
- libvirt 12.7.0, monolithic `libvirtd` running; `qemu:///system` works.
- libvirt headers + `pkg-config` present → native bindings viable.
- Current user is in the `libvirt` group.
- Existing state: pools `default`, `ISOs`; networks `internal`, `internet`;
  one running Alpine domain using SPICE graphics (127.0.0.1:5900).

## Decisions (confirmed with user)

1. **HTTP auth** — same token as the Unix socket (`Authorization: Bearer <token>`).
2. **Token delivery** — read from a file at startup (default
   `/etc/virt-man/token`, mode 0600); refuse to start if missing/unreadable.
3. **Endpoint scope** — full CRUD now (domains, networks, pools, ISOs,
   lifecycle, graphics).
4. **VNC/SPICE** — WebSocket proxy: daemon pipes WebSocket bytes ↔ graphics
   TCP socket so noVNC / spice-html5 connect through the daemon. WS auth via
   `?token=` query param (browsers can't set WS headers).

## Approach

- **Rust + axum 0.8** (tokio). One router, two listeners, both via
  **axum-server 0.8** (which supports TCP, TLS, and Unix sockets):
  - HTTP over TCP + TLS (`tls-rustls` feature), default `127.0.0.1:8443`.
  - Unix socket, default `/run/virt-man/daemon.sock` (root) or
    `$XDG_RUNTIME_DIR/virt-man/daemon.sock` (non-root), configurable.
- **TLS**: `rcgen` 0.14 generates a self-signed cert on first run, persisted
  under the cert dir, regenerated when expired. If config provides existing
  `tls.cert` / `tls.key` paths (e.g. a cert supplied by the web panel/client),
  load and serve those instead. `axum-server::tls_rustls::RustlsConfig` serves
  whichever is in use.
- **Auth**: one `tower` `from_fn` middleware layer checking the Bearer token
  against the file value (constant-time compare); the WS proxy checks `?token=`.
- **libvirt**: `virt` 0.4 crate. Open a fresh `Connect::open("qemu:///system")`
  per operation inside `tokio::task::spawn_blocking` (avoids libvirt's
  thread-affinity rules; connect is a local socket handshake).
  *ponytail: single persistent connection + actor thread if request volume ever
  makes per-call connects measurably slow.*
- **Config**: TOML (`serde` + `toml`) at `/etc/virt-man/daemon.toml` —
  http bind/port, unix socket path, token path, cert dir, optional
  `tls.cert`/`tls.key` paths. Sensible defaults if
  absent; `VIRT_MAN_CONFIG` env override.
- **Startup guard**: exit with a clear error unless `geteuid()==0` or member of
  the `libvirt` group.

## Files to modify

- `daemon/Cargo.toml` — dependencies.
- `daemon/src/main.rs` — entry: config, guards, TLS, serve both listeners.
- `daemon/src/config.rs` — config struct + TOML load/defaults + privilege guard.
- `daemon/src/tls.rs` — rcgen self-signed cert load/generate.
- `daemon/src/auth.rs` — token middleware + WS token check.
- `daemon/src/libvirt.rs` — connect wrapper + blocking-call helper + graphics lookup.
- `daemon/src/api/mod.rs` — router assembly.
- `daemon/src/api/health.rs` — `GET /health`, `GET /version`.
- `daemon/src/api/domains.rs` — domain CRUD + lifecycle.
- `daemon/src/api/networks.rs` — network CRUD + start/stop.
- `daemon/src/api/pools.rs` — storage pool CRUD + start/stop/refresh.
- `daemon/src/api/volumes.rs` — volumes/ISO list, create, delete, upload.
- `daemon/src/api/console.rs` — WebSocket VNC/SPICE proxy.

## Reuse

- `virt` crate (native libvirt bindings) instead of shelling out to `virsh`.
- axum extractors + `tower` `from_fn` middleware for auth.
- `rcgen` + `rustls` for TLS; axum-server for both transports.
- `axum::extract::ws::WebSocketUpgrade` for the console proxy (no WS dependency
  added beyond axum's).

## API surface

- `GET /health`, `GET /version`
- Domains: `GET /domains`, `GET /domains/{name}` (info+XML),
  `GET /domains/{name}/xml`, `PUT /domains/{name}/xml`, `POST /domains`,
  `DELETE /domains/{name}?remove_storage=`,
  `POST /domains/{name}/start|shutdown|reboot|destroy|reset`,
  `GET /domains/{name}/state`
- Graphics: `GET /domains/{name}/graphics` (type/host/port), and
  `WS /domains/{name}/console?token=` — pipes WebSocket to the domain's
  VNC or SPICE TCP endpoint (read from domain XML `graphics` element).
- Networks: `GET /networks`, `GET /networks/{name}`, `POST /networks`,
  `PUT /networks/{name}/xml`, `DELETE /networks/{name}`,
  `POST /networks/{name}/start|stop`
- Pools: `GET /pools`, `GET /pools/{name}`, `POST /pools`,
  `PUT /pools/{name}/xml`, `DELETE /pools/{name}`,
  `POST /pools/{name}/start|stop|refresh`
- Volumes/ISOs: `GET /pools/{pool}/volumes`, `GET /pools/{pool}/volumes/{vol}`,
  `POST /pools/{pool}/volumes`, `DELETE /pools/{pool}/volumes/{vol}`,
  `POST /pools/{pool}/volumes/{vol}/upload` (raw body streamed into libvirt
  `virStream`; ISOs live in the `ISOs` pool)

## Steps

- [x] 1. Add dependencies: `axum`, `axum-server` (`tls-rustls`), `tokio`
        (`full`), `virt`, `rcgen`, `rustls`, `serde`, `serde_json`, `toml`,
        `tracing`, `tracing-subscriber`, `subtle` (const-time compare).
- [x] 2. `config.rs`: TOML config, defaults, privilege guard.
- [x] 3. `tls.rs`: generate/persist self-signed cert.
- [x] 4. `auth.rs`: token middleware (Bearer + WS query token).
- [x] 5. `libvirt.rs`: connect + `spawn_blocking` helper + graphics lookup.
- [x] 6. `api/health.rs` + `api/mod.rs` router.
- [x] 7. `api/domains.rs` CRUD + lifecycle.
- [x] 8. `api/networks.rs`, `api/pools.rs`, `api/volumes.rs`.
- [x] 9. `api/console.rs` WebSocket↔TCP pipe proxy.
- [x] 10. `main.rs`: wire config → guards → TLS → serve TCP + Unix socket.
- [x] 11. Manual end-to-end verification.

## Verification

- `cargo build` / `cargo clippy` clean.
- Start as current user (in `libvirt` group): guards pass, cert generated.
- `curl --insecure https://127.0.0.1:8443/health` (with token) → 200.
- `curl --unix-socket $XDG_RUNTIME_DIR/virt-man/daemon.sock
  -H "Authorization: Bearer <token>" http://localhost/domains`.
- Cross-check with `virsh`: list domains, create/start/stop a test domain,
  list pools/networks, upload a small ISO to the `ISOs` pool.
- `GET /domains/alpinelinux3.23/graphics` → `spice`, `127.0.0.1:5900`.
- Open `ws://…/domains/alpinelinux3.23/console?token=` in a noVNC page.
- Negative: wrong/missing token → 401; non-libvirt-group user → startup error.
