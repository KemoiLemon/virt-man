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

test("file-backed disks are left alone rather than mangled", () => {
  const xml = `<domain><devices><disk type="file" device="disk">
    <source file="/var/lib/libvirt/images/x.qcow2"/><target dev="vda" bus="virtio"/>
  </disk></devices></domain>`;
  const got = hw.devicesFromXml(xml);
  assert.equal(got.filter((d) => d.kind === "disk").length, 0,
    "a non-volume disk is not claimed by the editor");
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
