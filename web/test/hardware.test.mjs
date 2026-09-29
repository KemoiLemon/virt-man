// Hardware editor device <-> XML round-trip.
//
// Editing an existing guest rewrites its domain XML, so a parse/serialise
// mistake would silently drop real devices. These pin the mapping.
//
// Run: pnpm test

import assert from "node:assert/strict";
import test, { before } from "node:test";

let hw;
before(async () => { hw = await import("./_build/hardware.js"); });

test("catalogue entries all declare an id, label and icon", () => {
  assert.ok(hw.HW_CATALOG.length >= 6, `${hw.HW_CATALOG.length} device types`);
  for (const t of hw.HW_CATALOG) {
    assert.ok(t.id && t.label && t.icon, `complete entry: ${JSON.stringify(t.id)}`);
    assert.ok(Array.isArray(t.fields), `${t.id} declares fields`);
  }
});

test("single-instance devices are marked so duplicates can be blocked", () => {
  const single = new Set(hw.HW_CATALOG.filter((t) => t.single).map((t) => t.id));
  for (const id of ["memballoon", "watchdog", "tpm", "rng", "channel"]) {
    assert.ok(single.has(id), `${id} is single-instance`);
  }
  for (const id of ["disk", "interface", "cdrom", "hostdev", "usb"]) {
    assert.ok(!single.has(id), `${id} may repeat`);
  }
});

test("a volume-backed disk serialises to a libvirt volume disk", () => {
  const xml = hw.deviceXml({ kind: "disk", fields: { pool: "default", volume: "data.qcow2", bus: "virtio" } }, 1);
  assert.match(xml, /<disk type="volume" device="disk">/);
  assert.match(xml, /pool="default"/);
  assert.match(xml, /volume="data\.qcow2"/);
  assert.match(xml, /bus="virtio"/);
});

test("disk targets advance so two disks never collide", () => {
  const a = hw.deviceXml({ kind: "disk", fields: { pool: "p", volume: "a", bus: "virtio" } }, 0);
  const b = hw.deviceXml({ kind: "disk", fields: { pool: "p", volume: "b", bus: "virtio" } }, 1);
  const dev = (x) => x.match(/dev="([^"]+)"/)[1];
  assert.notEqual(dev(a), dev(b), `${dev(a)} vs ${dev(b)}`);
});

test("an interface serialises with its network and model", () => {
  const xml = hw.deviceXml({ kind: "interface", fields: { network: "internal", model: "virtio" } });
  assert.match(xml, /<interface type="network">/);
  assert.match(xml, /network="internal"/);
  assert.match(xml, /<model type="virtio"\/>/);
  assert.doesNotMatch(xml, /<mac/, "no mac element when unset");
});

test("an optional MAC is emitted only when provided", () => {
  const xml = hw.deviceXml({ kind: "interface", fields: { network: "n", mac: "52:54:00:aa:bb:cc" } });
  assert.match(xml, /address="52:54:00:aa:bb:cc"/);
});

test("device values are escaped into the XML", () => {
  const xml = hw.deviceXml({ kind: "interface", fields: { network: 'a"&<b' } });
  assert.doesNotMatch(xml, /network="a"&<b"/, "raw value must not land in the attribute");
  assert.match(xml, /&quot;|&amp;|&lt;/, "special characters are escaped");
});

test("validation rejects a device missing required fields", () => {
  assert.ok(hw.validateDevice({ kind: "disk", fields: {} }), "empty disk is rejected");
  assert.ok(hw.validateDevice({ kind: "interface", fields: {} }), "empty interface is rejected");
  assert.equal(
    hw.validateDevice({ kind: "disk", fields: { pool: "p", volume: "v" } }), null,
    "a complete disk passes",
  );
  assert.equal(hw.validateDevice({ kind: "memballoon", fields: {} }), null, "fieldless device passes");
});

test("a new-volume disk needs a pool and size, not an existing volume", () => {
  assert.ok(
    hw.validateDevice({ kind: "disk", fields: { mode: "new", pool: "p" } }),
    "new disk without size is rejected",
  );
  assert.equal(
    hw.validateDevice({ kind: "disk", fields: { mode: "new", pool: "p", size_gb: "10" } }), null,
    "a new disk with a size passes",
  );
  const xml = hw.deviceXml({ kind: "disk", fields: { mode: "new", pool: "p", size_gb: "20", format: "raw" } }, 1);
  assert.match(xml, /volume="\(new 20 GB raw\)"/, "preview marks the pending volume");
});

test("TPM and watchdog serialise to their libvirt elements", () => {
  assert.match(
    hw.deviceXml({ kind: "tpm", fields: { version: "2.0" } }),
    /<tpm model="tpm-crb">[\s\S]*version="2\.0"/,
  );
  assert.match(
    hw.deviceXml({ kind: "watchdog", fields: { action: "poweroff" } }),
    /<watchdog model="i6300esb" action="poweroff"\/>/,
  );
});

const SAMPLE = `<domain type="kvm">
  <name>alpine</name>
  <vcpu>2</vcpu>
  <memory unit="KiB">786432</memory>
  <devices>
    <emulator>/usr/bin/qemu-system-x86_64</emulator>
    <disk type="volume" device="disk">
      <driver name="qemu" type="qcow2"/>
      <source pool="default" volume="alpine.qcow2"/>
      <target dev="vda" bus="virtio"/>
    </disk>
    <interface type="network">
      <source network="internal"/>
      <model type="virtio"/>
    </interface>
    <memballoon model="virtio"/>
    <channel type="unix">
      <target type="virtio" name="org.qemu.guest_agent.0"/>
    </channel>
  </devices>
</domain>`;

test("devices parse back out of a real domain XML", () => {
  const got = hw.devicesFromXml(SAMPLE);
  const kinds = got.map((d) => d.kind).sort();
  assert.deepEqual(kinds, ["channel", "disk", "interface", "memballoon"]);
  const disk = got.find((d) => d.kind === "disk");
  assert.equal(disk.fields.pool, "default");
  assert.equal(disk.fields.volume, "alpine.qcow2");
  assert.equal(disk.fields.bus, "virtio");
  const nic = got.find((d) => d.kind === "interface");
  assert.equal(nic.fields.network, "internal");
});

test("a file-backed disk round-trips through its path rather than being dropped", () => {
  // The caller strips every <disk> block out of the definition and re-adds
  // only what devicesFromXml returns, so a disk this function does not claim
  // is silently deleted when hardware is saved. File-backed disks used to be
  // skipped here — which lost them — so claiming them by path is what makes
  // them survive. A path is also what every libvirt can resolve, unlike a
  // pool+volume reference.
  const xml = `<domain><devices><disk type="file" device="disk">
    <driver name="qemu" type="qcow2"/>
    <source file="/var/lib/libvirt/images/x.qcow2"/><target dev="vda" bus="virtio"/>
  </disk></devices></domain>`;
  const disks = hw.devicesFromXml(xml).filter((d) => d.kind === "disk");
  assert.equal(disks.length, 1, "the disk is claimed, not dropped");
  assert.equal(disks[0].fields.path, "/var/lib/libvirt/images/x.qcow2");
  assert.equal(disks[0].fields.format, "qcow2", "the driver type is carried, not assumed");
  assert.equal(disks[0].fields.bus, "virtio");
  assert.equal(hw.validateDevice(disks[0]), null, "a path-addressed disk needs no pool");
  const back = hw.deviceXml(disks[0], 1);
  assert.match(back, /<disk type="file" device="disk">/);
  assert.match(back, /<source file="\/var\/lib\/libvirt\/images\/x\.qcow2"\/>/);
  assert.doesNotMatch(back, /volume=/, "never re-emitted as a volume reference");
});

test("a raw file disk keeps raw as its driver type", () => {
  // Guessing qcow2 for a raw image makes qemu parse it as the wrong type and
  // the guest fails to start, so the format travels with the disk.
  const xml = hw.deviceXml({ kind: "disk", fields: { path: "/p/x.raw", format: "raw" } }, 1);
  assert.match(xml, /<driver name="qemu" type="raw"\/>/);
});

test("a cdrom addressed by path serialises as a file disk", () => {
  const xml = hw.deviceXml({ kind: "cdrom", fields: { path: "/p/inst.iso" } }, 1);
  assert.match(xml, /<disk type="file" device="cdrom">/);
  assert.match(xml, /<source file="\/p\/inst\.iso"\/>/);
  assert.match(xml, /<readonly\/>/);
  assert.equal(hw.validateDevice({ kind: "cdrom", fields: { path: "/p/inst.iso" } }), null);
});

test("an empty cdrom serialises as a source-less drive", () => {
  const xml = hw.deviceXml({ kind: "cdrom", fields: { pool: "default", volume: "" } }, 1);
  assert.match(xml, /<disk type="file" device="cdrom">/);
  assert.doesNotMatch(xml, /<source/, "no source when the tray is empty");
  assert.match(xml, /<readonly\/>/);
  assert.equal(hw.validateDevice({ kind: "cdrom", fields: { pool: "default", volume: "" } }), null,
    "an empty drive is valid");
});

test("a cdrom with an ISO serialises a volume source", () => {
  const xml = hw.deviceXml({ kind: "cdrom", fields: { pool: "iso", volume: "alpine.iso" } }, 1);
  assert.match(xml, /<disk type="volume" device="cdrom">/);
  assert.match(xml, /pool="iso" volume="alpine\.iso"/);
  assert.equal(hw.validateDevice({ kind: "cdrom", fields: { volume: "x.iso" } }), "A CD/DVD drive requires a pool",
    "a volume without its pool is rejected");
});

test("an empty cdrom parses back out instead of being dropped", () => {
  const xml = `<domain><devices><disk type="file" device="cdrom">
    <driver name="qemu" type="raw"/><target dev="sda" bus="sata"/><readonly/>
  </disk></devices></domain>`;
  const got = hw.devicesFromXml(xml);
  assert.equal(got.filter((d) => d.kind === "cdrom").length, 1, "empty drive round-trips");
  assert.equal(got.find((d) => d.kind === "cdrom").fields.volume, "");
});

test("withDevices splices the fragment inside <devices>", () => {
  const out = hw.withDevices(SAMPLE, [
    { kind: "rng", fields: { backend: "/dev/urandom" } },
  ]);
  assert.match(out, /<rng model="virtio">/);
  assert.ok(out.indexOf("<rng") < out.lastIndexOf("</devices>"), "inserted before </devices>");
  assert.match(out, /<emulator>/, "existing devices survive");
});

test("round trip: parse then re-serialise preserves the device set", () => {
  const devices = hw.devicesFromXml(SAMPLE);
  // Strip what the editor owns, then splice its set back in.
  const stripped = SAMPLE
    .replace(/<disk\b[^>]*device=['"](disk|cdrom)['"][\s\S]*?<\/disk>\s*/g, "")
    .replace(/<interface\b[^>]*type=['"]network['"][\s\S]*?<\/interface>\s*/g, "")
    .replace(/<memballoon\b[^>]*\/>\s*/g, "")
    .replace(/<channel\b[\s\S]*?org\.qemu\.guest_agent[\s\S]*?<\/channel>\s*/g, "");
  const rebuilt = hw.withDevices(stripped, devices);
  const again = hw.devicesFromXml(rebuilt);
  assert.deepEqual(
    again.map((d) => d.kind).sort(),
    devices.map((d) => d.kind).sort(),
    "no device is lost across a save",
  );
  assert.match(rebuilt, /<emulator>/, "untouched elements survive the rewrite");
  assert.match(rebuilt, /volume="alpine\.qcow2"/, "disk details survive");
});

test("summaries describe each device for the sidebar", () => {
  assert.match(hw.deviceSummary({ kind: "disk", fields: { pool: "p", volume: "v", bus: "virtio" } }), /p\/v/);
  assert.match(hw.deviceSummary({ kind: "interface", fields: { network: "n" } }), /n/);
  assert.equal(hw.deviceSummary({ kind: "memballoon", fields: {} }), "virtio");
});

test("watchdog and memballoon models survive a round trip", () => {
  const xml = `<domain><devices>
    <watchdog model="itco" action="reset"/>
    <memballoon model="virtio"><address type="pci" domain="0x0000" bus="0x03" slot="0x00" function="0x0"/></memballoon>
  </devices></domain>`;
  const got = hw.devicesFromXml(xml);
  const wd = got.find((d) => d.kind === "watchdog");
  assert.equal(wd.fields.model, "itco", "watchdog model is captured, not defaulted");
  assert.match(hw.deviceXml(wd, 1), /model="itco"/, "watchdog model is re-serialised");
  const bal = got.find((d) => d.kind === "memballoon");
  assert.equal(bal.fields.model, "virtio", "balloon model is captured");
  assert.match(hw.deviceXml(bal, 1), /<memballoon model="virtio"\/>/, "balloon re-serialises");
});

// Rate limits are set from the VM's Network tab. The hardware editor rebuilds
// every <interface> it manages, so it must carry the <bandwidth> through, or a
// hardware save would silently lift a guest's limits.
test("an interface's rate limit survives a hardware-editor round trip", () => {
  const xml = `<domain><devices>
    <interface type='network'>
      <mac address='52:54:00:aa:bb:01'/>
      <source network='internet'/>
      <model type='virtio'/>
      <bandwidth>
        <inbound average='1221'/>
        <outbound average='2441' peak='5000' burst='1024'/>
      </bandwidth>
    </interface></devices></domain>`;
  const [nic] = hw.devicesFromXml(xml).filter((d) => d.kind === "interface");
  const out = hw.deviceXml(nic);
  assert.match(out, /<inbound average="1221"\/>/);
  assert.match(out, /<outbound average="2441" peak="5000" burst="1024"\/>/);
  assert.equal((out.match(/<bandwidth>/g) ?? []).length, 1);
});

test("a NIC with no limit gains no <bandwidth> element", () => {
  const out = hw.deviceXml({ kind: "interface", fields: { network: "n", model: "virtio" } });
  assert.doesNotMatch(out, /bandwidth/);
});

test("a bandwidth block is rebuilt from numbers, not copied", () => {
  const xml = `<domain><devices><interface type='network'><source network='n'/>
    <bandwidth><inbound average='10' onload='x'/><outbound average='"><script>'/></bandwidth>
  </interface></devices></domain>`;
  const [nic] = hw.devicesFromXml(xml).filter((d) => d.kind === "interface");
  const out = hw.deviceXml(nic);
  assert.match(out, /<bandwidth><inbound average="10"\/><\/bandwidth>/);
  assert.doesNotMatch(out, /onload|script/);
});

test("a boot entry names the device it actually boots", () => {
  // libvirt resolves <boot dev='hd'> to the first disk, so labelling the kind
  // alone ("Hard Disk") leaves which of several disks unidentifiable after a
  // reorder.
  const xml = `<domain><os><boot dev='hd'/><boot dev='cdrom'/></os><devices>
    <disk type='file' device='disk'>
      <source file='/var/lib/libvirt/images/boot.qcow2'/><target dev='vda' bus='virtio'/>
    </disk>
    <disk type='file' device='disk'>
      <source file='/var/lib/libvirt/images/data.qcow2'/><target dev='vdb' bus='virtio'/>
    </disk>
    <disk type='volume' device='cdrom'>
      <source pool='iso' volume='alpine.iso'/><target dev='sda' bus='sata'/>
    </disk>
  </devices></domain>`;
  const t = hw.bootTargets(xml);
  assert.equal(t.hd, "boot.qcow2", "the first disk, not the second");
  assert.equal(t.cdrom, "alpine.iso");
  // network/fd have no backing device, so they carry no name.
  assert.equal(t.network, undefined);
});

test("a guest with no disks names nothing", () => {
  const t = hw.bootTargets("<domain><os><boot dev='hd'/></os><devices></devices></domain>");
  assert.deepEqual(t, {}, "an empty cdrom must not be named");
});
