// Network XML builder/parser: modes emit the right elements and the edit
// form round-trips the live definition.
import assert from "node:assert/strict";
import test, { before } from "node:test";

let xml;

before(async () => {
  xml = await import("./_build/xml.js");
});

const form = (over = {}) => ({
  name: "net0",
  forward: "nat",
  bridge: "",
  ip: "",
  dhcp_start: "",
  dhcp_end: "",
  dns: "",
  ...over,
});

test("NAT networks emit a forward and their own subnet, no bridge", () => {
  const out = xml.networkXml(form({
    ip: "192.168.100.1/24",
    dhcp_start: "192.168.100.10",
    dhcp_end: "192.168.100.200",
  }));
  assert.match(out, /<forward mode="nat">/);
  assert.match(out, /<ip address="192\.168\.100\.1" netmask="255\.255\.255\.0">/);
  assert.match(out, /<range start="192\.168\.100\.10" end="192\.168\.100\.200"/);
  assert.doesNotMatch(out, /<bridge /);
});

test("NAT networks emit DNS forwarders before the ip element", () => {
  const out = xml.networkXml(form({
    ip: "192.168.100.1/24",
    dns: "1.1.1.1, 8.8.8.8",
  }));
  assert.match(out, /<dns>\s*<forwarder addr="1\.1\.1\.1"\/>\s*<forwarder addr="8\.8\.8\.8"\/>\s*<\/dns>/);
  assert.ok(out.indexOf("<dns>") < out.indexOf("<ip "), "dns precedes ip");
});

test("non-NAT modes ignore DNS forwarders", () => {
  assert.doesNotMatch(
    xml.networkXml(form({ forward: "bridge", bridge: "br0", dns: "1.1.1.1" })),
    /<dns>/,
  );
  assert.doesNotMatch(
    xml.networkXml(form({ forward: "none", dns: "1.1.1.1" })),
    /<dns>/,
  );
});

test("bridge networks attach to a bridge and define no ip", () => {
  const out = xml.networkXml(form({ forward: "bridge", bridge: "br0", ip: "10.0.0.1/24" }));
  assert.match(out, /<forward mode="bridge"\/>/);
  assert.match(out, /<bridge name="br0"\/>/);
  assert.doesNotMatch(out, /<ip /, "bridge networks must not define their own subnet");
});

test("isolated networks have no forward and may carry an ip", () => {
  const out = xml.networkXml(form({ forward: "none", ip: "10.9.0.1/24" }));
  assert.doesNotMatch(out, /<forward /);
  assert.doesNotMatch(out, /<bridge /);
  assert.match(out, /<ip address="10\.9\.0\.1"/);
});

test("the edit form round-trips a definition, preserving uuid", () => {
  const original = xml.networkXml(form({
    name: "existing",
    forward: "nat",
    ip: "192.168.77.1/24",
    dhcp_start: "192.168.77.50",
    dhcp_end: "192.168.77.99",
    dns: "1.1.1.1 9.9.9.9",
  }), "abc-123");
  const parsed = xml.networkFormFromXml(original);
  assert.equal(parsed.name, "existing");
  assert.equal(parsed.forward, "nat");
  assert.equal(parsed.ip, "192.168.77.1/24", "CIDR rebuilt from the netmask");
  assert.equal(parsed.dhcp_start, "192.168.77.50");
  assert.equal(parsed.dhcp_end, "192.168.77.99");
  assert.equal(parsed.dns, "1.1.1.1, 9.9.9.9", "forwarders re-joined");
  assert.equal(xml.networkUuid(original), "abc-123");
});

test("the parser accepts libvirt's single-quoted attributes", () => {
  const live = `<network>
  <name>live</name>
  <forward mode='nat'/>
  <bridge name='virbr0'/>
  <ip address='192.168.5.1' netmask='255.255.255.0'>
    <dhcp><range start='192.168.5.2' end='192.168.5.254'/></dhcp>
  </ip>
</network>`;
  const parsed = xml.networkFormFromXml(live);
  assert.equal(parsed.name, "live");
  assert.equal(parsed.forward, "nat");
  assert.equal(parsed.bridge, "virbr0");
  assert.equal(parsed.ip, "192.168.5.1/24");
  assert.equal(parsed.dhcp_start, "192.168.5.2");
});

test("a missing netmask still yields the bare address", () => {
  const out = xml.networkXml(form({ forward: "nat", ip: "10.1.2.3/24" }));
  const parsed = xml.networkFormFromXml(out);
  assert.equal(parsed.ip, "10.1.2.3/24");
});

test("DHCP host allocations round-trip through the network XML", () => {
  const out = xml.networkXml(form({
    ip: "192.168.100.1/24",
    dhcp_start: "192.168.100.10",
    dhcp_end: "192.168.100.200",
    hosts: [
      { mac: "52:54:00:aa:bb:cc", ip: "192.168.100.50", name: "worker" },
      { mac: "52:54:00:dd:ee:ff", ip: "192.168.100.51" },
    ],
  }));
  assert.match(out, /<host mac="52:54:00:aa:bb:cc" ip="192\.168\.100\.50" name="worker"\/>/);
  assert.match(out, /<host mac="52:54:00:dd:ee:ff" ip="192\.168\.100\.51"\/>/);
  const parsed = xml.networkFormFromXml(out);
  assert.equal(parsed.hosts.length, 2);
  assert.equal(parsed.hosts[0].mac, "52:54:00:aa:bb:cc");
  assert.equal(parsed.hosts[0].ip, "192.168.100.50");
  assert.equal(parsed.hosts[0].name, "worker");
  assert.equal(parsed.hosts[1].name, "");
});

test("a network with an ip but no range derives libvirt's default DHCP range", () => {
  assert.deepEqual(
    xml.defaultDhcpRange("192.168.167.1/24"),
    { start: "192.168.167.2", end: "192.168.167.254" },
  );
  assert.equal(xml.defaultDhcpRange("10.0.0.1/8")?.start, "10.0.0.2");
  assert.equal(xml.defaultDhcpRange(""), null);
});

test("nextFreeIp skips the gateway and allocated addresses", () => {
  const used = ["192.168.167.1", "192.168.167.2", "192.168.167.3"];
  assert.equal(xml.nextFreeIp("192.168.167.1/24", used), "192.168.167.4");
  assert.equal(
    xml.nextFreeIp("192.168.167.1/24", [], { start: "192.168.167.50", end: "192.168.167.50" }),
    "192.168.167.50",
  );
});

test("dhcpRemaining counts the pool minus the gateway and allocations", () => {
  // Default /24 pool is .2-.254 = 253 addresses; the gateway (.1, outside the
  // range anyway) and two static allocations are not available.
  const remaining = xml.dhcpRemaining("192.168.167.1/24", [
    "192.168.167.2",
    "192.168.167.10",
  ]);
  assert.equal(remaining.total, 253);
  assert.equal(remaining.free, 251);
});

test("dhcpRemaining treats an in-range gateway as taken", () => {
  // When the range includes the gateway address it must not be counted free.
  const remaining = xml.dhcpRemaining("10.0.0.1/29", [], {
    start: "10.0.0.1",
    end: "10.0.0.6",
  });
  assert.equal(remaining.total, 6);
  assert.equal(remaining.free, 5, "gateway excluded from the free count");
});

test("dhcpRemaining reports nothing for a network with no pool", () => {
  // Bridge-mode and subnet-less networks have no DHCP pool to speak of.
  assert.equal(xml.dhcpRemaining("", []), null);
  assert.equal(xml.dhcpRemaining("192.168.1.1/31", []), null, "no usable hosts");
});

test("UEFI guests get the real OVMF loader and a templated nvram store", () => {
  const out = xml.domainXml({
    name: "box",
    cpus: 2,
    memory_mb: 2048,
    firmware: "uefi",
    graphics: "vnc",
    existing_volume: "",
    new_disk: false,
    disk_size_gb: 10,
    disk_format: "qcow2",
    pool: "default",
    iso: "",
    iso_pool: "",
    network: "",
    bridge: "",
  });
  assert.match(out, /<loader readonly="yes" type="pflash" format="raw">\/usr\/share\/edk2\/x64\/OVMF_CODE\.4m\.fd<\/loader>/);
  assert.match(out, /<nvram template="\/usr\/share\/edk2\/x64\/OVMF_VARS\.4m\.fd"[^>]*>/);
  assert.match(out, /\/var\/lib\/libvirt\/qemu\/nvram\/box_VARS\.fd/);
});

test("BIOS guests have no loader or nvram", () => {
  const out = xml.domainXml({
    name: "box",
    cpus: 2,
    memory_mb: 2048,
    firmware: "bios",
    graphics: "vnc",
    existing_volume: "",
    new_disk: false,
    disk_size_gb: 10,
    disk_format: "qcow2",
    pool: "default",
    iso: "",
    iso_pool: "",
    network: "",
    bridge: "",
  });
  assert.doesNotMatch(out, /<loader/);
  assert.doesNotMatch(out, /<nvram/);
});

test("withFirmware swaps loader/nvram in place and strips on BIOS", () => {
  const uefi = xml.domainXml({
    name: "box", cpus: 2, memory_mb: 2048, firmware: "uefi", graphics: "vnc",
    existing_volume: "", new_disk: false, disk_size_gb: 10, disk_format: "qcow2",
    pool: "default", iso: "", iso_pool: "", network: "", bridge: "",
  });
  const bios = xml.withFirmware(uefi, "bios", "box");
  assert.doesNotMatch(bios, /<loader/);
  assert.doesNotMatch(bios, /<nvram/);
  assert.match(bios, /<os>\s*<type/, "plain os block");
  const back = xml.withFirmware(bios, "uefi", "box");
  assert.match(back, /OVMF_CODE\.4m\.fd/);
  assert.match(back, /OVMF_VARS\.4m\.fd/);
  assert.equal((back.match(/<loader/g) || []).length, 1, "exactly one loader after switching back");
  assert.equal((back.match(/<nvram/g) || []).length, 1, "exactly one nvram after switching back");
});

test("withFirmware leaves no dangling close tags on libvirt single-quoted XML", () => {
  // libvirt stores paired loader/nvram with single-quoted attributes and the
  // closing tags on their own lines; an over-eager opening-tag strip left
  // orphaned </loader> / </nvram> and failed define with a tag mismatch.
  const live = `<domain type='kvm'>
  <os>
    <type arch='x86_64' machine='pc-q35-11.1'>hvm</type>
    <loader readonly='yes' type='pflash' format='raw'>/usr/share/edk2/x64/OVMF_CODE.4m.fd</loader>
    <nvram template='/usr/share/edk2/x64/OVMF_VARS.4m.fd' format='raw'>/var/lib/libvirt/qemu/nvram/Alpine_VARS.fd</nvram>
    <boot dev='hd'/>
  </os>
</domain>`;
  const bios = xml.withFirmware(live, "bios", "Alpine");
  assert.doesNotMatch(bios, /<loader/);
  assert.doesNotMatch(bios, /<nvram/);
  assert.doesNotMatch(bios, /<\/loader>|<\/nvram>/, "no orphaned closing tags");
  assert.match(bios, /<os>\s*<type/, "os block intact");
  assert.match(bios, /<boot dev='hd'\/>/, "boot device survives");
});

test("an upload defaults its volume name to the picked file's name", () => {
  assert.equal(xml.volumeNameFromFile("debian-13.2.0-amd64-netinst.iso"), "debian-13.2.0-amd64-netinst.iso");
  // Surrounding whitespace would make a confusing volume name.
  assert.equal(xml.volumeNameFromFile("  arch.iso "), "arch.iso");
});

test("picking a file fills a blank name field", () => {
  assert.equal(xml.autoVolumeName("", "", "arch.iso"), "arch.iso");
  assert.equal(xml.autoVolumeName("   ", "", "arch.iso"), "arch.iso");
});

test("picking another file replaces our own previous auto-name", () => {
  // The field still holds what we filled in, so it is ours to update.
  assert.equal(xml.autoVolumeName("arch.iso", "arch.iso", "debian.iso"), "debian.iso");
});

test("a name the user typed is never overwritten", () => {
  // The whole point of the null return: re-picking a file must not discard a
  // custom name.
  assert.equal(xml.autoVolumeName("my custom name", "arch.iso", "debian.iso"), null);
});

// q35 gives each PCIe device its own root port and libvirt creates only as
// many as the boot devices need, so without spares a running guest cannot
// take a hot-added NIC ("No more available PCI slots", measured). Four spares
// were not enough -- boot devices consumed three -- so the count is pinned.
test("new guests carry spare PCIe root ports for hot-plug", () => {
  const out = xml.domainXml({ name: "g", cpus: 1, memory_mb: 512, graphics: "vnc", network: "n", disks: [] });
  const ports = out.match(/model="pcie-root-port"/g) ?? [];
  assert.equal(ports.length, xml.HOTPLUG_PORT_COUNT);
  assert.ok(xml.HOTPLUG_PORT_COUNT >= 8, "enough spares after the boot devices");
  // Index 0 is q35's pcie-root; a port without an index is rejected as index 0.
  assert.match(out, /<controller type="pci" index="0" model="pcie-root"\/>/);
  const idx = [...out.matchAll(/index="(\d+)" model="pcie-root-port"/g)].map((m) => Number(m[1]));
  assert.ok(idx.every((i) => i > 0) && new Set(idx).size === idx.length, "ports have distinct non-zero indices");
});
