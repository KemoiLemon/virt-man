// A guest addressed by UUID gets exactly the permission check it gets by name.
//
// The daemon resolves a UUID to a guest; grants, ownership and suspension are
// recorded under the guest's name. If the proxy checked the path as written, a
// request by UUID would reach an ACL path carrying none of them, so a
// "NoAccess on this guest" carve-out would not hold. The proxy rewrites the
// UUID to the name first; this checks it, end to end, through the real web
// server against a stub daemon (no real host is touched).
//
//   node test/live-uuid.mjs [server-binary]   (default web/server/target/debug/virt-man-web)

import { spawn } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import http from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";

const bin = process.argv[2] ?? "server/target/debug/virt-man-web";
const UUID = "ea15249b-a713-41dc-a808-05619e42eb2a";
const OTHER = "6f4d2a3b-0c9e-4d8f-b2a1-3e4c5d6e7f80";

// Stub daemon: two guests, and a log of every path it was asked for.
const seen = [];
const stub = http.createServer((req, res) => {
  const path = req.url.split("?")[0];
  seen.push(`${req.method} ${path}`);
  const body =
    path === "/domains" ? [
      { name: "secret", uuid: UUID, title: "Secret", state: "shutoff", active: false, vcpus: 1, memory_mb: 128 },
      { name: "open", uuid: OTHER, title: "Open", state: "shutoff", active: false, vcpus: 1, memory_mb: 128 },
    ]
    : path === "/networks" || path === "/pools" ? []
    : path === "/health" || path === "/version" ? { ok: true }
    : { name: path.split("/")[2] ?? "", ok: true };
  req.resume();
  req.on("end", () => { res.setHeader("content-type", "application/json"); res.end(JSON.stringify(body)); });
});
await new Promise((r) => stub.listen(0, "127.0.0.1", r));
const stubUrl = `http://127.0.0.1:${stub.address().port}`;

const data = mkdtempSync(join(tmpdir(), "vm-uuid-"));
const port = 8700 + Math.floor(Math.random() * 90);
const base = `https://127.0.0.1:${port}`;
process.env.NODE_TLS_REJECT_UNAUTHORIZED = "0";
const server = spawn(bin, [], {
  env: { ...process.env, VIRT_MAN_WEB_BIND: "127.0.0.1", VIRT_MAN_WEB_PORT: String(port), VIRT_MAN_WEB_DATA: data },
  stdio: "ignore",
});

const results = [];
const check = (name, ok, detail = "") => results.push([!!ok, name, detail]);
try {
  for (let i = 0; i < 80; i++) {
    try { await fetch(`${base}/auth/status`); break; } catch { await new Promise((r) => setTimeout(r, 100)); }
  }
  const J = { "content-type": "application/json" };
  const login = async (username) => {
    const r = await fetch(`${base}/auth/login`, { method: "POST", headers: J, body: JSON.stringify({ username, password: "password123" }) });
    return r.headers.get("set-cookie").match(/virt-man-session=[^;]+/)[0];
  };
  await fetch(`${base}/auth/setup`, { method: "POST", headers: J, body: JSON.stringify({ username: "root", password: "password123" }) });
  const admin = await login("root");
  const as = (cookie) => (p, init = {}) => fetch(`${base}${p}`, { ...init, headers: { ...J, ...(init.headers ?? {}), Cookie: cookie } });
  const A = as(admin);
  const conn = (await (await A("/connections", { method: "POST", body: JSON.stringify({ name: "stub", url: stubUrl, token: "t" }) })).json()).id;
  const bob = (await (await A("/users", { method: "POST", body: JSON.stringify({ username: "bob", password: "password123" }) })).json()).id;
  // bob may manage every guest on the daemon, except "secret".
  await A("/acl", { method: "PUT", body: JSON.stringify({ path: `/nodes/${conn}`, ugid: `user:${bob}`, role: "VMAdmin" }) });
  await A("/acl", { method: "PUT", body: JSON.stringify({ path: `/nodes/${conn}/vms/secret`, ugid: `user:${bob}`, role: "NoAccess" }) });
  const B = as(await login("bob"));

  for (const [how, id] of [["by name", "secret"], ["by UUID", UUID], ["by UUID in capitals", UUID.toUpperCase()]]) {
    const r = await B(`/proxy/${conn}/domains/${id}/start`, { method: "POST" });
    const body = await r.json();
    check(`the carve-out holds ${how}`, r.status === 403, `${r.status} ${JSON.stringify(body)}`);
    check(`…and the refusal names the guest (${how})`, /VM 'secret'/.test(body.error ?? ""), body.error);
  }
  check("no refused request reached the daemon", !seen.some((s) => s.includes("/start")), seen.join(" | "));

  const ok = await B(`/proxy/${conn}/domains/${OTHER}/start`, { method: "POST" });
  check("the rest of the daemon is still reachable by UUID", ok.status === 200, ok.status);
  check("…and the daemon was asked by name", seen.includes("POST /domains/open/start"), seen.join(" | "));

  const unknown = await B(`/proxy/${conn}/domains/7a5e3b4c-1d0f-4e9a-83b2-4f5d6e7f8091`);
  check("an unknown UUID is passed on as written (the daemon answers)", unknown.status !== 403, unknown.status);
} finally {
  server.kill();
  stub.close();
  rmSync(data, { recursive: true, force: true });
}

for (const [ok, name, detail] of results) console.log(`${ok ? "✔" : "✖"} ${name}${ok ? "" : ` — ${detail}`}`);
const fails = results.filter(([ok]) => !ok).length;
if (fails) { console.log(`\n${fails} check(s) failed`); process.exit(1); }
console.log("\nall UUID permission checks passed");
