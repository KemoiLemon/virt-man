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

const FIXTURE = "vmtest-suite";

function api(base, path, init) {
  return fetch(new URL(path, base).toString(), init);
}

async function json(base, path, init) {
  const res = await api(base, path, init);
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
async function firstConn(base) {
  const conns = await json(base, "/connections");
  if (!Array.isArray(conns) || !conns.length) {
    throw new Error(
      "no saved server on this backend — add one (or seed /auth/setup) before running live suites");
  }
  return conns[0].id;
}

/// Pick a pool to hold the fixture's disk, preferring an active dir pool.
async function pickPool(base, conn) {
  const pools = await json(base, `/proxy/${conn}/pools`).catch(() => []);
  if (!Array.isArray(pools)) return null;
  return (pools.find((p) => p.active) ?? pools[0])?.name ?? null;
}

async function pickNetwork(base, conn) {
  const nets = await json(base, `/proxy/${conn}/networks`).catch(() => []);
  if (!Array.isArray(nets)) return null;
  return (nets.find((n) => n.active) ?? nets[0])?.name ?? null;
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
function domainXml(name, pool, volume, network) {
  const disk = pool && volume
    ? `<disk type="volume" device="disk">
      <driver name="qemu" type="qcow2"/>
      <source pool="${pool}" volume="${volume}"/>
      <target dev="vda" bus="virtio"/>
    </disk>`
    : "";
  const nic = network
    ? `<interface type="network">
      <source network="${network}"/>
      <model type="virtio"/>
    </interface>`
    : "";
  return `<domain type="kvm">
  <name>${name}</name>
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
/// Returns `{ created, name }`. When the host already has a domain, that one
/// is used and `created` is false, so cleanup leaves the user's guest alone.
/// Otherwise a `vmtest-suite` domain is defined and must be passed back to
/// `removeTestVm`.
export async function ensureTestVm(base) {
  const conn = await firstConn(base);
  const existing = await json(base, `/proxy/${conn}/domains`).catch(() => []);
  if (Array.isArray(existing) && existing.length) {
    // Prefer a shut-off guest so the state-aware action assertions hold.
    const guest = existing.find((d) => !d.active) ?? existing[0];
    return { created: false, name: guest.name, conn };
  }

  const pool = await pickPool(base, conn);
  const network = await pickNetwork(base, conn);
  const volume = pool ? `${FIXTURE}.qcow2` : null;
  if (pool) {
    await json(base, `/proxy/${conn}/pools/${encodeURIComponent(pool)}/volumes`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ xml: volumeXml(volume) }),
    });
  }
  await json(base, `/proxy/${conn}/domains?start=false`, {
    method: "POST",
    headers: { "Content-Type": "application/xml" },
    body: domainXml(FIXTURE, pool, volume, network),
  });
  return { created: true, name: FIXTURE, conn };
}

/// Remove the fixture domain (and its disk) when this run created it.
/// No-op for a reused guest, so a real VM is never deleted by a test run.
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
}
