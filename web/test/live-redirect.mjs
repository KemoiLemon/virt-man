// HTTP -> HTTPS redirect on the single TLS port.
//
// Plain HTTP to the TLS port used to return a raw TLS alert record
// (15 03 03 ... = decode_error), which browsers show as a protocol error and
// curl shows as nothing at all. The server now sniffs the first byte and
// answers plaintext with a 308.
//
// This is transport-level, so it uses raw sockets rather than jsdom:
//   node test/live-redirect.mjs [httpsBaseUrl]

import assert from "node:assert/strict";
import net from "node:net";
import tls from "node:tls";

const base = new URL(process.argv[2] ?? "https://127.0.0.1:8006");
const host = base.hostname;
const port = Number(base.port || 443);

const results = [];
const check = (name, fn) => {
  try {
    fn();
    results.push(["pass", name]);
  } catch (e) {
    results.push(["FAIL", `${name} — ${e.message}`]);
  }
};

/// Send raw bytes, collect the reply until the peer closes.
function rawRequest(payload) {
  return new Promise((resolve, reject) => {
    const sock = net.connect(port, host);
    const chunks = [];
    sock.setTimeout(5000, () => {
      sock.destroy();
      reject(new Error("timed out"));
    });
    sock.on("connect", () => sock.write(payload));
    sock.on("data", (c) => chunks.push(c));
    sock.on("end", () => resolve(Buffer.concat(chunks)));
    sock.on("close", () => resolve(Buffer.concat(chunks)));
    sock.on("error", reject);
  });
}

const get = (path, hostHeader = `${host}:${port}`) =>
  rawRequest(`GET ${path} HTTP/1.1\r\nHost: ${hostHeader}\r\nConnection: close\r\n\r\n`);

// ---- plaintext gets a redirect, not a TLS alert

const plain = await get("/");
const text = plain.toString("latin1");

check("plaintext HTTP on the TLS port is not answered with a TLS alert", () => {
  // 0x15 = TLS alert record. That was the old broken behaviour.
  assert.notEqual(plain[0], 0x15, "first byte is not a TLS alert record");
  assert.ok(text.startsWith("HTTP/1.1"), `a real HTTP response (got ${JSON.stringify(text.slice(0, 24))})`);
});

check("it is a 308 pointing at the https:// URL", () => {
  assert.match(text, /^HTTP\/1\.1 308 /, "308 Permanent Redirect");
  const loc = text.match(/\r\nLocation: (.+)\r\n/)?.[1];
  assert.ok(loc, "Location header present");
  assert.equal(loc, `https://${host}:${port}/`, `points at https (got ${loc})`);
  // 308 (not 302) so a POST keeps its method and body.
  assert.doesNotMatch(text, /^HTTP\/1\.1 30[12] /, "not a method-changing redirect");
});

const withQuery = (await get("/vms?filter=running")).toString("latin1");
check("path and query are preserved", () => {
  const loc = withQuery.match(/\r\nLocation: (.+)\r\n/)?.[1];
  assert.equal(loc, `https://${host}:${port}/vms?filter=running`);
});

const withHost = (await get("/dash", "vm.example.test")).toString("latin1");
check("the client's Host is echoed, with our TLS port", () => {
  const loc = withHost.match(/\r\nLocation: (.+)\r\n/)?.[1];
  assert.equal(loc, `https://vm.example.test:${port}/dash`,
    "hostname preserved so a reverse proxy or real hostname still works");
});

const posted = (await rawRequest(
  `POST /auth/login HTTP/1.1\r\nHost: ${host}:${port}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}`,
)).toString("latin1");
check("POST is redirected too, with a method-preserving status", () => {
  assert.match(posted, /^HTTP\/1\.1 308 /, "308 keeps POST a POST");
});

// ---- TLS still works on the same port

const tlsBody = await new Promise((resolve, reject) => {
  // SNI must not be an IP literal, so only set it for real hostnames.
  const isIp = net.isIP(host) !== 0;
  const sock = tls.connect(
    { host, port, rejectUnauthorized: false, ...(isIp ? {} : { servername: host }) },
    () => {
      sock.write(`GET /auth/status HTTP/1.1\r\nHost: ${host}:${port}\r\nConnection: close\r\n\r\n`);
    },
  );
  const chunks = [];
  sock.setTimeout(5000, () => {
    sock.destroy();
    reject(new Error("TLS timed out"));
  });
  sock.on("data", (c) => chunks.push(c));
  sock.on("end", () => resolve(Buffer.concat(chunks).toString("utf8")));
  sock.on("error", reject);
});

check("HTTPS on the same port is unaffected by the sniffing", () => {
  assert.match(tlsBody, /^HTTP\/1\.1 200 /, "TLS request succeeds");
  assert.match(tlsBody, /auth_disabled/, "and returns the real API payload");
});

// ---- the sniff must not wedge the listener

const after = (await get("/")).toString("latin1");
check("the listener keeps serving both after mixed traffic", () => {
  assert.match(after, /^HTTP\/1\.1 308 /, "still redirects plaintext");
});

// ---- the listener must answer on both IP families
//
// `localhost` resolves to ::1 alone on many systems, so a 0.0.0.0 bind refused
// https://localhost:8006 outright while 127.0.0.1 worked. Only meaningful when
// pointed at a loopback name/address.
const loopbackNames = ["localhost", "127.0.0.1", "::1"];
if (loopbackNames.includes(host)) {
  const reach = async (family) => {
    try {
      await new Promise((resolve, reject) => {
        const sock = net.connect({ host, port, family });
        sock.setTimeout(4000, () => {
          sock.destroy();
          reject(new Error("timeout"));
        });
        sock.on("connect", () => {
          sock.destroy();
          resolve();
        });
        sock.on("error", reject);
      });
      return true;
    } catch {
      return false;
    }
  };
  const v4 = await reach(4);
  const v6 = await reach(6);
  check("the port answers over both IPv4 and IPv6", () => {
    assert.ok(v4, "IPv4 reachable");
    assert.ok(v6, "IPv6 reachable — a 0.0.0.0 bind would refuse ::1, breaking `localhost`");
  });
} else {
  results.push(["info", `host ${host} is not loopback, skipping the dual-stack check`]);
}

for (const [s, msg] of results) {
  console.log(`${s === "pass" ? "✔" : s === "info" ? "·" : "✖"} ${msg}`);
}
const failed = results.filter(([s]) => s === "FAIL");
if (failed.length) {
  console.error(`\n${failed.length} check(s) failed`);
  process.exit(1);
}
console.log("\nall redirect checks passed");
