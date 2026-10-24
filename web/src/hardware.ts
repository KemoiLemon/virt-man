// Proxmox-style hardware editor, ported from vm-man's hw-modal.
//
// A device list on the LEFT is the only navigation; selecting a device opens
// its form page on the right. Sidebar rows show the current value, so the
// whole VM config is readable at a glance. "Add Hardware" opens a chooser of
// real libvirt devices whose fields are declared in one catalogue, so the
// form and the XML builder cannot drift.

import { esc } from "./xml";
import { ic } from "./icons";
import { STORAGE_DEFAULT, unitValue } from "./units";
import type { StorageUnit } from "./units";

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
  /** Only show this field when another field equals this value. */
  showWhen?: { key: string; value: string };
  /**
   * Hide the field when its own value is blank. Used by a derived field (a
   * disk's path), so it appears for a device that has one and leaves no empty
   * row for a device that does not.
   */
  hideWhenBlank?: boolean;
  /**
   * An optional, rarely-set field. It is hidden until the user asks to see
   * advanced options (a per-device toggle), so a form opens with its essentials
   * rather than every option the device could take at once. A field that
   * already carries a value is always shown, so a definition read back from
   * libvirt is never silently hiding something it sets.
   */
  advanced?: boolean;
  /**
   * Render the value but do not accept edits. For fields the form derives
   * rather than the user setting directly; the value still round-trips.
   */
  readonly?: boolean;
}

export interface HwType {
  id: string;
  label: string;
  blurb: string;
  icon: keyof typeof ic;
  fields: HwField[];
  /** At most one instance per VM (libvirt rejects duplicates). */
  single?: boolean;
}

/** Devices the "Add Hardware" chooser offers, with their libvirt mapping. */
export const HW_CATALOG: HwType[] = [
  {
    id: "disk",
    label: "Hard Disk",
    blurb: "Attach an existing volume, or create a new one with a size",
    icon: "disk",
    fields: [
      {
        // UI only: a volume's title is the nickname, and it lives outside the
        // domain XML. The editor fills the placeholder with the drive's own
        // name and saves the value through the volume title API.
        key: "title", label: "Nickname", type: "text",
      },
      {
        key: "mode", label: "Source", type: "select", value: "existing",
        options: [
          { value: "existing", label: "Existing Volume" },
          { value: "new", label: "New Volume" },
        ],
      },
      { key: "pool", label: "Storage Pool", type: "select", options: [] },
      { key: "volume", label: "Volume", type: "select", options: [], showWhen: { key: "mode", value: "existing" } },
      // The number is in GiB; the field's unit selector decides how it is
      // written. One MiB is the floor, so the storage units below can express
      // sizes smaller than a gigabyte.
      { key: "size_gb", label: "Disk Size", type: "number", min: 1 / 1024, value: "10", placeholder: "10", showWhen: { key: "mode", value: "new" } },
      {
        // Applies to both forms: `deviceXml` writes it as the driver type for a
        // path-backed disk too, so hiding it here (as this did) meant a value
        // that was written but could not be seen or corrected. Guessing it wrong
        // makes qemu parse the file as the wrong image type.
        key: "format", label: "Format", type: "select", value: "qcow2",
        options: [
          { value: "qcow2", label: "qcow2" },
          { value: "raw", label: "raw" },
        ],
      },
      {
        key: "bus", label: "Bus", type: "select", value: "virtio",
        options: [
          { value: "virtio", label: "virtio" },
          { value: "sata", label: "SATA" },
          { value: "scsi", label: "SCSI" },
        ],
      },
      {
        // Shown only when the disk has one. Guests we wrote address their disk
        // by absolute path, and that path is the thing actually attached -- so
        // hiding it left the page describing the disk as an unnamed volume with
        // no visible source at all. Read-only, because it is derived: editing it
        // here would fight `resolveDevicePath` and the volume dropdowns.
        key: "path", label: "Path", type: "text", readonly: true,
        hideWhenBlank: true,
      },
    ],
  },
  {
    id: "cdrom",
    label: "CD/DVD Drive",
    blurb: "Attach an ISO from a storage pool",
    icon: "disk",
    fields: [
      { key: "pool", label: "ISO Pool", type: "select", options: [] },
      { key: "volume", label: "ISO Volume", type: "select", options: [] },
      {
        // A cdrom the SPA wrote is file-backed too, so it needs the same
        // treatment as the disk above.
        key: "path", label: "Path", type: "text", readonly: true,
        hideWhenBlank: true,
      },
    ],
  },
  {
    id: "interface",
    label: "Network Device",
    blurb: "Attach a virtual NIC to a network",
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
      { key: "mac", label: "MAC Address (optional)", type: "text", placeholder: "52:54:00:aa:bb:cc" },
    ],
  },
  {
    id: "video",
    label: "Video",
    blurb: "The guest's display adapter: model, video RAM and acceleration",
    icon: "vm",
    single: true,
    fields: [
      {
        // Only the models people actually choose, so no field promises a
        // backend this host might refuse. `none` is here because it is the
        // right answer for a headless guest, and `bochs`/`ramfb` are the two
        // small framebuffers worth offering (ramfb is what drives a display
        // before the OS loads a driver).
        key: "model", label: "Model", type: "select", value: "virtio",
        options: [
          { value: "virtio", label: "virtio" },
          { value: "qxl", label: "qxl" },
          { value: "vga", label: "vga" },
          { value: "cirrus", label: "cirrus" },
          { value: "vmvga", label: "vmvga" },
          { value: "bochs", label: "bochs" },
          { value: "ramfb", label: "ramfb" },
          { value: "none", label: "none" },
        ],
      },
      {
        // The concrete qemu device behind `virtio`. `virtio-vga-gl` is what
        // turns on the 3D path; `virtio-gpu` has no VGA compatibility, so it is
        // right for a guest whose display never goes through VGA. The
        // vhost-user devices need a chardev this form does not define, so they
        // stay administrators' to ask for through the XML tab.
        key: "device", label: "Virtio Device", type: "select", value: "virtio-vga",
        options: [
          { value: "virtio-vga", label: "virtio-vga" },
          { value: "virtio-vga-gl", label: "virtio-vga-gl (3D)" },
          { value: "virtio-gpu", label: "virtio-gpu" },
          { value: "virtio-gpu-gl", label: "virtio-gpu-gl (3D)" },
        ],
        showWhen: { key: "model", value: "virtio" },
      },
      { key: "vram", label: "VRAM (KiB)", type: "text", placeholder: "libvirt default", showWhen: { key: "model", value: "virtio" }, advanced: true },
      { key: "ram", label: "RAM (KiB)", type: "text", placeholder: "65536", showWhen: { key: "model", value: "qxl" }, advanced: true },
      { key: "vgamem", label: "VGA Memory (KiB)", type: "text", placeholder: "16384", showWhen: { key: "model", value: "qxl" }, advanced: true },
      { key: "vram64", label: "VRAM64 (KiB)", type: "text", placeholder: "libvirt default", showWhen: { key: "model", value: "qxl" }, advanced: true },
      { key: "heads", label: "Heads", type: "number", min: 1, max: 16, placeholder: "1", showWhen: { key: "model", value: "qxl" }, advanced: true },
      {
        // Turns on the 3D path. libvirt pairs it with a `*-gl` device (the form
        // keeps the two in step), and a guest that actually renders in 3D needs
        // a GL-capable display as well. The absolute path that would pin a
        // render node stays an administrator's to set through the XML tab.
        key: "accel3d", label: "3D Acceleration", type: "select", value: "",
        options: [
          { value: "", label: "Default" },
          { value: "yes", label: "on" },
          { value: "no", label: "off" },
        ],
        showWhen: { key: "model", value: "virtio" },
      },
      { key: "blob", label: "Blob Resources", type: "select", value: "", options: [{ value: "", label: "Default" }, { value: "on", label: "on" }, { value: "off", label: "off" }], showWhen: { key: "model", value: "virtio" }, advanced: true },
      { key: "edid", label: "EDID", type: "select", value: "", options: [{ value: "", label: "Default" }, { value: "on", label: "on" }, { value: "off", label: "off" }], advanced: true },
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
    single: true,
    fields: [
      { key: "backend", label: "Backend", type: "text", value: "/dev/urandom" },
    ],
  },
  {
    id: "memballoon",
    label: "Memory Balloon",
    blurb: "virtio balloon so the host can reclaim guest memory",
    icon: "memory",
    single: true,
    fields: [],
  },
  {
    id: "channel",
    label: "Guest Agent Channel",
    blurb: "virtio-serial channel for the QEMU guest agent",
    icon: "cpu",
    single: true,
    fields: [],
  },
  {
    id: "tpm",
    label: "TPM",
    blurb: "Emulated Trusted Platform Module (tpm-crb)",
    icon: "lock",
    single: true,
    fields: [
      {
        key: "version", label: "Version", type: "select", value: "2.0",
        options: [
          { value: "2.0", label: "2.0" },
          { value: "1.2", label: "1.2" },
        ],
      },
    ],
  },
  {
    id: "watchdog",
    label: "Watchdog",
    blurb: "i6300esb watchdog — fires when the guest hangs",
    icon: "firewall",
    single: true,
    fields: [
      {
        key: "action", label: "Action", type: "select", value: "reset",
        options: [
          { value: "reset", label: "reset" },
          { value: "poweroff", label: "poweroff" },
          { value: "shutdown", label: "shutdown" },
          { value: "pause", label: "pause" },
          { value: "none", label: "none" },
        ],
      },
    ],
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

/// The `<video>` element for a device, with only the attributes the chosen
/// model actually accepts.
///
/// libvirt's schema ties each attribute to its model: `ram`, `vgamem`, `vram64`
/// and `heads` are the QXL group; `device`, `blob` and `accel3d` are virtio's.
/// Writing a QXL attribute on a virtio model is refused by libvirt, which is why
/// the form hides fields rather than trusting the user to leave them blank.
/// `model='none'` takes nothing at all.
export function videoXml(d: HwDevice): string {
  const f = (k: string) => (d.fields[k] ?? "").trim();
  const model = f("model") || "virtio";
  if (model === "none") return `<video><model type="none"/></video>`;
  // A numeric attribute only goes in when it is a plain number: a stray value
  // would otherwise reach libvirt as-is and fail the define.
  const num = (k: string) => (/^\d+$/.test(f(k)) ? ` ${k}="${esc(f(k))}"` : "");
  const attrs: string[] = [];
  if (model === "qxl") {
    // `ram`, `vgamem` and `vram64` are QXL-only; `heads` is in the shared
    // optional group below, so it is not repeated here.
    for (const k of ["ram", "vgamem", "vram64"]) attrs.push(num(k));
  }
  if (model === "virtio") {
    // `device` names the qemu device; the default (`virtio-vga`) is what
    // libvirt picks anyway, so it is only written when it differs.
    const dev = f("device") || "virtio-vga";
    if (dev !== "virtio-vga") attrs.push(` device="${esc(dev)}"`);
    if (f("vram")) attrs.push(num("vram"));
    for (const k of ["blob", "edid"]) {
      if (f(k)) attrs.push(` ${k}="${esc(f(k))}"`);
    }
  } else {
    if (f("edid")) attrs.push(` edid="${esc(f("edid"))}"`);
  }
  // `primary` is libvirt's annotation; carry it through when the definition
  // already had it. `yes`/`no` only: anything else is left out.
  if (f("primary") === "yes" || f("primary") === "no") {
    attrs.push(` primary="${f("primary")}"`);
  }
  // `heads` and `vram` are in libvirt's shared optional group, so any model
  // that is not `virtio` (which already wrote `vram` above) may carry them.
  if (model !== "virtio") {
    for (const k of ["heads", "vram"]) attrs.push(num(k));
  } else {
    attrs.push(num("heads"));
  }
  // `acceleration` is a child of `model`, not a sibling: libvirt reads the 3D
  // flag from inside the model element, and a sibling `<acceleration>` is
  // accepted by the schema yet ignored by the parser -- so a `-gl` device came
  // back as "the 'virtio-vga-gl' frontend can be only with enabled 3d
  // acceleration" even though the XML named one.
  const accel = model === "virtio" && f("accel3d") === "yes"
    ? `<acceleration accel3d="yes"/>`
    : "";
  // Self-closing when there is no acceleration child: an empty paired tag is
  // valid but noisier, and libvirt writes the self-closing form itself.
  const modelTag = accel
    ? `<model type="${esc(model)}"${attrs.join("")}>\n        ${accel}\n      </model>`
    : `<model type="${esc(model)}"${attrs.join("")}/>`;
  return `<video>
      ${modelTag}
      <address type="pci"/>
    </video>`;
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
      const target = `<target dev="${diskTarget(bus, index)}" bus="${esc(bus)}"/>`;
      // A disk whose path we know is written as file-backed. libvirt 11.3
      // cannot translate a pool+volume source into a path, and a disk it
      // cannot resolve never reaches the guest's AppArmor profile — so qemu is
      // refused permission to open it and the guest will not start at all.
      // An absolute path is resolved the same way by every libvirt.
      const path = f("path");
      if (path) {
        return `<disk type="file" device="disk">
      <driver name="qemu" type="${f("format") === "raw" ? "raw" : "qcow2"}"/>
      <source file="${esc(path)}"/>
      ${target}
    </disk>`;
      }
      // No path known (a definition we did not write): keep the reference form
      // rather than guessing a path for someone else's volume.
      const newMode = (d.fields.mode ?? "existing") === "new";
      const unit = (f("size_unit") || STORAGE_DEFAULT) as StorageUnit;
      const size = unitValue(Number(f("size_gb") || "10"), "GiB", unit);
      const vol = newMode ? `(new ${size} ${unit} ${f("format") || "qcow2"})` : f("volume");
      return `<disk type="volume" device="disk">
      <driver name="qemu" type="qcow2"/>
      <source pool="${esc(f("pool"))}" volume="${esc(vol)}"/>
      ${target}
    </disk>`;
    }
    case "cdrom": {
      // No media = an empty drive (a tray with no disc), which libvirt models
      // as a file-backed cdrom with no <source>. Keep it valid so a default
      // drive can be added before the user picks an ISO.
      const path = f("path");
      const vol = f("volume");
      const src = path
        ? `<source file="${esc(path)}"/>`
        : vol ? `<source pool="${esc(f("pool"))}" volume="${esc(vol)}"/>` : "";
      const kind = !path && vol ? "volume" : "file";
      return `<disk type="${kind}" device="cdrom">
      <driver name="qemu" type="raw"/>
      ${src}${src ? "\n      " : ""}<target dev="sd${String.fromCharCode(96 + index)}" bus="sata"/>
      <readonly/>
    </disk>`;
    }
    case "interface": {
      const mac = f("mac");
      // Rate limits are set from the VM's Network tab, not this form. Carry
      // them through verbatim, or saving hardware would silently lift them.
      const bw = d.fields.bandwidth ?? "";
      // A guest attaches to the network's bridge: these networks are not
      // defined in libvirt. A NIC with no bridge is the "attach to nothing"
      // NIC, which keeps libvirt's empty-network source.
      const bridge = f("bridge");
      const attach = bridge ? `bridge="${esc(bridge)}"` : `network="${esc(f("network"))}"`;
      return `<interface type="${bridge ? "bridge" : "network"}">
      <source ${attach}/>
      <model type="${esc(f("model") || "virtio")}"/>${mac ? `\n      <mac address="${esc(mac)}"/>` : ""}${bw ? `\n      ${bw}` : ""}
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
    case "video":
      return videoXml(d);
    case "memballoon":
      return `<memballoon model="${esc(f("model") || "virtio")}"/>`;
    case "channel":
      return `<channel type="unix">
      <target type="virtio" name="org.qemu.guest_agent.0"/>
    </channel>`;
    case "tpm":
      return `<tpm model="tpm-crb">
      <backend type="emulator" version="${esc(f("version") || "2.0")}"/>
    </tpm>`;
    case "watchdog":
      // Preserve the existing model on a round trip; only newly added
      // watchdogs default to i6300esb.
      return `<watchdog model="${esc(f("model") || "i6300esb")}" action="${esc(f("action") || "reset")}"/>`;
    default:
      return "";
  }
}

/** Human-readable one-liner for a device, shown in sidebar/table rows. */
export function deviceSummary(d: HwDevice, resolve?: (kind: "pool" | "network", name: string) => string): string {
  const f = (k: string) => (d.fields[k] ?? "").trim();
  // Pools and networks are named by their UUID and carry their nickname as a
  // title, so showing the raw name shows a UUID for anything made or migrated
  // since UUIDs. The *value* stays the libvirt name -- this is display only.
  const named = (kind: "pool" | "network", name: string) => resolve?.(kind, name) || name;
  switch (d.kind) {
    case "disk": {
      // A nickname wins over the file name, which is what it exists for: the
      // name on disk is generated and unreadable, the nickname is the label
      // the user chose.
      const title = f("title");
      const path = f("path");
      // Same shape as the Create VM row: size, then the nickname.
      if (f("cap_gib") && (title || path)) {
        const unit = (f("size_unit") || STORAGE_DEFAULT) as StorageUnit;
        return `${unitValue(Number(f("cap_gib")), "GiB", unit)} ${unit} · ${title || path.split("/").pop()}`;
      }
      if (title) return `${title} (${f("bus") || "virtio"})`;
      if (path) return `${path.split("/").pop()} (${f("bus") || "virtio"})`;
      if ((d.fields.mode ?? "existing") === "new") {
        const unit = (f("size_unit") || STORAGE_DEFAULT) as StorageUnit;
        const size = unitValue(Number(f("size_gb") || "10"), "GiB", unit);
        return `${named("pool", f("pool"))}/new ${size} ${unit} ${f("format") || "qcow2"}`;
      }
      return `${named("pool", f("pool"))}/${f("volume")} (${f("bus") || "virtio"})`;
    }
    case "cdrom": {
      const path = f("path");
      if (path) return path.split("/").pop() ?? path;
      const vol = f("volume");
      return vol ? `${named("pool", f("pool"))}/${vol}` : "empty";
    }
    case "interface": {
      // A NIC read from XML names the bridge until the editor maps it to the
      // network, so the summary must not claim it has no network.
      const on = f("network")
        ? named("network", f("network"))
        : f("bridge") ? `bridge ${f("bridge")}` : "No network";
      return `${on} (${f("model") || "virtio"})`;
    }
    case "hostdev": return `${f("domain")}:${f("bus")}:${f("slot")}.${f("function")}`;
    case "usb": return `${f("vendor")}:${f("product")}`;
    case "rng": return f("backend") || "/dev/urandom";
    case "memballoon": return f("model") || "virtio";
    case "channel": return "org.qemu.guest_agent.0";
    case "tpm": return `emulated ${f("version") || "2.0"}`;
    case "watchdog": return `${f("model") || "i6300esb"} (${f("action") || "reset"})`;
    case "video": {
      const model = f("model") || "virtio";
      // The virtio device is the part people set, so name it when it is not the
      // plain default; the 3D flag rides alongside because it changes what the
      // guest can do and is otherwise invisible in the list.
      if (model === "virtio") {
        const dev = f("device") || "virtio-vga";
        return f("accel3d") === "yes" ? `${dev} + 3D` : dev;
      }
      return model;
    }
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
    case "disk": {
      // A disk addressed by path needs nothing else: it is already complete.
      if ((d.fields.path ?? "").trim()) return null;
      if ((d.fields.mode ?? "existing") === "new") {
        return need(["pool", "size_gb"], "A new disk");
      }
      return need(["pool", "volume"], "A hard disk");
    }
    case "cdrom":
      // An empty drive is fine, and so is media addressed by path.
      if ((d.fields.path ?? "").trim()) return null;
      if ((d.fields.volume ?? "").trim() && !(d.fields.pool ?? "").trim()) {
        return "A CD/DVD drive requires a pool";
      }
      return null;
    // A NIC may name no network: libvirt requires a <source>, but accepts an
    // empty one, so `<source network=""/>` is a NIC that attaches to nothing
    // until a network is chosen. xmlcheck allows exactly that (and no foreign
    // network), so this stays a non-admin's choice.
    case "interface": return null;
    case "hostdev": return need(["bus", "slot", "function"], "PCI passthrough");
    case "usb": return need(["vendor", "product"], "USB passthrough");
    case "video": {
      // The numeric attributes reach libvirt as written, so a non-numeric value
      // must be caught here rather than failing the define after the round trip.
      for (const k of ["vram", "ram", "vgamem", "vram64", "heads"]) {
        const v = (d.fields[k] ?? "").trim();
        if (v && !/^\d+$/.test(v)) return `Video ${k} must be a number`;
      }
      return null;
    }
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

/// Keep only well-formed `<inbound|outbound average=N .../>` rate entries from
/// a `<bandwidth>` block. The block comes from libvirt, but it is re-emitted
/// into XML we define, so it is rebuilt from numbers rather than copied.
function sanitizeBandwidth(block: string): string {
  if (!block) return "";
  const parts: string[] = [];
  for (const dir of ["inbound", "outbound"]) {
    const tag = block.match(new RegExp(`<${dir}\\b[^>]*>`))?.[0];
    if (!tag) continue;
    const nums = [...tag.matchAll(/\b(average|peak|burst|floor)=['"](\d+)['"]/g)]
      .map((m) => `${m[1]}="${m[2]}"`);
    if (nums.length) parts.push(`<${dir} ${nums.join(" ")}/>`);
  }
  return parts.length ? `<bandwidth>${parts.join("")}</bandwidth>` : "";
}

/** What each `<boot dev=...>` actually points at, in the same words
 * `deviceSummary` uses. libvirt picks a boot device by *kind*, not by name, so
 * `hd` means "the first disk, in declaration order" and `cdrom` the first CD:
 * labelling the kind alone leaves the user unable to tell which of several
 * disks they just reordered. `fd` names no backing device -- there is no way to
 * attach a floppy in this UI. */
export function bootTargets(xml: string): Record<string, string> {
  const name = (block: string) => {
    const src = block.match(/<source\b[^>]*>/)?.[0] ?? "";
    const at = (n: string) => src.match(new RegExp(`${n}\\s*=\\s*['"]([^'"]*)['"]`))?.[1] ?? "";
    const file = at("file");
    return file ? (file.split("/").pop() ?? file) : at("volume") || at("pool");
  };
  const blocks = [...xml.matchAll(/<disk\b[^>]*device=['"](disk|cdrom)['"][\s\S]*?<\/disk>/g)];
  const out: Record<string, string> = {};
  for (const kind of ["disk", "cdrom"] as const) {
    const names = blocks.filter((m) => m[1] === kind).map((m) => {
      if (kind === "disk") {
        const target = m[0].match(/<target\b[^>]*dev=['"]([^'"]+)['"]/);
        if (target) {
          out.hdVolume = name(m[0]);
          return target[1];
        }
      }
      return name(m[0]);
    }).filter(Boolean);
    // More than one disk: it is the first that boots, so it is the one named.
    if (names.length) out[kind === "disk" ? "hd" : "cdrom"] = names[0];
  }
  // A network boot tries the first NIC, so it is described the way the Network
  // tab describes one: which network it is on, and its MAC when the XML carries
  // it (libvirt only writes one once the guest has been started, or if it was
  // set explicitly). A guest attached by bridge names the bridge; the caller
  // shows the network's nickname for it with whatever resolver it already has.
  const nic = xml.match(/<interface\b[^>]*>[\s\S]*?<\/interface>/)?.[0];
  if (nic) {
    const src = nic.match(/<source\b[^>]*>/)?.[0] ?? "";
    const named = (n: string) => src.match(new RegExp(`${n}\\s*=\\s*['"]([^'"]*)['"]`))?.[1] ?? "";
    const net = named("network") || named("bridge");
    const mac = nic.match(/<mac\b[^>]*address=['"]([^'"]*)['"]/)?.[1] ?? "";
    if (net || mac) out.network = [net, mac].filter(Boolean).join(" ");
    // The bare name, alongside the formatted label above, so a caller can swap
    // it for the network's nickname without parsing the label apart. A libvirt
    // name cannot contain a space today, but splitting on one would break
    // silently if that ever changed.
    if (net) out.networkName = net;
  }
  // No `fd`: the editor cannot attach a floppy, so naming a device for it would
  // promise something the guest cannot have. See `bootChoices`.
  return out;
}

/// Boot devices a guest may try. The order they are tried in is the document
/// order of the `<os>` `<boot dev=.../>` elements, which is how libvirt reads
/// them.
///
/// `fd` is deliberately absent. This UI has no way to attach a floppy -- there
/// is no floppy in `HW_CATALOG` -- so offering it was an option that could only
/// ever add a `<boot dev='fd'/>` pointing at a drive the guest does not have.
/// A guest that already lists one keeps it (the list is built from its own XML);
/// it is just never offered again.
export const BOOT_DEVS = ["hd", "cdrom", "network"] as const;
export const BOOT_LABEL: Record<string, string> = {
  fd: "Floppy", hd: "Hard Disk", cdrom: "CD/DVD", network: "Network",
};

/// What to offer in the boot-order dropdown, and what to call each entry.
///
/// Three rules, each fixing a way the old plain `BOOT_DEVS` list misled:
///
/// - The label names the *device*, not just the kind. libvirt boots "the first
///   disk", so with two attached, "Hard Disk" does not say which one; the name
///   from `bootTargets` does (`Debian-13.qcow2`), and a NIC is named by its
///   network and MAC. The kind is the fallback for a boot device with no
///   backing device, which is the only label those can have.
/// - A kind already in the order is not offered again. Adding it twice is
///   meaningless -- libvirt resolves both to the same device -- and the second
///   entry did nothing but confuse the order.
/// - A kind the guest cannot boot from is not offered at all: no network boot
///   without a NIC, and no floppy (see `BOOT_DEVS`). Offering it produces a
///   `<boot>` element libvirt accepts and then never uses.
///
/// `names` comes from the guest XML (`bootTargets`), so an entry names the disk
/// really attached.
export function bootChoices(
  order: string[],
  names: Record<string, string>,
): { value: string; label: string }[] {
  return BOOT_DEVS.flatMap((d) => {
    if (order.includes(d)) return [];
    // A boot device with no backing device is not bootable. `bootTargets`
    // reports only what it found, so absence is the test -- no NIC means no
    // `network` entry. `hd`/`cdrom` are always offered: the kinds are meaningful
    // even before a disk is attached, which is how a guest is set up to boot
    // from media added later.
    if (d !== "hd" && d !== "cdrom" && names[d] === undefined) return [];
    return [{ value: d, label: names[d] ? `${names[d]} (${BOOT_LABEL[d]})` : BOOT_LABEL[d] }];
  });
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
    const drv = block.match(/<driver\b[^>]*>/)?.[0] ?? "";
    const pool = attr(src, "pool");
    const volume = attr(src, "volume");
    const path = attr(src, "file");
    const format = attr(drv, "type");
    const bus = attr(tgt, "bus");
    const kind = m[1] === "cdrom" ? "cdrom" : "disk";
    // An empty cdrom has no <source>: keep it as an empty drive so editing
    // round-trips it instead of silently dropping it.
    if (m[1] === "cdrom" && !volume && !path) {
      out.push({ kind, fields: { pool, volume } });
      continue;
    }
    // A path-addressed disk round-trips through its path. These used to be
    // skipped here, which silently dropped them: the caller strips every
    // <disk> block and re-adds only what this function returns, so a disk
    // missing from this list disappears when hardware is saved.
    if (path) {
      out.push({ kind, fields: { path, format, bus } });
      continue;
    }
    // Anything else is a pool+volume reference: keep it as one rather than
    // inventing a path for a volume whose location we cannot confirm.
    if (!pool || !volume) continue;
    out.push({ kind, fields: { pool, volume, format, bus } });
  }
  // A NIC attaches by bridge (what this app writes) or by libvirt network (an
  // older definition). Both are kept, and the editor maps a bridge to the
  // network that owns it; an unknown operator bridge keeps its name so it
  // round-trips.
  for (const m of xml.matchAll(/<interface\b[^>]*type=['"](?:network|bridge)['"][\s\S]*?<\/interface>/g)) {
    const block = m[0];
    const src = block.match(/<source\b[^>]*>/)?.[0] ?? "";
    out.push({
      kind: "interface",
      fields: {
        network: attr(src, "network"),
        bridge: attr(src, "bridge"),
        model: attr(block.match(/<model\b[^>]*>/)?.[0] ?? "", "type"),
        mac: attr(block.match(/<mac\b[^>]*>/)?.[0] ?? "", "address"),
        bandwidth: sanitizeBandwidth(block.match(/<bandwidth\b[\s\S]*?<\/bandwidth>/)?.[0] ?? ""),
      },
    });
  }
  if (/<memballoon\b/.test(xml)) {
    const tag = xml.match(/<memballoon\b[^>]*>/)?.[0] ?? "";
    out.push({ kind: "memballoon", fields: { model: attr(tag, "model") } });
  }
  // Exactly one video, edited as a device so its options round-trip. libvirt
  // writes it with a `<model>`, an `<acceleration>` when 3D is on, and an
  // `<address>`; only the model's own attributes are read back, and the
  // `<address>` is left for `withDevices` to regenerate rather than copied.
  {
    const block = xml.match(/<video\b[\s\S]*?<\/video>/)?.[0];
    if (block) {
      const model = block.match(/<model\b[^>]*>/)?.[0] ?? "";
      const accel = block.match(/<acceleration\b[^>]*\/?>/)?.[0] ?? "";
      out.push({
        kind: "video",
        fields: {
          model: attr(model, "type"),
          device: attr(model, "device"),
          vram: attr(model, "vram"),
          ram: attr(model, "ram"),
          vgamem: attr(model, "vgamem"),
          vram64: attr(model, "vram64"),
          heads: attr(model, "heads"),
          blob: attr(model, "blob"),
          edid: attr(model, "edid"),
          // `primary` is libvirt's own annotation of which adapter drives the
          // console; it is read back so a save does not drop it and make
          // libvirt re-decide for a guest with more than one adapter.
          primary: attr(model, "primary"),
          accel3d: attr(accel, "accel3d"),
        },
      });
    }
  }
  if (/org\.qemu\.guest_agent/.test(xml)) out.push({ kind: "channel", fields: {} });
  if (/<tpm\b/.test(xml)) {
    const version = /version=["']([^"']+)/.exec(xml)?.[1] ?? "2.0";
    out.push({ kind: "tpm", fields: { version } });
  }
  for (const m of xml.matchAll(/<watchdog\b[^>]*\/?>/g)) {
    out.push({ kind: "watchdog", fields: { action: attr(m[0], "action"), model: attr(m[0], "model") } });
  }
  for (const m of xml.matchAll(/<rng\b[\s\S]*?<\/rng>/g)) {
    const backend = m[0].match(/<backend[^>]*>([^<]*)<\/backend>/)?.[1] ?? "";
    out.push({ kind: "rng", fields: { backend } });
  }
  return out;
}
