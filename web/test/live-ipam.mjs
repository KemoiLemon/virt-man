// The daemon's IPAM against a real DHCP client: boots the Alpine live ISO,
// logs in on the serial console and runs udhcpc, so the addresses checked
// here are what a guest actually received on the wire.
//
// Covers the reported bug (deleting a reservation, then restoring it, left the
// guest with no address), permanent dynamic reservations, DNS for guest names
// and forwarding, a VM deletion taking its reservation with it, and the access
// log recording the guest's connections with the names it looked up.
//
// Needs sudo, the Alpine live ISO in the ISOs pool (ISO env to override), and
// a daemon-served network (NET, default "internet").
//
//   node test/live-ipam.mjs [base]      (default http://127.0.0.1:8799)

import { execFileSync, spawn } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const base = process.argv[2] ?? "http://127.0.0.1:8799";
const NET = process.env.NET ?? "internet";
const ISO = process.env.ISO ?? "alpine-standard-3.24.2-x86_64.iso";
const VM = "vmtest-ipam";
const MAC = "52:54:00:d4:c0:0e";
const conn = (await (await fetch(base + "/connections")).json())[0];
const api = (path, init) => fetch(`${base}/proxy/${conn.id}${path}`, init).then((r) => r.json());
const sh = (...a) => execFileSync("sudo", a, { stdio: ["ignore", "pipe", "ignore"] }).toString();
const wait = (ms) => new Promise((r) => setTimeout(r, ms));
const results = [];
const check = (name, ok, detail = "") => results.push([!!ok, name, detail]);
const store = () => JSON.parse(sh("cat", "/etc/virt-man/ipam.json")).networks[NET];
const mine = () => store()?.reservations.find((r) => r.mac === MAC);

/// Type commands on the guest's serial console and return what it printed.
function consoleRun(cmds, settleMs = 4000) {
  return new Promise((resolve) => {
    const p = spawn("sudo", ["script", "-qfc", `virsh console --force ${VM}`, "/dev/null"], { stdio: ["pipe", "pipe", "ignore"] });
    let out = "";
    p.stdout.on("data", (d) => { out += d.toString(); });
    const steps = ["", ...cmds];
    let i = 0;
    const next = () => {
      if (i < steps.length) {
        p.stdin.write(`${steps[i++]}\r`);
        setTimeout(next, i === 1 ? 2500 : settleMs);
      } else {
        p.stdin.write("\x1d");
        setTimeout(() => { p.kill(); resolve(out); }, 500);
      }
    };
    setTimeout(next, 1500);
  });
}
const lease = (out) => [...out.matchAll(/lease of (\d+\.\d+\.\d+\.\d+) obtained/g)].map((m) => m[1]).at(-1);
const dhcp = async () => lease(await consoleRun(["ip addr flush dev eth0; udhcpc -i eth0 -n -q -t 6 2>&1 | tail -2"], 9000));

const tmp = mkdtempSync(join(tmpdir(), "vm-ipam-"));
try {
  // pgrep exits 1 when nothing matches, which is the answer we want here.
  let dnsmasq = "";
  try { dnsmasq = execFileSync("pgrep", ["-x", "dnsmasq", "-a"], { stdio: ["ignore", "pipe", "ignore"] }).toString(); } catch { /* none */ }
  check("the network is served by the daemon, not dnsmasq",
    !dnsmasq.split("\n").some((l) => l.includes(`/${NET}.conf`)) && (await api(`/networks/${NET}/ipam`)).serving === true);

  writeFileSync(join(tmp, "vm.xml"), `<domain type='kvm'><name>${VM}</name><memory unit='MiB'>512</memory><vcpu>1</vcpu>
    <os><type arch='x86_64' machine='q35'>hvm</type><boot dev='cdrom'/></os><features><acpi/><apic/></features><devices>
    <disk type='volume' device='cdrom'><source pool='ISOs' volume='${ISO}'/><target dev='sda' bus='sata'/><readonly/></disk>
    <interface type='network'><source network='${NET}'/><mac address='${MAC}'/><model type='virtio'/></interface>
    <serial type='pty'/><console type='pty'/></devices></domain>`);
  sh("virsh", "define", join(tmp, "vm.xml"));
  sh("virsh", "start", VM);
  // Boot to the login prompt, then log in.
  let booted = false;
  for (let i = 0; i < 30 && !booted; i++) {
    await wait(3000);
    booted = /login:/.test(await consoleRun([], 500));
  }
  check("the live ISO booted to a login prompt", booted);
  await consoleRun(["root", "ip link set eth0 up"], 2500);

  const first = await dhcp();
  const r = mine();
  check("a guest with no reservation gets an address from the range", !!first, first);
  check("…and it is kept as a permanent reservation, tied to the VM", r?.ip === first && r?.dynamic === true && !!r?.vm, JSON.stringify(r));
  check("it gets the same address again after releasing it", (await dhcp()) === first);

  // The reported bug: delete the reservation, then restore it.
  const br = readFileSync(`/sys/class/net/virbr1/ifindex`, "utf8").trim();
  await api(`/networks/${NET}/hosts/${MAC}`, { method: "DELETE" });
  const afterDelete = await dhcp();
  check("after its reservation is deleted the guest still gets an address", !!afterDelete, afterDelete);
  const restored = "192.168.167.77";
  await api(`/networks/${NET}/hosts`, { method: "PUT", headers: { "content-type": "application/json" },
    body: JSON.stringify({ mac: MAC, ip: restored, name: "ipamtest" }) });
  check("after restoring a reservation the guest gets exactly that address", (await dhcp()) === restored);
  check("…and the network was never restarted", readFileSync(`/sys/class/net/virbr1/ifindex`, "utf8").trim() === br);

  const dns = await consoleRun([`nslookup ipamtest 2>&1 | tail -2`, `echo FWD=$(nslookup alpinelinux.org 2>&1 | grep -A9 '^Name:' | grep -c '^Address')`, `cat /etc/resolv.conf`], 3000);
  check("the guest resolves guest names through the gateway", dns.includes(`Address: ${restored}`), dns.slice(-400));
  const fwd = Number(dns.match(/FWD=(\d+)/)?.[1] ?? 0);
  check("…and outside names are forwarded upstream", fwd >= 1, `FWD=${fwd}`);
  check("…with the gateway as its only nameserver", /nameserver 192\.168\.167\.1\s/.test(dns) && (dns.match(/^nameserver /gm) ?? []).length === 1);

  // Access log: the connection and the lookup behind it, attributed to this
  // VM, reached through the web server's store.
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

  await api(`/domains/${VM}/destroy`, { method: "POST" });
  await api(`/domains/${VM}`, { method: "DELETE" });
  let gone = false;
  for (let i = 0; i < 20 && !gone; i++) { await wait(1000); gone = !mine(); }
  check("deleting the VM deletes its reservation", gone);
} finally {
  try { sh("virsh", "destroy", VM); } catch { /* off */ }
  try { sh("virsh", "undefine", VM); } catch { /* gone */ }
  await api(`/networks/${NET}/hosts/${MAC}`, { method: "DELETE" }).catch(() => {});
  rmSync(tmp, { recursive: true, force: true });
}

for (const [ok, name, detail] of results) console.log(`${ok ? "\u2714" : "\u2716"} ${name}${ok ? "" : `  — ${detail}`}`);
const failed = results.filter(([ok]) => !ok);
if (failed.length) {
  console.error(`\n${failed.length} check(s) failed`);
  process.exit(1);
}
console.log("\nall IPAM checks passed");
