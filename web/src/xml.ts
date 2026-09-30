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

/// OVMF loader + NVRAM fragment for a UEFI guest. The code path is the
/// read-only firmware; the nvram is the per-guest variable store, copied
/// from the OVMF_VARS template on first define. BIOS guests get nothing.
///
/// These are the paths edk2 installs on Arch; Debian and Alpine use
/// `/usr/share/OVMF/` instead. The SPA cannot see the host's filesystem, so the
/// daemon rewrites a path that does not exist there to one that does
/// (`daemon/src/firmware.rs`) — this is the shape to write, not the final path.
export function firmwareXml(firmware: "uefi" | "bios", name: string): string {
  if (firmware !== "uefi") return "";
  return `<loader readonly="yes" type="pflash" format="raw">/usr/share/edk2/x64/OVMF_CODE.4m.fd</loader>
    <nvram template="/usr/share/edk2/x64/OVMF_VARS.4m.fd" format="raw">/var/lib/libvirt/qemu/nvram/${esc(name)}_VARS.fd</nvram>`;
}

/// Rewrite an existing domain's firmware selection in place. Strips whatever
/// loader/nvram the guest already has (so a BIOS<->UEFI switch actually
/// takes effect) and splices the new fragment after `<type>`.
export function withFirmware(
  domainXml: string,
  firmware: "uefi" | "bios",
  name: string,
): string {
  // Match the paired form FIRST, then self-closing. The old order had the
  // self-closing pattern with an OPTIONAL slash, so it consumed the opening
  // tag of a paired loader/nvram and left the closing tag dangling — which
  // libvirt rejects as an opening/ending tag mismatch.
  let out = domainXml
    .replace(/\s*<loader\b[\s\S]*?<\/loader>\s*/g, "")
    .replace(/\s*<loader\b[^>]*\/>\s*/g, "")
    .replace(/\s*<nvram\b[\s\S]*?<\/nvram>\s*/g, "")
    .replace(/\s*<nvram\b[^>]*\/>\s*/g, "")
    // The `<firmware>` element goes with the attribute it depends on. Both are
    // libvirt's *feature-based autoselection*: with `firmware='efi'` present
    // libvirt reads `<firmware>` to mean "these features of the autoselected
    // firmware", and without it the element is illegal. Dropping the attribute
    // while keeping the element made every UEFI hardware save fail with
    // "cannot use feature-based firmware autoselection when firmware
    // autoselection is disabled" — which is what libvirt itself writes into a
    // domain's XML, so this was the normal case, not an edge one.
    .replace(/\s*<firmware\b[\s\S]*?<\/firmware>\s*/g, "")
    .replace(/\s*<firmware\b[^>]*\/>\s*/g, "")
    .replace(/(<os\b[^>]*)\s+firmware=['"][^'"]*['"]/g, "$1");
  const frag = firmwareXml(firmware, name);
  if (frag) {
    // After `<type>`, not after `<os>`: libvirt requires `<type>` to be the
    // first child of `<os>`. Splicing straight after the opening tag put
    // `<loader>` ahead of it, which reorders the definition for no reason even
    // where libvirt tolerated it.
    out = out.replace(/(<os\b[^>]*>\s*<type\b[^>]*>[^<]*<\/type>)/, `$1\n    ${frag}`);
  }
  return out;
}

export interface DomainForm {
  /// What the user calls it. Becomes the `<title>`: the libvirt name is the
  /// UUID (`uuid` below), so two guests may share a display name.
  name: string;
  /// libvirt's identity, generated for a new guest. When set, the guest is
  /// defined with `<name>` = this UUID and `<title>` = `name`.
  uuid?: string;
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
  /// Absolute path of the disk to attach, when it is known. Preferred over
  /// the pool+volume pair: a guest whose disks are addressed by path starts on
  /// a libvirt that cannot resolve volume sources. See `hardware.ts`.
  disk_path?: string;
  /// Absolute path of the ISO to attach as a cdrom, when known.
  iso_path?: string;
  pool: string;
  // ISO / cdrom
  iso: string; // volume name in selected pool, or empty
  iso_pool: string;
  network: string; // empty = none
  bridge: string; // for bridge network name override (uses network name by default)
}

export function domainXml(f: DomainForm): string {
  // Named by UUID when it has one, with the display name as the title; a
  // form without a UUID (tests, an older caller) keeps the old behaviour.
  const libvirtName = f.uuid ?? f.name;
  const name = esc(libvirtName);
  const identity = f.uuid
    ? `\n  <uuid>${esc(f.uuid)}</uuid>\n  <title>${esc(f.name)}</title>`
    : "";
  const mem = Math.max(128, f.memory_mb) * 1024; // KiB
  const cpus = Math.max(1, f.cpus);
  const arch = "x86_64";
  // The UEFI variable store is named after the libvirt name, like libvirt's.
  const loader = firmwareXml(f.firmware, libvirtName);
  const graphicsPort = f.graphics === "spice" ? "5901" : "5900";

  let disks = "";
  if (f.disk_path) {
    // File-backed, by absolute path. libvirt 11.3 cannot translate a
    // pool+volume source into a path, and a disk it cannot resolve never
    // reaches the guest's AppArmor profile — so qemu is refused permission to
    // open it and the guest will not start. A path resolves everywhere.
    const fmt = f.disk_format === "raw" ? "raw" : "qcow2";
    disks += `
    <disk type="file" device="disk">
      <driver name="qemu" type="${fmt}"/>
      <source file="${esc(f.disk_path)}"/>
      <target dev="vda" bus="virtio"/>
    </disk>`;
  } else if (f.new_disk) {
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
  if (f.iso_path) {
    disks += `
    <disk type="file" device="cdrom">
      <driver name="qemu" type="raw"/>
      <source file="${esc(f.iso_path)}"/>
      <target dev="sda" bus="sata"/>
      <readonly/>
    </disk>`;
  } else if (f.iso) {
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
  <name>${name}</name>${identity}
  <memory unit="KiB">${mem}</memory>
  <vcpu>${cpus}</vcpu>
  <os>
    <type arch="${arch}" machine="q35">hvm</type>
    ${loader}
${BOOT_ORDER_XML}
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
    ${HOTPLUG_PORTS}
  </devices>
</domain>`;
}

/// A new guest's boot order: hard disk, then CD/DVD.
///
/// Written as real `<boot>` elements rather than left to libvirt's own
/// defaults. Without them libvirt picks its own order, which is usually the same
/// but is neither guaranteed nor visible in the definition -- so the Boot Order
/// panel showed an order the guest did not actually have, and a save would
/// have written one that differed from what had been booting.
const BOOT_ORDER_XML = `    <boot dev="hd"/>
    <boot dev="cdrom"/>`;

/// PCIe root ports, so a running guest can take hot-plugged devices.///
/// On q35 every PCIe device sits on a root port of its own, and libvirt only
/// creates as many as the devices defined at boot need -- so a fresh guest had
/// no free slot and hot-adding a NIC failed with "No more available PCI slots"
/// (measured). Declared ports are used by the boot devices first, so a handful
/// is not enough: with four, the boot devices took three and one hot-add fit.
/// Fourteen is what virt-manager defines, and leaves spares after a typical
/// guest's disks, NIC, video, balloon and RNG. A controller with no index is
/// read as index 0, which q35 reserves for pcie-root, so the root is declared
/// first and the ports numbered after it.
export const HOTPLUG_PORT_COUNT = 14;
export const HOTPLUG_PORTS = [
  `<controller type="pci" index="0" model="pcie-root"/>`,
  ...Array.from({ length: HOTPLUG_PORT_COUNT }, (_, i) =>
    `<controller type="pci" index="${i + 1}" model="pcie-root-port"/>`),
].join("\n    ");

export interface DhcpHost {
  mac: string;
  ip: string;
  name?: string;
}

export interface NetworkForm {
  /// What the user calls it; the `<title>` when the network is named by UUID.
  name: string;
  forward: "nat" | "bridge" | "none";
  bridge: string;
  ip: string; // e.g. 192.168.100.1/24, or empty
  dhcp_start: string;
  dhcp_end: string;
  dns: string; // comma/space separated forwarders, NAT only
  hosts: DhcpHost[]; // static DHCP allocations, MAC -> IP
}

/** Optional uuid from an existing definition, preserved so editing/renaming
 *  updates the network in place instead of defining a duplicate. */
export function networkXml(f: NetworkForm, uuid = "", libvirtName = ""): string {
  // A network is named by its UUID (a new one: `libvirtName` = its UUID) with
  // the display name as the title. An existing network keeps whatever libvirt
  // name it has -- renaming one restarts it -- so the title is what changes.
  const name = esc(libvirtName || f.name);
  const id = uuid ? `<uuid>${esc(uuid)}</uuid>` : "";
  const title = libvirtName && libvirtName !== f.name ? `<title>${esc(f.name)}</title>` : "";
  let forward = "";
  if (f.forward === "nat") {
    forward = `<forward mode="nat"><nat><port start="1024" end="65535"/></nat></forward>`;
  } else if (f.forward === "bridge") {
    forward = `<forward mode="bridge"/>`;
  }
  // A bridge-mode network attaches to a host bridge device; a NAT or isolated
  // network owns one libvirt created (virbr1…). Its name must be kept too:
  // dropping it let libvirt pick a new bridge on every save, which is a change
  // to the running network, so saving the editor restarted the network (and
  // could rename its bridge under the firewall rules).
  const bridge = f.bridge ? `<bridge name="${esc(f.bridge)}"/>` : "";
  // Custom DNS forwarders (NAT only). libvirt wants <dns> before <ip>.
  let dns = "";
  if (f.forward === "nat" && f.dns.trim()) {
    const servers = f.dns.split(/[,\s]+/).filter(Boolean);
    if (servers.length) {
      dns = `<dns>\n    ${servers.map((s) => `<forwarder addr="${esc(s)}"/>`).join("\n    ")}\n  </dns>`;
    }
  }
  // NAT and isolated networks may carry their own subnet; a bridge network
  // takes its addressing from the bridge and must not define <ip>.
  let ip = "";
  if (f.forward !== "bridge" && f.ip) {
    const address = f.ip.split("/")[0];
    // A <dhcp> element carries both the dynamic range (when set) and any
    // static host allocations.
    const hostTags = (f.hosts ?? [])
      .filter((h) => h.mac && h.ip)
      .map((h) => `<host mac="${esc(h.mac)}" ip="${esc(h.ip)}"${h.name ? ` name="${esc(h.name)}"` : ""}/>`)
      .join("");
    const hasDhcp = (f.dhcp_start && f.dhcp_end) || hostTags;
    const dhcp = hasDhcp
      ? `<dhcp>${f.dhcp_start && f.dhcp_end ? `<range start="${esc(f.dhcp_start)}" end="${esc(f.dhcp_end)}"/>` : ""}${hostTags}</dhcp>`
      : "";
    ip = `<ip address="${esc(address)}" netmask="${esc(netmask(f.ip))}">${dhcp}</ip>`;
  }
  return `<network>
  <name>${name}</name>
  ${id}
  ${title}
  ${forward}
  ${bridge}
  ${dns}
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

/// Read an XML attribute's value from a tag fragment, single- or double-quoted.
function attr(tag: string, name: string): string {
  let from = 0;
  while (true) {
    const i = tag.indexOf(name, from);
    if (i < 0) return "";
    const after = tag.slice(i + name.length).trimStart();
    if (!after.startsWith("=")) {
      from = i + name.length;
      continue;
    }
    const rest = after.slice(1).trimStart();
    const q = rest[0];
    if (q !== '"' && q !== "'") {
      from = i + name.length;
      continue;
    }
    const inner = rest.slice(1);
    const j = inner.indexOf(q);
    return j < 0 ? "" : inner.slice(0, j);
  }
}

/// The element from `<name` up to its closing `>` (or the whole remainder).
function elem(xml: string, name: string): string {
  const i = xml.indexOf(`<${name}`);
  if (i < 0) return "";
  const j = xml.indexOf(">", i);
  return j < 0 ? xml.slice(i) : xml.slice(i, j + 1);
}

function allElems(xml: string, name: string): string[] {
  const out: string[] = [];
  let from = 0;
  while (true) {
    const i = xml.indexOf(`<${name}`, from);
    if (i < 0) break;
    const j = xml.indexOf(">", i);
    out.push(j < 0 ? xml.slice(i) : xml.slice(i, j + 1));
    from = i + name.length + 1;
  }
  return out;
}

export function networkFormFromXml(xml: string): NetworkForm {
  const forwardElem = elem(xml, "forward");
  const forward = (attr(forwardElem, "mode") || "none") as NetworkForm["forward"];
  const ipElem = elem(xml, "ip");
  const address = attr(ipElem, "address");
  const netmask = attr(ipElem, "netmask");
  const prefix = attr(ipElem, "prefix");
  // The <ip> tag carries only the address; rebuild the CIDR from its netmask
  // or prefix so the field round-trips through the same netmask() path.
  let cidr = address;
  if (address) {
    if (prefix) cidr = `${address}/${prefix}`;
    else if (netmask) cidr = `${address}/${netmaskBits(netmask)}`;
  }
  const range = elem(xml, "range");
  const dns = allElems(xml, "forwarder")
    .map((f) => attr(f, "addr"))
    .filter(Boolean)
    .join(", ");
  const hosts = allElems(xml, "host")
    .map((h) => ({
      mac: attr(h, "mac"),
      ip: attr(h, "ip"),
      name: attr(h, "name"),
    }))
    .filter((h) => h.mac && h.ip);
  return {
    // The display name: the title when there is one, since a network named
    // by its UUID keeps what people call it there.
    name: tag(xml, "title") || tag(xml, "name"),
    forward,
    bridge: attr(elem(xml, "bridge"), "name"),
    ip: cidr,
    dhcp_start: attr(range, "start"),
    dhcp_end: attr(range, "end"),
    dns,
    hosts,
  };
}

export function networkUuid(xml: string): string {
  return tag(xml, "uuid");
}

// ---------------------------------------------------------- IPv4 helpers

function ipv4(s: string): number | null {
  const parts = s.split(".");
  if (parts.length !== 4) return null;
  let n = 0;
  for (const p of parts) {
    const v = parseInt(p, 10);
    if (!Number.isFinite(v) || v < 0 || v > 255) return null;
    n = (n << 8) | v;
  }
  return n >>> 0;
}

function ipv4str(n: number): string {
  return [(n >>> 24) & 0xff, (n >>> 16) & 0xff, (n >>> 8) & 0xff, n & 0xff].join(".");
}

/// libvirt's default DHCP range for a CIDR when no explicit <range> is set:
/// gateway + 1 through broadcast - 1 (e.g. 192.168.167.2 – 192.168.167.254).
/// DHCP is on by default whenever a network has an <ip>, so an absent range
/// is not an empty pool — the page should show this derived range instead.
export function defaultDhcpRange(ipCidr: string): { start: string; end: string } | null {
  const [addr, bits] = ipCidr.split("/");
  const ip = ipv4(addr);
  const prefix = parseInt(bits ?? "", 10);
  if (ip == null || !Number.isFinite(prefix) || prefix < 1 || prefix > 30) return null;
  const mask = (0xffffffff << (32 - prefix)) >>> 0;
  const network = (ip & mask) >>> 0;
  const broadcast = (network | (~mask >>> 0)) >>> 0;
  const start = network + 2; // network address +1 is the gateway
  const end = broadcast - 1;
  if (start > end) return null;
  return { start: ipv4str(start), end: ipv4str(end) };
}

/// First unallocated address in a network's DHCP range (explicit, else the
/// derived default), skipping the gateway and any IPs already handed out.
/// Returns null when the subnet or range is missing/exhausted.
/// The addresses already spoken for in a network: every static allocation,
/// plus the gateway itself (dnsmasq never hands that out). Shared by the
/// next-free lookup and the remaining-count so the two can never disagree.
function usedAddresses(ipCidr: string, used: string[]): Set<string> {
  const set = new Set<string>();
  for (const entry of used) {
    const trimmed = entry?.trim();
    if (trimmed) set.add(trimmed);
  }
  const gw = ipCidr.split("/")[0];
  if (gw) set.add(gw);
  return set;
}

export function nextFreeIp(
  ipCidr: string,
  used: string[],
  range?: { start: string; end: string },
): string | null {
  const r = range ?? defaultDhcpRange(ipCidr);
  if (!r) return null;
  const start = ipv4(r.start);
  const end = ipv4(r.end);
  if (start == null || end == null || start > end) return null;
  const taken = usedAddresses(ipCidr, used);
  for (let n = start; n <= end; n++) {
    const s = ipv4str(n);
    if (!taken.has(s)) return s;
  }
  return null;
}

/// How much of the DHCP pool is left: addresses in the range that are neither
/// the gateway nor already handed out as static allocations. `null` when the
/// network has no pool to speak of (bridge mode, or no subnet).
export function dhcpRemaining(
  ipCidr: string,
  used: string[],
  range?: { start: string; end: string },
): { free: number; total: number } | null {
  const r = range ?? defaultDhcpRange(ipCidr);
  if (!r) return null;
  const start = ipv4(r.start);
  const end = ipv4(r.end);
  if (start == null || end == null || start > end) return null;
  const total = end - start + 1;
  const taken = usedAddresses(ipCidr, used);
  let handedOut = 0;
  for (let n = start; n <= end; n++) {
    if (taken.has(ipv4str(n))) handedOut++;
  }
  return { free: total - handedOut, total };
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

/// A pool has no `<title>` (libvirt drops one), so its display name travels
/// separately, as `?title=` on create; `uuid` becomes its libvirt name.
export function poolXml(f: PoolForm, uuid = ""): string {
  const name = esc(uuid || f.name);
  const id = uuid ? `\n  <uuid>${esc(uuid)}</uuid>` : "";
  const source = f.source
    ? `<source><device path="${esc(f.source)}"/><format type="${esc(f.source_format || "auto")}"/></source>`
    : "";
  const target = `<target><path>${esc(f.target || "")}</path></target>`;
  return `<pool type="${f.type}">
  <name>${name}</name>${id}
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
  // Whole bytes. `<capacity>` is parsed as u64 by the web server, and a
  // fractional GiB input multiplies out to a decimal (1.1 GiB -> 1181116006.4),
  // which was refused with "<capacity> must be a number".
  const cap = Math.max(0, Math.round(f.capacity_gb * 1024 * 1024 * 1024));
  return `<volume>
  <name>${esc(f.name)}</name>
  <capacity>${cap}</capacity>
  <target><format type="${f.format}"/></target>
</volume>`;
}

/// Volume name to use for a picked upload file. Browsers supply the leaf name
/// only, and libvirt accepts spaces in a volume name, so it is taken verbatim
/// rather than mangled.
export function volumeNameFromFile(fileName: string): string {
  return fileName.trim();
}

/// The name an upload field should show after a file was picked, or `null` to
/// leave the field alone.
///
/// A blank field, or one still holding the previous auto-filled name, follows
/// the file; anything else is a name the user typed, and re-picking a file
/// must not discard it.
export function autoVolumeName(
  current: string,
  previousAutoName: string,
  fileName: string,
): string | null {
  if (current.trim() !== "" && current !== previousAutoName) return null;
  return volumeNameFromFile(fileName);
}

/// A fresh random UUID (v4), for a new guest, network or pool: its libvirt
/// name and its identity, so display names need not be unique.
export function newUuid(): string {
  return crypto.randomUUID();
}
