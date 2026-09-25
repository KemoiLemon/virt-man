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

export function networkXml(f: NetworkForm): string {
  const name = esc(f.name);
  const forward = f.forward === "nat" ? `<forward mode="nat"><nat><port start="1024" end="65535"/></nat></forward>`
    : f.forward === "bridge" ? `<forward mode="bridge"/>`
    : "";
  const bridge = f.bridge ? `<bridge name="${esc(f.bridge)}"/>` : "";
  let ip = "";
  if (f.ip) {
    const range = f.dhcp_start && f.dhcp_end
      ? `<dhcp><range start="${esc(f.dhcp_start)}" end="${esc(f.dhcp_end)}"/></dhcp>`
      : "";
    ip = `<ip address="${esc(f.ip.split("/")[0])}" netmask="${esc(netmask(f.ip))}">${range}</ip>`;
  }
  return `<network>
  <name>${name}</name>
  ${forward}
  ${bridge}
  ${ip}
</network>`;
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
