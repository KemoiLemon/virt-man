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
  assert.equal(
    hw.validateDevice({ kind: "interface", fields: {} }), null,
    "an interface with no network is allowed: that is the form's 'None', and it " +
      "emits <source network=\"\"/>, which libvirt accepts",
  );
  assert.equal(
    hw.validateDevice({ kind: "disk", fields: { pool: "p", volume: "v" } }), null,
    "a complete disk passes",
  );
  assert.equal(hw.validateDevice({ kind: "memballoon", fields: {} }), null, "fieldless device passes");
});

/// "None" must still produce a NIC, and one whose source libvirt accepts: the
/// schema requires a `<source>`, not a named one.
test("an interface with no network still emits an empty source", () => {
  const xml = hw.deviceXml({ kind: "interface", fields: { network: "" } });
  assert.match(xml, /<interface[^>]*>/, `the interface element is kept: ${xml}`);
  assert.match(xml, /<source network=""\/>/, `an empty source, not no source: ${xml}`);
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

test("a summary shows the pool and network nickname, not their UUID", () => {
  // Pools and networks are named by their UUID and keep the human name as a
  // title, so the hardware tab used to show a UUID for anything made or
  // migrated since. The resolver is display-only: the underlying `fields` still
  // hold the libvirt names, which is what the XML is written from.
  const resolve = (kind, name) => {
    if (kind === "pool") return "Local images";
    return name === "net-a" ? "LAN" : name;
  };
  const disk = { kind: "disk", fields: { pool: "8f0d-uuid", volume: "a.qcow2", bus: "virtio" } };
  assert.equal(hw.deviceSummary(disk, resolve), "Local images/a.qcow2 (virtio)");
  assert.equal(
    hw.deviceSummary({ kind: "interface", fields: { network: "net-a", model: "virtio" } }, resolve),
    "LAN (virtio)",
  );
  // The cdrom summary names its pool too.
  assert.equal(
    hw.deviceSummary({ kind: "cdrom", fields: { pool: "8f0d-uuid", volume: "b.iso" } }, resolve),
    "Local images/b.iso",
  );
  // Without a resolver the libvirt name stands, so nothing depends on it.
  assert.equal(hw.deviceSummary(disk), "8f0d-uuid/a.qcow2 (virtio)");
});

test("Format is offered for a path-backed disk, not only a new volume", () => {
  // `deviceXml` writes `fields.format` as the driver type for a path-backed
  // disk as well, so hiding the field (as this used to, behind mode=new) meant
  // a value that was written but could not be seen or corrected. Guessing it
  // wrong makes qemu parse the file as the wrong image type.
  const disk = hw.HW_CATALOG.find((c) => c.id === "disk");
  const format = disk.fields.find((f) => f.key === "format");
  assert.ok(format, "the disk form offers Format");
  assert.equal(format.showWhen, undefined, "and shows it for every source mode");
});

test("a path-backed disk shows its path, and only when it has one", () => {
  // Our own guests address a disk by absolute path, and that path is what is
  // actually attached. Without a field for it the page described the disk as an
  // unnamed volume with no visible source, and the volume dropdown came up
  // blank -- the disk looked unattached.
  for (const id of ["disk", "cdrom"]) {
    const t = hw.HW_CATALOG.find((c) => c.id === id);
    const path = t.fields.find((f) => f.key === "path");
    assert.ok(path, `${id} offers a path field`);
    assert.equal(path.readonly, true, `${id}: it is derived, so not editable by hand`);
    assert.equal(path.hideWhenBlank, true, `${id}: hidden when there is no path`);
  }
});

test("a path-backed disk round-trips as file-backed", () => {
  // The guard against the bug that `resolveDevicePath` deleting an unresolved
  // path produced: `<source pool="" volume=""/>`, an unusable disk.
  const d = { kind: "disk", fields: { path: "/var/lib/libvirt/images/x.qcow2", format: "qcow2", bus: "virtio" } };
  const xml = hw.deviceXml(d, 0);
  assert.match(xml, /type="file"/);
  assert.match(xml, /<source file="\/var\/lib\/libvirt\/images\/x\.qcow2"\/>/);
  assert.doesNotMatch(xml, /pool=""/, "never an empty pool reference");
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
  // No interface in this XML, so there is nothing for a network boot to use.
  assert.equal(t.network, undefined);
  assert.equal(t.networkName, undefined);
});

test("a network boot entry names the interface's network and MAC", () => {
  // libvirt network-boots from the first NIC, so the entry has to say which one
  // that is: the network it sits on and its MAC.
  const xml = `<domain><os><boot dev='network'/></os><devices>
    <interface type='network'>
      <mac address='52:54:00:05:55:c9'/>
      <source network='internet'/>
      <model type='virtio'/>
    </interface>
  </devices></domain>`;
  const t = hw.bootTargets(xml);
  assert.equal(t.network, "internet 52:54:00:05:55:c9", "network and MAC, in one label");
  // The bare name travels separately, so a caller can swap in the network's
  // nickname without splitting the label on a space.
  assert.equal(t.networkName, "internet");

  // A NIC with no MAC in the XML (libvirt writes one once the guest has been
  // started) still names its network.
  const noMac = "<domain><devices><interface type='network'><source network='lan'/></interface></devices></domain>";
  assert.equal(hw.bootTargets(noMac).network, "lan");
  assert.equal(hw.bootTargets(noMac).networkName, "lan");
});

test("a guest with no disks names nothing", () => {
  const t = hw.bootTargets("<domain><os><boot dev='hd'/></os><devices></devices></domain>");
  assert.deepEqual(t, {}, "an empty cdrom must not be named");
});

test("the boot dropdown names the device and hides what is already added", () => {
  // Three things the old kind-only list got wrong: "Hard Disk" does not say
  // *which* disk libvirt will boot when several are attached; offering a kind
  // already in the order adds a second entry resolving to the same device; and
  // offering a kind the guest has no device for adds a `<boot>` element libvirt
  // accepts and never uses.
  const names = { hd: "Debian-13.qcow2", cdrom: "debian-netinst.iso", network: "internet 52:54:00:05:55:c9" };

  // Nothing added yet: each kind offered, named where it has a device.
  const all = hw.bootChoices([], names);
  assert.deepEqual(all.map((c) => c.value), ["hd", "cdrom", "network"]);
  assert.equal(all[0].label, "Debian-13.qcow2 (Hard Disk)", "the real disk is named");
  assert.equal(all[1].label, "debian-netinst.iso (CD/DVD)");
  // A network entry names the network and its MAC.
  assert.equal(all[2].label, "internet 52:54:00:05:55:c9 (Network)");

  // The default order leaves only what is not already in it.
  assert.deepEqual(hw.bootChoices(["hd", "cdrom"], names).map((c) => c.value), ["network"]);
  // Order does not matter for the filter, only membership.
  assert.deepEqual(hw.bootChoices(["cdrom", "hd"], names).map((c) => c.value), ["network"]);
  // Everything added: nothing left, so Add is disabled and no empty value is
  // pushed into the order.
  assert.deepEqual(hw.bootChoices(["hd", "cdrom", "network"], names), []);
});

test("a floppy is never offered: the editor cannot attach one", () => {
  // It was listed unconditionally, so a guest with no floppy drive was offered
  // a floppy boot option -- which writes `<boot dev='fd'/>` for a drive that
  // does not exist. `HW_CATALOG` has no floppy, so the option could only ever
  // be wrong.
  assert.ok(
    !hw.BOOT_DEVS.includes("fd"),
    `fd must not be offered, got ${JSON.stringify(hw.BOOT_DEVS)}`,
  );
  // Not even with a name for it, and not for any order.
  const withEveryName = { hd: "d.qcow2", cdrom: "c.iso", network: "net", fd: "disk.img" };
  for (const order of [[], ["hd"], ["hd", "cdrom", "network"]]) {
    assert.ok(
      !hw.bootChoices(order, withEveryName).some((c) => c.value === "fd"),
      `fd offered for order ${JSON.stringify(order)}`,
    );
  }
});

test("network boot is not offered without a NIC", () => {
  // A guest with no interface cannot network-boot, so offering it would add a
  // `<boot dev='network'/>` that libvirt accepts and never uses.
  const noNic = { hd: "d.qcow2", cdrom: "c.iso" };
  assert.deepEqual(
    hw.bootChoices([], noNic).map((c) => c.value),
    ["hd", "cdrom"],
    "no network entry without an interface",
  );
  // With a NIC it appears, named by network and MAC.
  const withNic = { ...noNic, network: "internet 52:54:00:05:55:c9" };
  assert.deepEqual(hw.bootChoices([], withNic).map((c) => c.value), ["hd", "cdrom", "network"]);
});

