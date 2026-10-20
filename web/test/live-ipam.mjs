// The daemon's IPAM against a real DHCP client: boots the Alpine live ISO,
// logs in on the serial console and runs udhcpc, so the addresses checked here
// are what a guest actually received on the wire.
//
// Networks are the daemon's own: it owns the bridge, gives every running guest
// a permanent reservation before anything asks, and runs one dnsmasq per active
// network (static-only DHCP, DNS for the reserved names, upstream forwarding).
// The suite therefore creates a NAT network when the host has none, reads the
// reservations through the API, and finds dnsmasq by its per-network config file
// — there is no libvirt network and no /etc/virt-man/ipam.json.
//
// Covers the reported bug (deleting a reservation, then restoring it, left the
// guest with no address), permanent dynamic reservations, DNS for guest names
// and forwarding, a VM deletion taking its reservation with it, and the access
// log recording the guest's connections with the names it looked up.
//
// Needs sudo and the Alpine live ISO in the ISOs pool (ISO env to override).
//
//   node test/live-ipam.mjs [base]      (default http://127.0.0.1:8799)

import { execFileSync, spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { existsSync, readFileSync } from "node:fs";
import { ensureTestNetwork, removeTestNetwork } from "./_fixture.mjs";

const base = process.argv[2] ?? "http://127.0.0.1:8799";
const ISO = process.env.ISO ?? "alpine-standard-3.24.2-x86_64.iso";
// A guest is named by its UUID; the UUID is also where its serial socket lives.
const VM = randomUUID();
const MAC = "52:54:00:d4:c0:0e";
const conn = (await (await fetch(base + "/connections")).json())[0];
const api = (path, init) => fetch(`${base}/proxy/${conn.id}${path}`, init).then((r) => r.json());
const wait = (ms) => new Promise((r) => setTimeout(r, ms));
const results = [];
const check = (name, ok, detail = "") => results.push([!!ok, name, detail]);

// A guest is given a disk by absolute path, so the ISO volume's path comes
// from the pool it lives in rather than from a pool+volume source.
let isoPath = "";
const pools = await api("/pools").catch(() => []);
for (const p of Array.isArray(pools) ? pools : []) {
  const vols = await api(`/pools/${encodeURIComponent(p.name)}/volumes`).catch(() => []);
  isoPath ||= (Array.isArray(vols) ? vols : []).find((v) => v.name === ISO)?.path ?? "";
}
if (!isoPath) {
  console.error(`the Alpine live ISO '${ISO}' is not in any pool; set ISO=<volume name>`);
  process.exit(1);
}

// The daemon's network: `name` is its key (a UUID), `bridge` is what the guest
// NIC names, `gateway` is where its DNS and DHCP answer from.
const net = await ensureTestNetwork(base);
const NET = net.name;
const NETP = `/networks/${encodeURIComponent(NET)}`;
const ipamInfo = await api(`${NETP}/ipam`);
const hosts = async () => (await api(`${NETP}/hosts`)).hosts ?? [];
const mine = async () => (await hosts()).find((r) => r.mac === MAC);
// The supervisor (every 5 s) is what allocates; nothing is created by asking
// DHCP. Poll rather than assuming an exchange made the reservation.
const waitForMine = async (ms = 15000) => {
  for (let t = 0; t < ms; t += 1000) {
    const r = await mine();
    if (r) return r;
    await wait(1000);
  }
  return await mine();
};

// The guest's serial console is a Unix socket in the daemon's runtime
// directory, not a pty. The suite assumes the daemon's standard data dir
// (`/var/lib/virt-man-daemon`). QEMU owns the socket as root, so a small
// Python bridge runs under sudo; `select` keeps both directions moving
// without a terminal in between.
const SERIAL = `/var/lib/virt-man-daemon/qemu/${VM}/serial.sock`;
const BRIDGE = `
import socket, select, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(sys.argv[1])
stdin = sys.stdin.buffer
open_stdin = True
while True:
    fds = [s] + ([stdin] if open_stdin else [])
    r, _, _ = select.select(fds, [], [], 0.2)
    if not r:
        # The caller closed its end: keep relaying until the guest is quiet for
        # a moment, rather than dropping the last reply.
        if not open_stdin: sys.exit(0)
        continue
    for f in r:
        if f is s:
            d = s.recv(65536)
            if not d: sys.exit(0)
            sys.stdout.buffer.write(d)
            sys.stdout.buffer.flush()
        else:
            d = stdin.read1(65536)
            if not d:
                open_stdin = False
            else:
                s.sendall(d)
`;
let serialMissing = false;
/// Type commands on the guest's serial console and return what it printed.
function consoleRun(cmds, settleMs = 4000) {
  if (!existsSync(SERIAL)) {
    // Say why once, and fail the checks that need it instead of waiting on a
    // connection that will never be made.
    if (!serialMissing) {
      serialMissing = true;
      console.error(`serial socket missing: ${SERIAL}`);
      check("the guest's serial console socket exists", false, SERIAL);
    }
    return Promise.resolve("");
  }
  return new Promise((resolve) => {
    const p = spawn("sudo", ["python3", "-u", "-c", BRIDGE, SERIAL], { stdio: ["pipe", "pipe", "ignore"] });
    let out = "";
    p.stdout.on("data", (d) => { out += d.toString(); });
    const steps = ["", ...cmds];
    let i = 0;
    const next = () => {
      if (i < steps.length) {
        p.stdin.write(`${steps[i++]}\r`);
        setTimeout(next, i === 1 ? 2500 : settleMs);
      } else {
        // Closing stdin ends the bridge, so the socket is free for the next
        // command batch.
        p.stdin.end();
        setTimeout(() => { p.kill(); resolve(out); }, 500);
      }
    };
    setTimeout(next, 1500);
  });
}
const lease = (out) => [...out.matchAll(/lease of (\d+\.\d+\.\d+\.\d+) obtained/g)].map((m) => m[1]).at(-1);
const dhcp = async () => lease(await consoleRun(["ip addr flush dev eth0; udhcpc -i eth0 -n -q -t 6 2>&1 | tail -2"], 9000));

try {
  // One dnsmasq per active network, each with its own config. pgrep exits 1
  // when nothing matches, which is the answer we want here.
  let dnsmasq = "";
  try {
    dnsmasq = execFileSync("pgrep", ["-f", `dnsmasq-${NET}.conf`],
      { stdio: ["ignore", "pipe", "ignore"] }).toString().trim();
  } catch { /* not running */ }
  check("dnsmasq serves the network, from its own config",
    dnsmasq.length > 0 && ipamInfo.serving === true, dnsmasq);

  const vmXml = `<domain type='kvm'><name>${VM}</name><memory unit='MiB'>512</memory><vcpu>1</vcpu>
    <os><type arch='x86_64' machine='q35'>hvm</type><boot dev='cdrom'/></os><features><acpi/><apic/></features><devices>
    <disk type='file' device='cdrom'><driver name='qemu' type='raw'/><source file='${isoPath}'/><target dev='sda' bus='sata'/><readonly/></disk>
    <interface type='bridge'><source bridge='${net.bridge}'/><mac address='${MAC}'/><model type='virtio'/></interface>
    <serial type='pty'/><console type='pty'/></devices></domain>`;
  const def = await fetch(`${base}/proxy/${conn.id}/domains?start=true`, {
    method: "POST", headers: { "Content-Type": "application/xml" },
    body: vmXml,
  });
  if (!def.ok) throw new Error(`could not define ${VM}: ${def.status} ${await def.text()}`);
  // QEMU creates the socket at start; give it a moment if the start just returned.
  for (let i = 0; i < 20 && !existsSync(SERIAL); i++) await wait(250);
  // Boot to the login prompt, then log in.
  let booted = false;
  for (let i = 0; i < 30 && !booted && !serialMissing; i++) {
    await wait(3000);
    booted = /login:/.test(await consoleRun([], 500));
  }
  check("the live ISO booted to a login prompt", booted);
  await consoleRun(["root", "ip link set eth0 up"], 2500);

  // Preallocated: the guest appearing on the bridge is what reserves an
  // address, so this is settled before the guest ever asks DHCP.
  const pre = await waitForMine();
  check("a guest on the bridge is given a permanent reservation", pre?.dynamic === true && !!pre?.vm, JSON.stringify(pre));
  const first = await dhcp();
  check("…and the DHCP lease is that reservation's address", !!first && first === pre?.ip, `${first} vs ${pre?.ip}`);
  check("it gets the same address again after releasing it", (await dhcp()) === first);

  // The reported bug: delete the reservation, then restore it.
  const br = readFileSync(`/sys/class/net/${net.bridge}/ifindex`, "utf8").trim();
  await api(`${NETP}/hosts/${MAC}`, { method: "DELETE" });
  const again = await waitForMine();
  check("after its reservation is deleted the guest is given a new one", !!again, JSON.stringify(again));
  const afterDelete = await dhcp();
  check("…and still gets an address", !!afterDelete && afterDelete === again?.ip, `${afterDelete} vs ${again?.ip}`);
  // A free high address: the allocator hands out the lowest free ones, so walk
  // down from the top of the range until one is not already reserved.
  const taken = new Set((await hosts()).map((h) => h.ip));
  const parts = ipamInfo.range_end.split(".").map(Number);
  const stem = parts.slice(0, 3).join(".");
  let restored = "";
  for (let n = parts[3]; n > 1 && !restored; n--) {
    const cand = `${stem}.${n}`;
    if (!taken.has(cand)) restored = cand;
  }
  await api(`${NETP}/hosts`, { method: "PUT", headers: { "content-type": "application/json" },
    body: JSON.stringify({ mac: MAC, ip: restored, name: "ipamtest" }) });
  check("after restoring a reservation the guest gets exactly that address", (await dhcp()) === restored);
  check("…and the network was never restarted", readFileSync(`/sys/class/net/${net.bridge}/ifindex`, "utf8").trim() === br);

  // Retried: the console read is a fixed window, and on a busy host the
  // guest's first lookup can land after it closes. The answer is what is
  // checked, not how fast it came.
  let dns = "";
  for (let i = 0; i < 3 && !dns.includes(`Address: ${restored}`); i++) {
    dns = await consoleRun([`nslookup ipamtest 2>&1 | tail -2`, `echo FWD=$(nslookup alpinelinux.org 2>&1 | grep -A9 '^Name:' | grep -c '^Address')`, `cat /etc/resolv.conf`], 3000);
  }
  check("the guest resolves guest names through the gateway", dns.includes(`Address: ${restored}`), dns.slice(-400));
  const fwd = Number(dns.match(/FWD=(\d+)/)?.[1] ?? 0);
  check("…and outside names are forwarded upstream", fwd >= 1, `FWD=${fwd}`);
  const gwRe = new RegExp(`nameserver ${net.gateway.replace(/\./g, "\\.")}\\s`);
  check("…with the gateway as its only nameserver",
    gwRe.test(dns) && (dns.match(/^nameserver /gm) ?? []).length === 1, dns.slice(-200));

  // Access log: the connection and the lookup behind it, attributed to this
  // VM, reached through the web server's store. The DNS row comes from
  // dnsmasq's query log, read at the supervisor's pass, so allow for one tick.
  await consoleRun(["wget -q -O /dev/null -T 5 http://example.org; echo FETCHED"], 6000);
  const logsFor = async (extra) => (await (await fetch(`${base}/access-logs?${new URLSearchParams({ conn: conn.id, vm: VM, since_ms: "0", ...extra })}`)).json());
  let conns = null;
  for (let i = 0; i < 20 && !conns?.rows?.some((r) => r.kind === "connection" && r.domain === "example.org"); i++) {
    await wait(1000);
    conns = await logsFor({ q: "example.org" });
  }
  const c = conns?.rows?.find((r) => r.kind === "connection" && r.domain === "example.org");
  check("the access log has the guest's HTTP connection, named by its DNS lookup", c?.proto === "tcp" && c?.dport === 80, JSON.stringify(conns?.rows?.slice(0, 3)));
  check("…and the lookup itself", conns?.rows?.some((r) => r.kind === "dns" && r.domain === "example.org"));
  const byDomain = await logsFor({ group: "domain" });
  check("grouping by domain counts what the guest reached", byDomain.groups?.some((g) => g.key === "example.org" && g.count >= 2), JSON.stringify(byDomain.groups));

  const onlyDns = await logsFor({ kind: "dns" });
  check("the Records filter shows only lookups when asked", onlyDns.rows?.length > 0 && onlyDns.rows.every((r) => r.kind === "dns"));
  const onlyConn = await logsFor({ kind: "connections" });
  check("…or only connections", onlyConn.rows?.length > 0 && onlyConn.rows.every((r) => r.kind === "connection"));

  // Live packet capture: only while polled, decoded, exportable, stoppable.
  const cap = await api(`/domains/${VM}/capture`, { method: "POST", headers: { "content-type": "application/json" }, body: "{}" });
  check("a packet capture starts on the guest's interface", !!cap.id && cap.interfaces?.length === 1, JSON.stringify(cap));
  await consoleRun(["ping -c2 -W2 1.1.1.1 >/dev/null; nslookup github.com >/dev/null; echo PINGED"], 6000);
  const got = await api(`/domains/${VM}/capture/${cap.id}?after=0`);
  const lines = got.lines ?? [];
  check("it streams the guest's ping, both ways",
    lines.some((l) => l.info === "echo request" && l.dir === "out") && lines.some((l) => l.info === "echo reply" && l.dir === "in"),
    JSON.stringify(lines.slice(0, 5)));
  check("…and its DNS question by name", lines.some((l) => l.proto === "dns" && /github\.com/.test(l.info)));
  const pcap = new Uint8Array(await (await fetch(`${base}/proxy/${conn.id}/domains/${VM}/capture/${cap.id}/pcap`)).arrayBuffer());
  check("the capture downloads as a pcap file", pcap.length > 24 && pcap[0] === 0xd4 && pcap[3] === 0xa1);
  await api(`/domains/${VM}/capture/${cap.id}`, { method: "DELETE" });
  const after = await fetch(`${base}/proxy/${conn.id}/domains/${VM}/capture/${cap.id}?after=0`);
  check("stopping it ends the capture", after.status === 404);

  await api(`/domains/${VM}/destroy`, { method: "POST" });
  await api(`/domains/${VM}`, { method: "DELETE" });
  let gone = false;
  for (let i = 0; i < 20 && !gone; i++) { await wait(1000); gone = !(await mine()); }
  check("deleting the VM deletes its reservation", gone);
} finally {
  // The delete stops the guest first, so it replaces destroy and undefine.
  try { await api(`/domains/${VM}`, { method: "DELETE" }); } catch { /* gone */ }
  await api(`${NETP}/hosts/${MAC}`, { method: "DELETE" }).catch(() => {});
  await removeTestNetwork(base, net);
}

for (const [ok, name, detail] of results) console.log(`${ok ? "\u2714" : "\u2716"} ${name}${ok ? "" : `  — ${detail}`}`);
const failed = results.filter(([ok]) => !ok);
if (failed.length) {
  console.error(`\n${failed.length} check(s) failed`);
  process.exit(1);
}
console.log("\nall IPAM checks passed");
