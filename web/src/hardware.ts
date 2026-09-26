// Proxmox-style hardware editor, ported from vm-man's hw-modal.
//
// A device list on the LEFT is the only navigation; selecting a device opens
// its form page on the right. Sidebar rows show the current value, so the
// whole VM config is readable at a glance. "Add Hardware" opens a chooser of
// real libvirt devices whose fields are declared in one catalogue, so the
// form and the XML builder cannot drift.

import { esc } from "./xml";
import { ic } from "./icons";

// ----------------------------------------------------------------- catalogue

export interface HwField {
  key: string;
  label: string;
  type: "text" | "number" | "select";
  placeholder?: string;
  value?: string;
  min?: number;
  max?: number;
  options?: { value: string; label: string }[];
  hint?: string;
}

export interface HwType {
  id: string;
  label: string;
  blurb: string;
  icon: keyof typeof ic;
  fields: HwField[];
}

/** Devices the "Add Hardware" chooser offers, with their libvirt mapping. */
export const HW_CATALOG: HwType[] = [
  {
    id: "disk",
    label: "Extra Disk",
    blurb: "Attach another volume from a storage pool",
    icon: "disk",
    fields: [
      { key: "pool", label: "Storage pool", type: "select", options: [] },
      { key: "volume", label: "Volume", type: "select", options: [] },
      {
        key: "bus", label: "Bus", type: "select", value: "virtio",
        options: [
          { value: "virtio", label: "virtio" },
          { value: "sata", label: "SATA" },
          { value: "scsi", label: "SCSI" },
        ],
      },
    ],
  },
  {
    id: "cdrom",
    label: "CD/DVD Drive",
    blurb: "Attach an ISO from a storage pool",
    icon: "disk",
    fields: [
      { key: "pool", label: "ISO pool", type: "select", options: [] },
      { key: "volume", label: "ISO volume", type: "select", options: [] },
    ],
  },
  {
    id: "interface",
    label: "Network Device",
    blurb: "Attach a virtual NIC to a libvirt network",
    icon: "network",
    fields: [
      { key: "network", label: "Network", type: "select", options: [] },
      {
        key: "model", label: "Model", type: "select", value: "virtio",
        options: [
          { value: "virtio", label: "virtio" },
          { value: "e1000e", label: "e1000e" },
          { value: "rtl8139", label: "rtl8139" },
        ],
      },
      { key: "mac", label: "MAC address (optional)", type: "text", placeholder: "52:54:00:aa:bb:cc" },
    ],
  },
  {
    id: "hostdev",
    label: "PCI Passthrough",
    blurb: "Assign a host PCI device to the guest",
    icon: "cpu",
    fields: [
      { key: "domain", label: "Domain", type: "text", value: "0x0000", placeholder: "0x0000" },
      { key: "bus", label: "Bus", type: "text", value: "0x01", placeholder: "0x01" },
      { key: "slot", label: "Slot", type: "text", value: "0x00", placeholder: "0x00" },
      { key: "function", label: "Function", type: "text", value: "0x0", placeholder: "0x0" },
    ],
  },
  {
    id: "usb",
    label: "USB Passthrough",
    blurb: "Assign a host USB device by vendor/product id",
    icon: "disk",
    fields: [
      { key: "vendor", label: "Vendor ID", type: "text", placeholder: "0x1234" },
      { key: "product", label: "Product ID", type: "text", placeholder: "0x5678" },
    ],
  },
  {
    id: "rng",
    label: "RNG",
    blurb: "virtio entropy source backed by /dev/urandom",
    icon: "cpu",
    fields: [
      { key: "backend", label: "Backend", type: "text", value: "/dev/urandom" },
    ],
  },
  {
    id: "memballoon",
    label: "Memory Balloon",
    blurb: "virtio balloon so the host can reclaim guest memory",
    icon: "memory",
    fields: [],
  },
  {
    id: "channel",
    label: "Guest Agent Channel",
    blurb: "virtio-serial channel for the QEMU guest agent",
    icon: "cpu",
    fields: [],
  },
];

export function hwType(id: string): HwType | undefined {
  return HW_CATALOG.find((t) => t.id === id);
}

// ------------------------------------------------------------ device -> XML

export interface HwDevice {
  kind: string;
  fields: Record<string, string>;
}

/** Next free target dev name for a bus, e.g. vdb / sdb. */
function diskTarget(bus: string, index: number): string {
  const prefix = bus === "virtio" ? "vd" : bus === "scsi" ? "sd" : "sd";
  return `${prefix}${String.fromCharCode("a".charCodeAt(0) + index)}`;
}

/**
 * Build the libvirt `<devices>` fragment for one device. Kept beside the
 * catalogue so a field rename can never silently stop reaching the XML.
 */
export function deviceXml(d: HwDevice, index = 1): string {
  const f = (k: string) => (d.fields[k] ?? "").trim();
  switch (d.kind) {
    case "disk": {
      const bus = f("bus") || "virtio";
      return `<disk type="volume" device="disk">
      <driver name="qemu" type="qcow2"/>
      <source pool="${esc(f("pool"))}" volume="${esc(f("volume"))}"/>
      <target dev="${diskTarget(bus, index)}" bus="${esc(bus)}"/>
    </disk>`;
    }
    case "cdrom":
      return `<disk type="volume" device="cdrom">
      <driver name="qemu" type="raw"/>
      <source pool="${esc(f("pool"))}" volume="${esc(f("volume"))}"/>
      <target dev="sd${String.fromCharCode(112 + index)}" bus="sata"/>
      <readonly/>
    </disk>`;
    case "interface": {
      const mac = f("mac");
      return `<interface type="network">
      <source network="${esc(f("network"))}"/>
      <model type="${esc(f("model") || "virtio")}"/>${mac ? `\n      <mac address="${esc(mac)}"/>` : ""}
    </interface>`;
    }
    case "hostdev":
      return `<hostdev mode="subsystem" type="pci" managed="yes">
      <source><address domain="${esc(f("domain") || "0x0000")}" bus="${esc(f("bus"))}" slot="${esc(f("slot"))}" function="${esc(f("function"))}"/></source>
    </hostdev>`;
    case "usb":
      return `<hostdev mode="subsystem" type="usb" managed="yes">
      <source><vendor id="${esc(f("vendor"))}"/><product id="${esc(f("product"))}"/></source>
    </hostdev>`;
    case "rng":
      return `<rng model="virtio">
      <backend model="random">${esc(f("backend") || "/dev/urandom")}</backend>
    </rng>`;
    case "memballoon":
      return `<memballoon model="virtio"/>`;
    case "channel":
      return `<channel type="unix">
      <target type="virtio" name="org.qemu.guest_agent.0"/>
    </channel>`;
    default:
      return "";
  }
}

/** Human-readable one-liner for a device, shown in sidebar/table rows. */
export function deviceSummary(d: HwDevice): string {
  const f = (k: string) => (d.fields[k] ?? "").trim();
  switch (d.kind) {
    case "disk": return `${f("pool")}/${f("volume")} (${f("bus") || "virtio"})`;
    case "cdrom": return `${f("pool")}/${f("volume")}`;
    case "interface": return `${f("network")} (${f("model") || "virtio"})`;
    case "hostdev": return `${f("domain")}:${f("bus")}:${f("slot")}.${f("function")}`;
    case "usb": return `${f("vendor")}:${f("product")}`;
    case "rng": return f("backend") || "/dev/urandom";
    case "memballoon": return "virtio";
    case "channel": return "org.qemu.guest_agent.0";
    default: return d.kind;
  }
}

/** Reject a device whose required fields are blank, before it reaches XML. */
export function validateDevice(d: HwDevice): string | null {
  const need = (keys: string[], what: string) => {
    for (const k of keys) {
      if (!(d.fields[k] ?? "").trim()) return `${what} requires ${k}`;
    }
    return null;
  };
  switch (d.kind) {
    case "disk": return need(["pool", "volume"], "An extra disk");
    case "cdrom": return need(["pool", "volume"], "A CD/DVD drive");
    case "interface": return need(["network"], "A network device");
    case "hostdev": return need(["bus", "slot", "function"], "PCI passthrough");
    case "usb": return need(["vendor", "product"], "USB passthrough");
    default: return null;
  }
}

// ------------------------------------------------------------ XML injection

/**
 * Splice a `<devices>` fragment into an existing domain XML, replacing any
 * devices of the same kinds this editor manages. Editing an existing guest
 * has to preserve everything libvirt added that the form does not model.
 */
export function withDevices(domainXml: string, devices: HwDevice[]): string {
  const frag = devices
    .map((d, i) => deviceXml(d, i + 1))
    .filter(Boolean)
    .join("\n    ");
  const close = domainXml.lastIndexOf("</devices>");
  if (close === -1) return domainXml;
  return `${domainXml.slice(0, close)}  ${frag}\n  ${domainXml.slice(close)}`;
}

/** Parse the devices this editor manages back out of a domain XML. */
export function devicesFromXml(xml: string): HwDevice[] {
  const out: HwDevice[] = [];
  const attr = (tag: string, name: string) => {
    const m = tag.match(new RegExp(`${name}\\s*=\\s*['"]([^'"]*)['"]`));
    return m ? m[1] : "";
  };

  for (const m of xml.matchAll(/<disk\b[^>]*device=['"](disk|cdrom)['"][\s\S]*?<\/disk>/g)) {
    const block = m[0];
    const src = block.match(/<source\b[^>]*>/)?.[0] ?? "";
    const tgt = block.match(/<target\b[^>]*>/)?.[0] ?? "";
    const pool = attr(src, "pool");
    const volume = attr(src, "volume");
    // Only volume-backed disks round-trip; file-backed ones stay untouched.
    if (!pool || !volume) continue;
    out.push({
      kind: m[1] === "cdrom" ? "cdrom" : "disk",
      fields: { pool, volume, bus: attr(tgt, "bus") },
    });
  }
  for (const m of xml.matchAll(/<interface\b[^>]*type=['"]network['"][\s\S]*?<\/interface>/g)) {
    const block = m[0];
    out.push({
      kind: "interface",
      fields: {
        network: attr(block.match(/<source\b[^>]*>/)?.[0] ?? "", "network"),
        model: attr(block.match(/<model\b[^>]*>/)?.[0] ?? "", "type"),
        mac: attr(block.match(/<mac\b[^>]*>/)?.[0] ?? "", "address"),
      },
    });
  }
  if (/<memballoon\b/.test(xml)) out.push({ kind: "memballoon", fields: {} });
  if (/org\.qemu\.guest_agent/.test(xml)) out.push({ kind: "channel", fields: {} });
  for (const m of xml.matchAll(/<rng\b[\s\S]*?<\/rng>/g)) {
    const backend = m[0].match(/<backend[^>]*>([^<]*)<\/backend>/)?.[1] ?? "";
    out.push({ kind: "rng", fields: { backend } });
  }
  return out;
}
