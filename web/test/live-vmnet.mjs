// VM Network tab, against a real running guest: live rate limits (checked on
// the host's tc qdiscs, not just the XML), IPAM reservations (checked in the
// daemon's store and in its DNS answers, with the bridge left untouched),
// hot-add, hot-remove, NICs added elsewhere appearing, and the old /firewall URL.
//
// Needs sudo (virsh, tc, reading /etc/virt-man/ipam.json) and a network named by
// VM_NET (default "internet") that has a DHCP range. Defines and boots its own
// throwaway guest from the same generator Create VM uses, and removes it.
//
//   node test/live-vmnet.mjs [base]      (default http://127.0.0.1:8799)

import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { JSDOM, VirtualConsole } from "jsdom";

const base = process.argv[2] ?? "http://127.0.0.1:8799";
const NET = process.env.VM_NET ?? "internet";
const VM = "vmtest-vmnet";

execFileSync("npx", ["vite", "build", "--logLevel", "error", "--outDir", "test/_live"],
  { stdio: "ignore", env: { ...process.env, VM_IIFE: "1" } });
execFileSync("npx", ["vite", "build", "--ssr", "src/xml.ts", "--outDir", "test/_build", "--logLevel", "error", "--emptyOutDir", "false"],
  { stdio: "ignore" });
const x = await import(new URL("./_build/xml.js", import.meta.url).href);
const code = readFileSync("test/_live/app.iife.js", "utf8");
const conn = (await (await fetch(base + "/connections")).json())[0];
const sh = (...a) => execFileSync("sudo", a).toString();
// The daemon's IPAM store: what its DHCP server will hand out.
const reserved = (mac) => JSON.parse(sh("cat", "/etc/virt-man/ipam.json")).networks[NET]?.reservations.find((r) => r.mac === mac);
const bridgeIndex = () => {
  const br = sh("virsh", "net-info", NET).match(/Bridge:\s+(\S+)/)?.[1];
  return readFileSync(`/sys/class/net/${br}/ifindex`, "utf8").trim();
};

const def = await fetch(`${base}/proxy/${conn.id}/domains?start=true`, {
  method: "POST", headers: { "Content-Type": "application/xml" },
  body: x.domainXml({ name: VM, cpus: 1, memory_mb: 128, graphics: "vnc", network: NET, disks: [] }),
});
if (!def.ok) {
  console.error(`could not define ${VM}: ${await def.text()}`);
  process.exit(1);
}

const results = [];
const check = (name, ok, detail = "") => results.push([!!ok, name, detail]);
let mac0;
try {
  const dom = new JSDOM(`<!doctype html><html><body><div id="app"></div></body></html>`, {
    url: `${base}/${encodeURIComponent(conn.name)}/vms/${VM}/network`,
    runScripts: "dangerously", pretendToBeVisual: true, virtualConsole: new VirtualConsole(),
  });
  const w = dom.window;
  const d = w.document;
  w.fetch = (i, init) => fetch(new URL(typeof i === "string" ? i : i.url, base).toString(), init);
  w.ResizeObserver ??= class { observe() {} unobserve() {} disconnect() {} };
  w.eval(code);
  const wait = (ms) => new Promise((r) => w.setTimeout(r, ms));
  const until = async (f, ms = 8000) => {
    for (let t = 0; t < ms; t += 100) { if (f()) return true; await wait(100); }
    return false;
  };
  const rows = () => [...d.querySelectorAll("#vmnet-nics tr[data-nic]")];
  const toasts = () => [...d.querySelectorAll(".toast")].map((t) => t.textContent.trim()).join(" | ");
  const macOf = (r) => r?.textContent.match(/([0-9a-f]{2}:){5}[0-9a-f]{2}/)?.[0];

  await until(() => rows().length);
  mac0 = macOf(rows()[0]);
  check("the Network tab opens from its URL", d.querySelector(".tab.active")?.textContent.trim() === "Network");
  check("the guest's NIC is listed with its network", rows().length === 1 && rows()[0].textContent.includes(NET));
  check("the firewall editor is on the same tab", await until(() => d.querySelector("#vmnet-fw #vmfw-apply")));

  // Rate limits: verified on the host, where they are enforced.
  rows()[0].querySelector("[data-nic-rate]").click(); await wait(50);
  d.querySelector("#rate-rx").value = "10";
  d.querySelector("#rate-tx").value = "20";
  d.querySelector("#rate-ok").click();
  check("the table shows both limits", await until(() => /10 Mbps[\s\S]*20 Mbps/.test(rows()[0]?.textContent ?? "")));
  const dev = sh("virsh", "domiflist", VM).match(new RegExp(`(vnet\\d+)\\s+network\\s+${NET}\\s+\\S+\\s+${mac0}`))?.[1];
  const cls = sh("tc", "class", "show", "dev", dev);
  const pol = sh("tc", "filter", "show", "dev", dev, "parent", "ffff:");
  check("RX limit is enforced on the host tap (htb, ~10 Mbit)", /rate 97\d\dKbit/.test(cls), cls.trim());
  check("TX limit is enforced by the ingress policer (~20 Mbit)", /rate 19\d\d\dKbit/.test(pol), pol.trim().slice(0, 120));

  // IPAM: in the daemon's store, served by its DNS, and the network is not
  // restarted to apply it (a restart recreates the bridge: a new ifindex).
  const bridgeBefore = bridgeIndex();
  rows()[0].querySelector("[data-nic-ipam]").click(); await wait(50);
  const ip = d.querySelector("#ipam-ip").value;
  check("IPAM suggests a free address", /^\d+\.\d+\.\d+\.\d+$/.test(ip), ip);
  d.querySelector("#ipam-ok").click();
  check("the table shows the reserved address", await until(() => rows()[0]?.textContent.includes(ip)));
  check("the reservation is in the daemon's IPAM, as a manual one", reserved(mac0)?.ip === ip && reserved(mac0)?.dynamic === false,
    JSON.stringify(reserved(mac0)));
  const gw = JSON.parse(sh("cat", "/etc/virt-man/ipam.json")).networks[NET].gateway;
  const dug = execFileSync("dig", ["+short", `@${gw}`, VM, "A"]).toString().trim();
  check("…and its name resolves to it on the network's DNS", dug === ip, dug);
  check("the network was not restarted to apply it", bridgeIndex() === bridgeBefore);

  // Hot-add from the tab.
  d.querySelector("#vmnet-add").click(); await wait(50);
  d.querySelector("#nic-ok").click();
  check("a hot-added interface appears in the table", await until(() => rows().length === 2), toasts());
  check("…and is attached to the running guest", (sh("virsh", "domiflist", VM).match(/vnet\d+/g) ?? []).length === 2);

  // Added by another path (the hardware editor / virsh): it just shows up.
  sh("virsh", "attach-interface", VM, "network", NET, "--model", "virtio", "--mac", "52:54:00:9a:00:09", "--live", "--config");
  d.querySelector('.tab[data-vmtab="summary"]').click(); await wait(800);
  d.querySelector('.tab[data-vmtab="network"]').click();
  check("a NIC added elsewhere appears in the tab", await until(() => rows().some((r) => macOf(r) === "52:54:00:9a:00:09")));

  // Hot-remove. This guest has no OS to acknowledge the ACPI eject, so the UI
  // must say the removal is pending rather than claim the NIC is gone.
  rows().find((r) => macOf(r) === "52:54:00:9a:00:09").querySelector("[data-nic-del]").click(); await wait(100);
  [...d.querySelectorAll(".modal-back.open .btn")].find((b) => /Remove/.test(b.textContent))?.click();
  check("an unacknowledged unplug is reported as pending", await until(() => /has not released it yet/.test(toasts())), toasts());
  check("removal takes the NIC out of the definition", !sh("virsh", "dumpxml", "--inactive", VM).includes("52:54:00:9a:00:09"));

  await until(() => rows()[0]?.querySelector("[data-nic-ipam]"));
  rows()[0].querySelector("[data-nic-ipam]").click(); await wait(50);
  check("an existing reservation offers Release", !!d.querySelector("#ipam-clear"));
  d.querySelector("#ipam-clear")?.click();
  check("Release removes the reservation", await until(() => !reserved(mac0)));
  check("…and still does not restart the network", bridgeIndex() === bridgeBefore);

  w.history.pushState(null, "", `/${encodeURIComponent(conn.name)}/vms/${VM}/firewall`);
  w.dispatchEvent(new w.PopStateEvent("popstate"));
  check("the old /firewall URL opens the Network tab",
    await until(() => d.querySelector(".tab.active")?.textContent.trim() === "Network" && w.location.pathname.endsWith("/network")),
    w.location.pathname);
  w.close();
} finally {
  try { sh("virsh", "destroy", VM); } catch { /* already off */ }
  try { sh("virsh", "undefine", VM); } catch { /* already gone */ }
  if (mac0) await fetch(`${base}/proxy/${conn.id}/networks/${NET}/hosts/${mac0}`, { method: "DELETE" }).catch(() => {});
}

for (const [ok, name, detail] of results) console.log(`${ok ? "\u2714" : "\u2716"} ${name}${ok ? "" : `  — ${detail}`}`);
const failed = results.filter(([ok]) => !ok);
if (failed.length) {
  console.error(`\n${failed.length} check(s) failed`);
  process.exit(1);
}
console.log("\nall VM network checks passed");
