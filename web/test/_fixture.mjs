// Throwaway libvirt domain for the live suites.
//
// The live suites drive the real SPA against a real daemon, so they need a
// guest to select and inspect. Relying on one already existing on the host
// made them fail on any machine that happened to have none (and made them
// touch whatever guest the user did have). Instead each suite calls
// `ensureTestVm` up front: it reuses an existing domain if there is one, and
// otherwise defines a disposable `vmtest-suite` and tears it down again.
//
// Talks to the web backend directly over HTTP — before the SPA boots, so the
// tree already lists the guest on first paint.

import { randomUUID } from "node:crypto";
import { mkdirSync, rmSync } from "node:fs";
import { join } from "node:path";

const FIXTURE = "vmtest-suite";
const TEST_NET_TITLE = "vmtest-net";
const TEST_POOL_TITLE = "vmtest-pool";

function api(base, path, init, f = fetch) {
  return f(new URL(path, base).toString(), init);
}

async function json(base, path, init, f = fetch) {
  const res = await api(base, path, init, f);
  const text = await res.text();
  if (!res.ok) throw new Error(`${path}: HTTP ${res.status} ${text.slice(0, 200)}`);
  try {
    return JSON.parse(text);
  } catch {
    return text;
  }
}

/// The daemon connection the SPA will show. The suites all drive the first
/// server, so the fixture must provision against that same one.
async function firstConn(base, f = fetch) {
  const conns = await json(base, "/connections", undefined, f);
  if (!Array.isArray(conns) || !conns.length) {
    throw new Error(
      "no saved server on this backend — add one (or seed /auth/setup) before running live suites");
  }
  return conns[0].id;
}

/// Where a test pool roots. Inside the repo, so it is an absolute directory the
/// daemon accepts: `/tmp`, `/var/tmp`, `/var/lib/virt-man*` and the daemon's own
/// state are in its deny list. Created here as the suite's own user, so the
/// suite can remove it again afterwards even though the daemon (root) writes the
/// volumes into it.
function testPoolDir() {
  const dir = join(process.cwd(), "test", "_pools", `${process.pid}-${randomUUID().slice(0, 8)}`);
  mkdirSync(dir, { recursive: true });
  return dir;
}

/// The pool's directory, read back from its rendered definition. The list does
/// not carry it, and a reused pool still needs it to drop a file in by hand.
async function poolRoot(base, conn, name, f) {
  try {
    const { xml } = await json(
      base, `/proxy/${conn}/pools/${encodeURIComponent(name)}/xml`, undefined, f);
    return xml.match(/<path>([^<]*)<\/path>/)?.[1] ?? null;
  } catch {
    return null;
  }
}

/// A native directory pool for the suite's volumes.
///
/// Pools are the daemon's own directories now, and a greenfield host has none,
/// so a suite that needs storage provisions one. Any active pool is reused
/// (`created:false`, so cleanup leaves it alone); otherwise a `vmtest-pool` is
/// created at a throwaway directory. A non-admin can only reuse: creation is
/// administrator-only, so pass `opts.fetch`/`opts.conn` from a logged-in page
/// and this returns the pool that administrator already made.
///
/// Returns `{ created, name, uuid, title, path, conn }`. Pass the object back to
/// `removeTestPool`.
export async function ensureTestPool(base, opts = {}) {
  const f = opts.fetch ?? fetch;
  const conn = opts.conn ?? (await firstConn(base, f));
  const pools = await json(base, `/proxy/${conn}/pools`, undefined, f).catch(() => []);
  const existing = Array.isArray(pools) ? pools.find((p) => p.active) : null;
  if (existing) {
    return {
      created: false,
      name: existing.name,
      uuid: existing.uuid ?? existing.name,
      title: existing.title || existing.name,
      path: await poolRoot(base, conn, existing.name, f),
      conn,
    };
  }

  const name = randomUUID();
  const dir = testPoolDir();
  // A pool is named by its UUID; what a person typed is its title.
  const xml = `<pool type='dir'><name>${name}</name>` +
    `<target><path>${dir}</path></target></pool>`;
  try {
    await json(
      base,
      `/proxy/${conn}/pools?title=${encodeURIComponent(TEST_POOL_TITLE)}`,
      { method: "POST", headers: { "Content-Type": "application/xml" }, body: xml },
      f,
    );
  } catch (e) {
    rmSync(dir, { recursive: true, force: true });
    throw new Error(
      `could not create the test pool '${TEST_POOL_TITLE}' at ${dir}: ${e.message}. ` +
      "A non-admin needs an administrator-created pool to reuse.");
  }

  let made = null;
  for (let i = 0; i < 30 && !made; i++) {
    const list = await json(base, `/proxy/${conn}/pools`, undefined, f).catch(() => []);
    made = Array.isArray(list) ? list.find((p) => p.name === name && p.active) : null;
    if (!made) await new Promise((r) => setTimeout(r, 200));
  }
  if (!made) {
    throw new Error(`the test pool ${name} was created but is not active`);
  }
  return { created: true, name, uuid: name, title: TEST_POOL_TITLE, path: dir, conn };
}

/// Delete the pool `ensureTestPool` created, and the throwaway directory with
/// it. A reused pool is never touched, and a failed delete must not fail the
/// suite. The daemon removes only the definition; the files are ours to drop.
export async function removeTestPool(base, pool, opts = {}) {
  if (!pool?.created) return;
  try {
    await json(
      base,
      `/proxy/${pool.conn}/pools/${encodeURIComponent(pool.name)}`,
      { method: "DELETE" },
      opts.fetch ?? fetch,
    );
  } catch {
    // Best effort: leaving a test pool behind is better than throwing during
    // cleanup, but its directory is still ours to remove.
  }
  if (pool.path) {
    try {
      rmSync(pool.path, { recursive: true, force: true });
    } catch {
      // Root-owned leftovers are better than a cleanup that throws.
    }
  }
}

/// Pick a pool to hold the fixture's disk, preferring an active dir pool.
async function pickPool(base, conn) {
  const pools = await json(base, `/proxy/${conn}/pools`).catch(() => []);
  if (!Array.isArray(pools)) return null;
  return (pools.find((p) => p.active) ?? pools[0])?.name ?? null;
}

/// Pick a network to attach the fixture's NIC to, by the pair the guest needs:
/// the key the app names it by, and the bridge the NIC is actually attached to.
async function pickNetwork(base, conn) {
  const nets = await json(base, `/proxy/${conn}/networks`).catch(() => []);
  if (!Array.isArray(nets)) return null;
  const n = nets.find((x) => x.active) ?? nets[0];
  return n ? { name: n.name, bridge: n.bridge } : null;
}

/// A NAT network the suites can put a guest on.
///
/// Networks are the daemon's own, not libvirt's, and a greenfield host has none
/// — there is no "internet" network to assume. Any active NAT network is reused
/// (`created:false`, so cleanup leaves it alone); otherwise `vmtest-net` is
/// created through the API. Its `<ip>` is left out on purpose: the daemon gives
/// a NAT network without one a free /24 and starts its bridge and dnsmasq.
///
/// Returns `{ created, name, title, bridge, gateway, conn }`. `name` is the
/// network's key (a UUID); `title` is what the SPA displays and `bridge` is what
/// a guest NIC names. Pass the object back to `removeTestNetwork`.
export async function ensureTestNetwork(base) {
  const conn = await firstConn(base);
  const nets = await json(base, `/proxy/${conn}/networks`).catch(() => []);
  const nat = Array.isArray(nets) ? nets.find((n) => n.active && n.forward === "nat") : null;
  if (nat) {
    // `ip` is the gateway with its prefix, e.g. 10.44.0.1/24.
    return {
      created: false,
      name: nat.name,
      title: nat.title || nat.name,
      bridge: nat.bridge,
      gateway: (nat.ip || "").split("/")[0],
      conn,
    };
  }

  const name = randomUUID();
  const xml = `<network>
  <name>${name}</name>
  <title>${TEST_NET_TITLE}</title>
  <forward mode="nat"/>
</network>`;
  try {
    await json(base, `/proxy/${conn}/networks`, {
      method: "POST",
      headers: { "Content-Type": "application/xml" },
      body: xml,
    });
  } catch (e) {
    throw new Error(
      `could not create the test network '${TEST_NET_TITLE}': ${e.message}. ` +
      "The daemon must be reachable and able to create a bridge and start dnsmasq (SSL/root).");
  }

  let made = null;
  for (let i = 0; i < 30 && !made; i++) {
    const list = await json(base, `/proxy/${conn}/networks`).catch(() => []);
    made = Array.isArray(list) ? list.find((n) => n.name === name && n.active) : null;
    if (!made) await new Promise((r) => setTimeout(r, 200));
  }
  if (!made) {
    throw new Error(
      `the test network ${name} was created but is not active: its bridge or dnsmasq did not start`);
  }
  return {
    created: true,
    name,
    title: TEST_NET_TITLE,
    bridge: made.bridge,
    gateway: (made.ip || "").split("/")[0],
    conn,
  };
}

/// Delete the network `ensureTestNetwork` created. Best effort: a reused
/// network is never touched, and a failed delete must not fail the suite.
export async function removeTestNetwork(base, net) {
  if (!net?.created) return;
  try {
    await json(base, `/proxy/${net.conn}/networks/${encodeURIComponent(net.name)}`,
      { method: "DELETE" });
  } catch {
    // Leaving a test network behind is better than throwing during cleanup.
  }
}

function volumeXml(name) {
  return `<volume>
  <name>${name}</name>
  <capacity unit="GiB">1</capacity>
  <target><format type="qcow2"/></target>
</volume>`;
}

/// A minimal VNC guest: disk (when a pool exists), one NIC (when a network
/// exists) and the pty serial/console pair the app's reader expects. Defined
/// shut off, which is what the state-aware action checks want.
function domainXml(name, title, pool, volume, network) {
  const disk = pool && volume
    ? `<disk type="file" device="disk">
      <driver name="qemu" type="qcow2"/>
      <source file="${pool.path}/${volume}"/>
      <target dev="vda" bus="virtio"/>
    </disk>`
    : "";
  // The networks are the daemon's own, so a guest is attached to one's bridge;
  // naming a libvirt network here defines a NIC nothing can resolve.
  const nic = network
    ? `<interface type="bridge">
      <source bridge="${network.bridge}"/>
      <model type="virtio"/>
    </interface>`
    : "";
  return `<domain type="kvm">
  <name>${name}</name>
  <title>${title}</title>
  <memory unit="KiB">524288</memory>
  <vcpu>1</vcpu>
  <os><type arch="x86_64" machine="q35">hvm</type></os>
  <features><acpi/><apic/></features>
  <devices>
    <emulator>/usr/bin/qemu-system-x86_64</emulator>
    ${disk}
    ${nic}
    <graphics type="vnc" port="5900" autoport="yes" listen="127.0.0.1"/>
    <video><model type="virtio"/></video>
    <serial type="pty"><target port="0"/></serial>
    <console type="pty"><target type="serial" port="0"/></console>
  </devices>
</domain>`;
}

/// Ensure a guest exists for this suite run.
///
/// Returns `{ created, name, conn, network? }`. When the host already has a
/// domain, that one is used and `created` is false, so cleanup leaves the
/// user's guest alone. Otherwise a `vmtest-suite` domain is defined and must be
/// passed back to `removeTestVm`. Its NIC needs a network, and a greenfield host
/// has none, so one is provisioned through `ensureTestNetwork` when no active
/// network exists (and removed again with the guest, unless it was reused).
export async function ensureTestVm(base) {
  const conn = await firstConn(base);
  const existing = await json(base, `/proxy/${conn}/domains`).catch(() => []);
  if (Array.isArray(existing) && existing.length) {
    // Prefer a shut-off guest so the state-aware action assertions hold.
    const guest = existing.find((d) => !d.active) ?? existing[0];
    return { created: false, name: guest.name, conn };
  }

  const pool = await pickPool(base, conn);
  const poolPath = pool ? await poolRoot(base, conn, pool) : null;
  const diskPool = poolPath ? { path: poolPath } : null;
  const picked = await pickNetwork(base, conn);
  const network = picked
    ? { created: false, name: picked.name, bridge: picked.bridge, conn }
    : await ensureTestNetwork(base);
  const fixtureId = randomUUID();
  const volume = diskPool ? `${fixtureId}.qcow2` : null;
  if (pool && volume) {
    await json(base, `/proxy/${conn}/pools/${encodeURIComponent(pool)}/volumes`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ xml: volumeXml(volume) }),
    });
  }
  await json(base, `/proxy/${conn}/domains?start=false`, {
    method: "POST",
    headers: { "Content-Type": "application/xml" },
    body: domainXml(fixtureId, FIXTURE, diskPool, volume, network),
  }).catch(async (e) => {
    // The volume was made before the domain. If the define is refused, the
    // disk would never be referenced again -- the orphan this fixture used
    // to leave behind.
    await dropVolume(base, conn, pool, volume);
    throw e;
  });
  return {
    created: true, name: fixtureId, conn, network,
    volume: pool && volume ? { pool, name: volume } : null,
  };
}

/// Best-effort delete of a volume a fixture created. A failed delete leaves
/// nothing worse than an orphaned test disk, so it never throws.
async function dropVolume(base, conn, pool, volume) {
  if (!pool || !volume) return;
  try {
    await json(
      base,
      `/proxy/${conn}/pools/${encodeURIComponent(pool)}/volumes/${encodeURIComponent(volume)}`,
      { method: "DELETE" });
  } catch {
    // cleanup is best effort
  }
}

/// Remove the fixture domain (and its disk) when this run created it, then the
/// network it had to create for the NIC. No-op for a reused guest, so a real VM
/// is never deleted by a test run.
export async function removeTestVm(base, fixture) {
  if (!fixture?.created) return;
  try {
    await json(
      base,
      `/proxy/${fixture.conn}/domains/${encodeURIComponent(fixture.name)}?remove_storage=true`,
      { method: "DELETE" });
  } catch {
    // Best effort: leaving a shut-off fixture behind is better than failing
    // the suite on cleanup.
  }
  // `remove_storage` can miss a disk the daemon could not match to the
  // definition. The fixture created this volume and knows its name, so remove
  // it explicitly rather than trusting the domain delete.
  await dropVolume(base, fixture.conn, fixture.volume?.pool, fixture.volume?.name);
  await removeTestNetwork(base, fixture.network);
}
