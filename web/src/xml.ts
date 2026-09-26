// libvirt XML builders. The UI generates XML from forms so users never write
// XML by hand. Escape is minimal but sufficient for names/paths in practice.

export function esc(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&apos;");
}

export interface DomainForm {
  name: string;
  cpus: number;
  memory_mb: number;
  graphics: "vnc" | "spice";
  firmware: "uefi" | "bios";
  // existing disk attach
  existing_volume: string; // pool volume name or empty
  // or new disk
  new_disk: boolean;
  disk_size_gb: number;
  disk_format: "raw" | "qcow2";
  pool: string;
  // ISO / cdrom
  iso: string; // volume name in selected pool, or empty
  iso_pool: string;
  network: string; // empty = none
  bridge: string; // for bridge network name override (uses network name by default)
}

export function domainXml(f: DomainForm): string {
  const name = esc(f.name);
  const mem = Math.max(128, f.memory_mb) * 1024; // KiB
  const cpus = Math.max(1, f.cpus);
  const arch = "x86_64";
  const loader = f.firmware === "uefi"
    ? `<loader readonly="yes" type="pflash">/usr/share/edk2/x64/OVMF_CODE.fd</loader>`
    : "";
  const graphicsPort = f.graphics === "spice" ? "5901" : "5900";

  let disks = "";
  if (f.new_disk) {
    const fmt = f.disk_format === "qcow2" ? "qcow2" : "raw";
    disks += `
    <disk type="file" device="disk">
      <driver name="qemu" type="${fmt}"/>
      <source pool="${esc(f.pool)}"/>
      <target dev="vda" bus="virtio"/>
    </disk>`;
  } else if (f.existing_volume) {
    disks += `
    <disk type="volume" device="disk">
      <driver name="qemu" type="qcow2"/>
      <source pool="${esc(f.pool)}" volume="${esc(f.existing_volume)}"/>
      <target dev="vda" bus="virtio"/>
    </disk>`;
  }
  if (f.iso) {
    disks += `
    <disk type="volume" device="cdrom">
      <driver name="qemu" type="raw"/>
      <source pool="${esc(f.iso_pool)}" volume="${esc(f.iso)}"/>
      <target dev="sda" bus="sata"/>
      <readonly/>
    </disk>`;
  }

  const net = f.network
    ? `
    <interface type="network">
      <source network="${esc(f.network)}"/>
      <model type="virtio"/>
    </interface>`
    : "";

  return `<domain type="kvm">
  <name>${name}</name>
  <memory unit="KiB">${mem}</memory>
  <vcpu>${cpus}</vcpu>
  <os>
    <type arch="${arch}" machine="q35">hvm</type>
    ${loader}
  </os>
  <features><acpi/><apic/></features>
  <devices>
    <emulator>/usr/bin/qemu-system-x86_64</emulator>
    ${disks}
    ${net}
    <graphics type="${f.graphics}" port="${graphicsPort}" autoport="yes" listen="127.0.0.1"/>
    <video><model type="virtio"/></video>
    <serial type="pty"><target port="0"/></serial>
    <console type="pty"><target type="serial" port="0"/></console>
  </devices>
</domain>`;
}

export interface NetworkForm {
  name: string;
  forward: "nat" | "bridge" | "none";
  bridge: string;
  ip: string; // e.g. 192.168.100.1/24, or empty
  dhcp_start: string;
  dhcp_end: string;
}

/** Optional uuid from an existing definition, preserved so editing/renaming
 *  updates the network in place instead of defining a duplicate. */
export function networkXml(f: NetworkForm, uuid = ""): string {
  const name = esc(f.name);
  const id = uuid ? `<uuid>${esc(uuid)}</uuid>` : "";
  let forward = "";
  if (f.forward === "nat") {
    forward = `<forward mode="nat"><nat><port start="1024" end="65535"/></nat></forward>`;
  } else if (f.forward === "bridge") {
    forward = `<forward mode="bridge"/>`;
  }
  // Only a bridge-mode network attaches to a host bridge device.
  const bridge = f.forward === "bridge" && f.bridge ? `<bridge name="${esc(f.bridge)}"/>` : "";
  // NAT and isolated networks may carry their own subnet; a bridge network
  // takes its addressing from the bridge and must not define <ip>.
  let ip = "";
  if (f.forward !== "bridge" && f.ip) {
    const address = f.ip.split("/")[0];
    const range = f.dhcp_start && f.dhcp_end
      ? `<dhcp><range start="${esc(f.dhcp_start)}" end="${esc(f.dhcp_end)}"/></dhcp>`
      : "";
    ip = `<ip address="${esc(address)}" netmask="${esc(netmask(f.ip))}">${range}</ip>`;
  }
  return `<network>
  <name>${name}</name>
  ${id}
  ${forward}
  ${bridge}
  ${ip}
</network>`;
}

/// Parse a network's XML back into form fields, so the edit dialog can start
/// from the live definition instead of a blank form. Missing bits read empty.
/// Only attribute-free leaf tags (<name>, <bridge>, <uuid>) go through here;
/// <ip> carries attributes and is read directly in networkFormFromXml.
function tag(xml: string, name: string): string {
  const open = `<${name}>`;
  const close = `</${name}>`;
  const i = xml.indexOf(open);
  if (i < 0) return "";
  const j = xml.indexOf(close, i + open.length);
  if (j < 0) return "";
  return xml.slice(i + open.length, j);
}

export function networkFormFromXml(xml: string): NetworkForm {
  const forwardMatch = /<forward mode="([^"]+)"/.exec(xml);
  const forward = (forwardMatch?.[1] ?? "none") as NetworkForm["forward"];
  const ipMatch = /<ip[^>]*address="([^"]+)"/.exec(xml);
  const netmaskMatch = /<ip[^>]*netmask="([^"]+)"/.exec(xml);
  const range = /<range start="([^"]+)" end="([^"]+)"/.exec(xml);
  let cidr = ipMatch?.[1] ?? "";
  // The <ip> tag carries only the address; rebuild the CIDR from its netmask
  // so the field round-trips through the same netmask() path.
  if (cidr && netmaskMatch) {
    cidr = `${cidr}/${netmaskBits(netmaskMatch[1])}`;
  }
  return {
    name: tag(xml, "name"),
    forward,
    bridge: tag(xml, "bridge"),
    ip: cidr,
    dhcp_start: range?.[1] ?? "",
    dhcp_end: range?.[2] ?? "",
  };
}

export function networkUuid(xml: string): string {
  return tag(xml, "uuid");
}

function netmaskBits(mask: string): number {
  return mask.split(".").reduce((bits, octet) => {
    const n = parseInt(octet, 10);
    let ones = 0;
    for (let b = 7; b >= 0; b--) {
      if (n & (1 << b)) ones++;
      else break;
    }
    return bits + ones;
  }, 0);
}

function netmask(cidr: string): string {
  const bits = parseInt(cidr.split("/")[1] ?? "24", 10) || 24;
  const mask = bits === 0 ? 0 : (~0 << (32 - bits)) >>> 0;
  return [24, 16, 8, 0].map((s) => (mask >>> s) & 0xff).join(".");
}

export interface PoolForm {
  name: string;
  type: "dir" | "logical" | "fs";
  target: string; // path for dir/fs; blank for logical
  source: string; // source device for logical; source dir for fs
  source_format: string;
}

export function poolXml(f: PoolForm): string {
  const name = esc(f.name);
  const source = f.source
    ? `<source><device path="${esc(f.source)}"/><format type="${esc(f.source_format || "auto")}"/></source>`
    : "";
  const target = `<target><path>${esc(f.target || "")}</path></target>`;
  return `<pool type="${f.type}">
  <name>${name}</name>
  ${source}
  ${target}
</pool>`;
}

export interface VolumeForm {
  name: string;
  capacity_gb: number;
  format: "raw" | "qcow2";
}

export function volumeXml(f: VolumeForm): string {
  const cap = f.capacity_gb * 1024 * 1024 * 1024;
  return `<volume>
  <name>${esc(f.name)}</name>
  <capacity>${cap}</capacity>
  <target><format type="${f.format}"/></target>
</volume>`;
}
