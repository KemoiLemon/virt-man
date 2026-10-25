import { api, remainingLimits } from "../api";
import type { Limit, NetworkSummary, PoolSummary, Quota, UsageCounts, VolumeSummary } from "../api";
import { assignOwner } from "../admin";
import { ic } from "../icons";
import { HW_CATALOG, deviceSummary, deviceXml, devicesFromXml, hwType, validateDevice, withDevices } from "../hardware";
import type { HwDevice, HwField } from "../hardware";
import { MEMORY_DEFAULT, MEMORY_UNITS, STORAGE_DEFAULT, STORAGE_UNITS, convert, unitOptions, unitValue } from "../units";
import type { MemoryUnit, StorageUnit } from "../units";
import { fmtBytes } from "../stats";
import { runTask } from "../task";
import { addDisplay, domainXml, esc, newUuid, volumeXml, withFirmware } from "../xml";
import type { DomainForm } from "../xml";
import { state } from "../app/state";
import { bridgeNetwork, displayName, isUuid, unescapeXml, withTitle } from "../app/util";
import { openModal, toast } from "../app/dialogs";
import { setVolumeTitle } from "../app/actions";
import { hooks } from "../app/hooks";
import { guestList } from "../app/nav";
import { $, setHtml } from "../app/dom";
import { MIN_DISK_GIB, clampGib } from "./vm-form";

async function loadFormData(conn: string) {
  const pools = await api.get<PoolSummary[]>(`/proxy/${conn}/pools`);
  const nets = await api.get<NetworkSummary[]>(`/proxy/${conn}/networks`);
  return { pools, nets };
}

/// The caller's remaining limits on one daemon, for a hard `max` on a form
/// field: what the server would accept, so a create is not refused after the
/// work. Empty for an administrator, or when the read fails — the field is
/// then unbounded and the server still refuses an over-limit value with an
/// explanatory message.
async function fieldsLeft(conn: string): Promise<Limit> {
  if (hooks.canAdmin()) return {};
  try {
    const u = await api.get<{
      quota: Quota;
      total: UsageCounts;
      daemons: (UsageCounts & { conn: string; online: boolean })[];
    }>("/me/usage");
    const own = u.daemons.find((d) => d.conn === conn) ?? u.total;
    return remainingLimits(u.quota, conn, own, u.total);
  } catch {
    return {};
  }
}

/// ` max="N"` for a field, or nothing when there is no ceiling.
function maxAttr(n: number | null | undefined): string {
  return n == null ? "" : ` max="${n}"`;
}

/// What the first disk of a new guest is called until the user picks a name.
const PRIMARY_DISK = "Primary";

/// Create a volume for every "new volume" disk in the device list, then rewrite
/// the device to reference it, so the XML builder can attach it like an
/// existing volume. Called before defining the domain in both create and edit.
async function createPendingVolumes(
  conn: string,
  vmName: string,
  devices: HwDevice[],
  set: (msg: string) => void,
  made: { pool: string; name: string }[] = [],
  nicknameBased = false,
) {
  let diskNo = 1; // the boot disk is the first
  for (let i = 0; i < devices.length; i++) {
    const d = devices[i];
    if (d.kind === "disk") diskNo++;
    if (d.kind !== "disk" || (d.fields.mode ?? "existing") !== "new") continue;
    const pool = (d.fields.pool ?? "").trim();
    const raw = d.fields.size_gb ?? "";
    const size = raw === "" ? 1 : Number(raw);
    const sizeGb = Math.max(MIN_DISK_GIB, Number.isFinite(size) ? size : 1);
    const format = (d.fields.format ?? "qcow2") === "raw" ? "raw" : "qcow2";
    const name = nicknameBased
      ? `${vmName}-disk${d.fields.new_suffix || newUuid().slice(0, 8)}.${format}`
      : `${vmName}-disk${i + 1}-${sizeGb}gb.${format}`;
    set(`creating ${unitValue(sizeGb, "GiB", "GiB")} GiB volume ${name}`);
    const vol = await api.post<{ name: string; path: string }>(
      `/proxy/${conn}/pools/${encodeURIComponent(pool)}/volumes`,
      { xml: volumeXml({ name, capacity_gb: sizeGb, format }),
    });
    made.push({ pool, name });
    d.fields.volume = name;
    // Address it by path as well: the reference form is not resolvable on
    // every libvirt, and an unresolvable disk keeps the guest from starting.
    if (vol.path) d.fields.path = vol.path;
    delete d.fields.mode;
    // Create VM names a disk the user left unnamed after the guest and its
    // place in the list; applyDiskTitles then writes it like a typed one.
    if (nicknameBased && !(d.fields.title ?? "").trim()) {
      d.fields.title = `${vmName.trim()} ${diskNo}`;
      d.fields.title_loaded = "1";
      d.fields.title_original = "";
    }
  }
}

/// Save the nicknames typed into a disk form. The nickname is the backing
/// volume's title, so it must be written after any new volume exists.
///
/// Only a value the editor actually read or the user typed is written: a disk
/// whose page was never opened carries no `title_loaded`, and writing its empty
/// field would clear a title that is merely unknown to this form.
async function applyDiskTitles(conn: string, devices: HwDevice[]) {
  for (const d of devices) {
    if (d.kind !== "disk" || d.fields.title_loaded !== "1") continue;
    const title = (d.fields.title ?? "").trim();
    if (title === (d.fields.title_original ?? "").trim()) continue;
    const pool = (d.fields.pool ?? "").trim();
    const name = (d.fields.volume ?? "").trim();
    // A path outside every pool has no title to set.
    if (!pool || !name) continue;
    await setVolumeTitle(conn, pool, name, title);
  }
}

/// What picking a graphics type costs, said before it is saved rather than
/// discovered on the Console tab.
///
/// This matters because the two are not equal for a browser user: the built-in
/// console is RFB only, so switching a guest to SPICE takes its console away and
/// leaves a virt-viewer download. That is a fair trade for someone with
/// virt-viewer, and a straight loss for someone on a phone, so it is stated
/// rather than assumed. A guest can carry *both* devices -- virt-viewer then
/// prefers SPICE while the browser keeps VNC -- which is why the hint points at
/// Add Hardware instead of pretending there is a choice to make here.
function gfxNote(kind: "vnc" | "spice"): string {
  return kind === "spice"
    ? "SPICE is not shown in the browser: the Console tab will offer a virt-viewer download instead. Add a VNC device too if you also want the built-in console."
    : "VNC is what the built-in console speaks, over the daemon's own compression.";
}

/// Whether a device field is shown, given a way to read the current value of
/// any field on the device (its own `fields` map for an existing device, the
/// `draft` for the Add Hardware chooser) and whether advanced options are being
/// shown.
///
/// Three gates, checked here so the device page and the chooser cannot drift:
/// `showWhen` (a field only applies to some other field's value), `hideWhenBlank`
/// (a derived field with nothing to show), and `advanced` (an optional extra,
/// hidden until the user asks for it -- or already set, so a definition read
/// back from libvirt never hides something it carries). Keeping the extras
/// behind `advanced` is what makes a form open with its essentials instead of
/// every option the device could take at once.
function visibleField(
  f: HwField,
  value: (key: string) => string,
  showAdvanced: boolean,
): boolean {
  if (f.showWhen && value(f.showWhen.key) !== f.showWhen.value) return false;
  if (f.hideWhenBlank && !value(f.key).trim()) return false;
  if (f.advanced && !showAdvanced && !value(f.key).trim()) return false;
  return true;
}

/// Proxmox-style hardware editor: device list on the LEFT, the selected
/// device's form on the right. Used for both "Create VM" and editing an
/// existing guest's hardware, so the two can never drift apart.
export async function openHardwareEditor(opts: {
  vm?: string;
  inline?: boolean;
  /// Create on this server instead of the selected one (the admin page).
  conn?: string;
  /// An admin creating the guest for someone else: who gets it, and what to
  /// run once it exists (repaint the admin page, which stays open).
  forUser?: { owner: string; after: () => void };
} = {}) {
  const conn = opts.conn ?? state.selectedConn;
  if (!conn) return;
  const editing = !!opts.vm;
  const owner = editing ? null : opts.forUser?.owner ?? null;
  const inline = !!opts.inline;

  // Existing guest: read its current definition so the form starts populated.
  let baseXml = "";
  // Per-device-kind state for the "advanced options" toggle, so a device page
  // opens with its essentials and reveals the optional extras only when asked.
  const showAdvanced: Record<string, boolean> = {};
  // A new guest starts with a video adapter like `domainXml` used to write: the
  // editor owns `<video>` now, so nothing else puts one in.
  let devices: HwDevice[] = [{ kind: "video", fields: { model: "virtio", device: "virtio-vga" } }];
  let general = {
    name: "",
    cpus: 2,
    memory_mb: 2048,
    firmware: "uefi" as "uefi" | "bios",
    graphics: "vnc" as "vnc" | "spice",
  };

  if (editing && opts.vm) {
    try {
      const { xml } = await api.get<{ xml: string }>(
        `/proxy/${conn}/domains/${encodeURIComponent(opts.vm)}/xml`);
      baseXml = xml;
      devices = devicesFromXml(xml);
      // A guest defined before video was editable may carry no `<video>` at
      // all; give the editor one to show, so saving hardware does not leave the
      // guest without a display adapter.
      if (!devices.some((d) => d.kind === "video")) {
        devices.push({ kind: "video", fields: { model: "virtio", device: "virtio-vga" } });
      }
      general = {
        // The display name: the title when the guest has one.
        name: unescapeXml(xml.match(/<title>([^<]*)<\/title>/)?.[1] ?? "") || opts.vm,
        cpus: +(xml.match(/<vcpu[^>]*>(\d+)<\/vcpu>/)?.[1] ?? 2),
        memory_mb: Math.round(+(xml.match(/<memory[^>]*>(\d+)<\/memory>/)?.[1] ?? 2097152) / 1024),
        firmware: /pflash|OVMF/i.test(xml) ? "uefi" : "bios",
        graphics: /<graphics[^>]*type=['"]spice/.test(xml) ? "spice" : "vnc",
      };
    } catch (e) {
      toast((e as Error).message, "bad");
      return;
    }
  }

  // New VM: one disk is created up front, described by these fields.
  // The boot disk: a new volume, or one the user already has ("existing").
  const newDisk = {
    pool: "", size_gb: 10, format: "qcow2" as "raw" | "qcow2",
    mode: "new" as "new" | "existing", volume: "",
    // The boot disk's nickname, for the volume this form creates. An existing
    // volume keeps the title it already has.
    title: "",
  };

  // Create VM keeps its modal; editing an existing guest renders the editor
  // inline in the Hardware tab, so devices are editable without an extra
  // "Edit Hardware" click.
  // Unit selectors are presentation state: the model stays in MiB and GiB, so
  // changing a unit converts the number in the field, never the stored size.
  let memUnit: MemoryUnit = MEMORY_DEFAULT;
  let diskUnit: StorageUnit = STORAGE_DEFAULT;

  const modal = inline ? null : openModal(
    editing ? `Hardware — ${esc(opts.vm ?? "")}` : "Create VM",
    "",
    `<button class="btn" id="hw-cancel">Cancel</button>
     <button class="btn primary" id="hw-save">${editing ? "Save" : "Create"}</button>`,
  );
  if (modal) modal.querySelector(".modal")?.classList.add("hw-modal");

  const { pools, nets } = await loadFormData(conn);
  // A guest's NIC names the bridge it is plugged into; the form picks by
  // network. The bridge is kept as it is, so a NIC on a bridge no network here
  // owns still saves back unchanged.
  for (const d of devices) {
    if (d.kind === "interface" && !d.fields.network && d.fields.bridge) {
      d.fields.network = bridgeNetwork(nets, d.fields.bridge);
    }
  }
  // Hard ceilings from the caller's remaining limits, so a field cannot be set
  // past what the server would accept and then be refused after the work. An
  // edit may grow by the delta, so the guest's own use is added back.
  const left = await fieldsLeft(conn);
  const cap = {
    cpu: left.vcpus == null ? null : left.vcpus + (editing ? general.cpus : 0),
    mem: left.memory_mb == null ? null : left.memory_mb + (editing ? general.memory_mb : 0),
    disk: left.storage_gb == null ? null : left.storage_gb,
  };
  if (cap.disk != null) newDisk.size_gb = Math.min(newDisk.size_gb, Math.max(MIN_DISK_GIB, cap.disk));
  // Volumes per pool, fetched lazily and cached for the device forms.
  const volCache: Record<string, VolumeSummary[]> = {};
  const volumes = async (pool: string): Promise<VolumeSummary[]> => {
    if (!pool) return [];
    if (!volCache[pool]) {
      try {
        volCache[pool] = await api.get<VolumeSummary[]>(
          `/proxy/${conn}/pools/${encodeURIComponent(pool)}/volumes`);
      } catch {
        volCache[pool] = [];
      }
    }
    return volCache[pool];
  };
  /// Point a disk at its volume's path, so the definition refers to the file
  /// rather than to a pool+volume pair. libvirt cannot resolve a volume source
  /// Reconcile a disk's `path` and its `pool`+`volume` pair, in both
  /// directions, so the form can preselect and the XML is still written by
  /// path.
  ///
  /// A path is what `deviceXml` writes (libvirt 11.3 cannot resolve a
  /// pool+volume source into a path, and a disk it cannot resolve never reaches
  /// the guest's AppArmor profile, so the guest will not start). But the form's
  /// dropdowns are pool+volume, and a guest we wrote carries only a path -- so
  /// both were blank for a disk that is plainly attached. Searching the pools
  /// for the file fills them in.
  ///
  /// The path is never dropped. This used to `delete d.fields.path` whenever
  /// the volume lookup missed, which meant opening the Hardware tab on a
  /// path-backed disk and saving rewrote it as `<source pool="" volume=""/>` --
  /// an unusable disk, lost by looking at it. A file outside every pool (the
  /// common case here: an ISO in a synced folder) keeps its path and is simply
  /// not preselected in the dropdowns.
  const resolveDevicePath = async (d: HwDevice) => {
    const name = (d.fields.volume ?? "").trim();
    const pool = (d.fields.pool ?? "").trim();
    const path = (d.fields.path ?? "").trim();

    // pool+volume -> path, when the volume is reachable.
    if (name) {
      const found = (await volumes(pool)).find((v) => v.name === name);
      if (found?.path) {
        d.fields.path = found.path;
        // The volume list carries no format, so it comes from the name: we name
        // our own volumes after their format, and getting this wrong would make
        // qemu parse the file as the wrong image type.
        d.fields.format = /\.raw$/i.test(found.name) ? "raw" : "qcow2";
      }
      return;
    }

    // path -> pool+volume, so the dropdowns show where it lives. Best effort:
    // a path in no pool we can list stays as it is.
    if (path && !pool) {
      const owner = await poolOwning(path);
      if (owner) {
        d.fields.pool = owner.pool;
        d.fields.volume = owner.name;
      }
    }
  };

  /// The pool and volume name for a volume path, or null when no listed pool
  /// contains it. Pools are searched in order and the first match wins, which
  /// is enough: the daemon gives us each volume's real path, so a false match
  /// would need two pools to report the same file.
  const poolOwning = async (path: string): Promise<{ pool: string; name: string } | null> => {
    for (const p of pools) {
      const hit = (await volumes(p.name)).find((v) => v.path === path);
      if (hit) return { pool: p.name, name: hit.name };
    }
    return null;
  };

  /// The name a new volume will get. The nickname field shows it greyed so an
  /// empty field still says which drive it is; the save reuses `new_suffix`
  /// for the file, so the hint names the file that actually appears.
  function pendingDiskName(d: HwDevice, index: number): string {
    const format = (d.fields.format ?? "qcow2") === "raw" ? "raw" : "qcow2";
    // The chooser's draft has no size yet; its field shows the catalogue's
    // default, so the preview must use the same number.
    const fallback = hwType("disk")?.fields.find((f) => f.key === "size_gb")?.value ?? "";
    const raw = d.fields.size_gb ?? fallback;
    const size = raw === "" ? 1 : Number(raw);
    const sizeGb = Math.max(MIN_DISK_GIB, Number.isFinite(size) ? size : 1);
    const guest = editing
      ? (isUuid(opts.vm!) ? opts.vm! : general.name.trim() || opts.vm!)
      : (general.name.trim() || "vm");
    if (editing) return `${guest}-disk${index + 1}-${sizeGb}gb.${format}`;
    // Two guests may share a display name, so a new guest's disks carry a
    // random suffix. It is chosen once, here, and reused by the create.
    if (!d.fields.new_suffix) d.fields.new_suffix = newUuid().slice(0, 8);
    return `${guest}-disk${d.fields.new_suffix}.${format}`;
  }

  /// The drive's own name, for an empty nickname field: the volume name, or
  /// the file name for a disk that only has a path.
  function diskDefaultName(d: HwDevice, index: number): string {
    if ((d.fields.mode ?? "existing") === "new") return pendingDiskName(d, index);
    const name = (d.fields.volume ?? "").trim();
    if (name) return name;
    const path = (d.fields.path ?? "").trim();
    return path ? path.split("/").pop() ?? path : "";
  }

  /// Whether there is a volume whose title can carry the nickname. A disk
  /// attached by a path outside every pool has file names only, so offering
  /// the field would promise storage that does not exist.
  function diskTitleWritable(d: HwDevice): boolean {
    if (!(d.fields.pool ?? "").trim()) return false;
    return (d.fields.mode ?? "existing") === "new" || !!(d.fields.volume ?? "").trim();
  }

  /// Read a disk's nickname from its volume, and remember the value the server
  /// already has, so a save writes only what this form changed.
  async function loadDiskTitle(d: HwDevice) {
    if (d.kind !== "disk" || d.fields.title_loaded === "1") return;
    let pool = (d.fields.pool ?? "").trim();
    let name = (d.fields.volume ?? "").trim();
    if (!name) {
      const path = (d.fields.path ?? "").trim();
      if (!path) return;
      const owner = await poolOwning(path);
      if (!owner) return;
      pool = owner.pool;
      name = owner.name;
    }
    const v = (await volumes(pool)).find((x) => x.name === name);
    if (!v) return;
    // Display only: the list row shows the size beside the nickname.
    d.fields.cap_gib = String(v.capacity / 2 ** 30);
    d.fields.title = v.title ?? "";
    d.fields.title_original = d.fields.title;
    d.fields.title_loaded = "1";
  }
  // Pools that contain at least one ISO. The cdrom pool dropdown hides
  // every other pool, so a pool with no bootable media never appears.
  let isoPoolsCache: string[] | null = null;
  const isoPools = async (): Promise<string[]> => {
    if (isoPoolsCache) return isoPoolsCache;
    const out: string[] = [];
    for (const p of pools) {
      if ((await volumes(p.name)).some((v) => v.name.toLowerCase().endsWith(".iso"))) {
        out.push(p.name);
      }
    }
    isoPoolsCache = out;
    return out;
  };
  newDisk.pool = pools[0]?.name ?? "";
  // The list rows read the nickname and size, so load them before the first
  // paint rather than when each disk's page is opened.
  if (editing) await Promise.all(devices.map(loadDiskTitle));

  // New VMs start with an empty CD/DVD drive and a network NIC, so both a
  // boot ISO and connectivity are one click away instead of buried in
  // Add Hardware. The NIC defaults to the first network.
  if (!editing) {
    devices = [{ kind: "cdrom", fields: { pool: newDisk.pool, volume: "" } }];
    if (nets.length) {
      devices.push({ kind: "interface", fields: { network: nets[0].name, bridge: nets[0].bridge, model: "virtio" } });
    }
  }

  // Where the editor paints. Inline mode reuses the Hardware tab body; the
  // footer buttons live outside the re-rendered region so their handlers
  // survive field edits (render() replaces only the .hw-body area).
  let host: HTMLElement;
  let bodyHost: HTMLElement;
  if (inline) {
    host = $("#vm-body") as HTMLElement;
    // Let the editor fill the pane below the tab bar.
    host.parentElement?.classList.add("vm-fill");
    setHtml(host, `<div class="hw-inline">
      <div class="hw-inline-body"></div>
      <div class="hw-foot">
        <button class="btn" id="hw-cancel">Discard</button>
        <button class="btn primary" id="hw-save">Save</button>
      </div>
    </div>`);
    bodyHost = host.querySelector(".hw-inline-body") as HTMLElement;
  } else {
    host = modal!.querySelector(".modal-body") as HTMLElement;
    bodyHost = host;
    host.style.padding = "0";
    host.style.overflow = "hidden";
  }
  // The foot buttons live in `root` (the modal wrapper or the inline host).
  const root = (inline ? host : modal!) as HTMLElement;

  let page = "general";

  const opt = (list: string[], sel: string) =>
    list.map((v) => `<option value="${esc(v)}" ${v === sel ? "selected" : ""}>${esc(v)}</option>`).join("");

  /// A pool's or network's nickname, for display only.
  ///
  /// Both are named by their UUID and keep the human name as a `title`, so the
  /// raw name is a UUID for anything made or migrated since then. Only the
  /// *label* changes: every option's value stays the libvirt name, because that
  /// is what the XML and the API address them by.
  const resolveName = (kind: "pool" | "network", name: string): string => {
    if (!name) return name;
    const list = kind === "pool" ? pools : nets;
    return list.find((x) => x.name === name)?.title || name;
  };

  /// `opt`, but labelled by nickname. `fallback` names the entry used when the
  /// stored value is not in the list at all (a pool this caller cannot see).
  ///
  /// A network may be left unset: a NIC with no network is a NIC that attaches
  /// to nothing, and libvirt wants a `<source>` even then, so the empty value
  /// becomes `<source network=""/>`. A pool cannot be empty (a disk names one),
  /// so the blank entry is offered for networks only.
  const optNamed = (kind: "pool" | "network", list: string[], sel: string) =>
    (kind === "network"
      ? `<option value="" ${sel ? "" : "selected"}>None</option>`
      : "") +
    list
      .map((v) => `<option value="${esc(v)}" ${v === sel ? "selected" : ""}>${esc(resolveName(kind, v))}</option>`)
      .join("");

  /** Sidebar rows mirror Proxmox: label plus the current value. */
  function sidebar(): string {
    const fixed = [
      { key: "general", icon: ic.datacenter(), label: "General", sum: general.name || "Unnamed" },
      { key: "processors", icon: ic.cpu(), label: "Processors", sum: `${general.cpus} vCPU` },
      { key: "memory", icon: ic.memory(), label: "Memory", sum: `${unitValue(general.memory_mb, "MiB", memUnit)} ${memUnit}` },
      ...(editing ? [] : [{
        key: "disk", icon: ic.disk(), label: "Hard Disk",
        sum: newDisk.mode === "existing"
          ? (newDisk.volume || "Choose a volume")
          : `${unitValue(newDisk.size_gb, "GiB", diskUnit)} ${diskUnit} · ${newDisk.title.trim() || PRIMARY_DISK}`,
      }]),
      { key: "display", icon: ic.vm(), label: "Display", sum: general.graphics.toUpperCase() },
      { key: "firmware", icon: ic.cpu(), label: "BIOS", sum: general.firmware === "uefi" ? "UEFI (OVMF)" : "BIOS (SeaBIOS)" },
    ];
    const rows = fixed.map((f) => `
      <button class="hw-item ${page === f.key ? "active" : ""}" data-hw="${f.key}">
        <span class="hw-ic">${f.icon}</span>
        <span class="hw-tx"><b>${esc(f.label)}</b><i>${esc(f.sum)}</i></span>
      </button>`).join("");
    const custom = devices.map((d, i) => {
      const t = hwType(d.kind);
      return `
      <button class="hw-item hw-custom ${page === `dev-${i}` ? "active" : ""}" data-hw="dev-${i}">
        <span class="hw-ic">${ic[t?.icon ?? "cpu"]()}</span>
        <span class="hw-tx"><b>${esc(t?.label ?? d.kind)}</b><i>${esc(deviceSummary(d, resolveName))}</i></span>
      </button>`;
    }).join("");
    return `
      <div class="hw-side-head">HARDWARE</div>
      <div class="hw-side-scroll">${rows}<div class="hw-custom-list">${custom}</div></div>
      <button class="hw-add" id="hw-add">${ic.plus(15)} ADD HARDWARE</button>`;
  }

  async function pageHtml(): Promise<string> {
    if (page.startsWith("dev-")) {
      const i = +page.slice(4);
      const d = devices[i];
      const t = hwType(d.kind);
      if (!t) return `<div class="hw-page active"><div class="hw-note">Unknown device.</div></div>`;
      // A cdrom whose pool has no ISOs points at the first pool that does,
      // so its volume list is never silently empty.
      if (d.kind === "cdrom") {
        const withIsos = await isoPools();
        if (withIsos.length && !withIsos.includes(d.fields.pool ?? "")) {
          d.fields.pool = withIsos[0];
        }
      }
      // After the pool is settled, so the path is looked up in the right one.
      if (d.kind === "disk" || d.kind === "cdrom") await resolveDevicePath(d);
      if (d.kind === "disk") {
        if ((d.fields.mode ?? "existing") === "new") {
          // Nothing to read yet: the volume is made at save time.
          if (d.fields.title_loaded !== "1") {
            d.fields.title_loaded = "1";
            d.fields.title_original = "";
          }
        } else {
          await loadDiskTitle(d);
        }
      }
      // The value reader is the device's own fields, with each field's declared
      // default standing in for one it does not carry.
      const fieldVal = (k: string) =>
        d.fields[k] ?? t.fields.find((x) => x.key === k)?.value ?? "";
      const showAdv = showAdvanced[d.kind] ?? false;
      const fields = await Promise.all(t.fields
        .filter((f) => !(f.key === "title" && d.kind === "disk" && !diskTitleWritable(d)))
        .filter((f) => visibleField(f, fieldVal, showAdv))
        .map(async (f) => {
        const val = d.fields[f.key] ?? f.value ?? "";
        if (f.key === "title" && d.kind === "disk") {
          return `<label class="field"><span>${esc(f.label)}</span>
            <input data-field="title" type="text" value="${esc(val)}"
              placeholder="${esc(diskDefaultName(d, i))}"></label>`;
        }
        if (f.type === "select") {
          let list: string[] = (f.options ?? []).map((o) => o.value);
          if (f.key === "pool") list = d.kind === "cdrom" ? await isoPools() : pools.map((p) => p.name);
          if (f.key === "network") list = nets.map((n) => n.name);
          if (f.key === "volume") {
            list = (await volumes(d.fields.pool ?? pools[0]?.name ?? "")).map((v) => v.name);
            // The cdrom picks an ISO; a disk may pick any volume.
            if (d.kind === "cdrom") list = list.filter((v) => v.toLowerCase().endsWith(".iso"));
            // The current volume must be in its own list. A path-backed disk
            // resolves to a pool+volume that may not be in that pool's list yet
            // (a stale daemon cache, or a volume this caller cannot see), and an
            // absent option renders the select blank -- which is the bug where
            // the disk looked unattached. Prepending it keeps the truth visible.
            if (val && !list.includes(val)) list = [val, ...list];
          }
          // A cdrom may be empty: that is a valid empty drive.
          const emptyOpt = d.kind === "cdrom" && f.key === "volume"
            ? `<option value="" ${val ? "" : "selected"}>No Media</option>`
            : "";
          const labels = f.options?.length
            ? f.options.map((o) => `<option value="${esc(o.value)}" ${o.value === val ? "selected" : ""}>${esc(o.label)}</option>`).join("")
            : emptyOpt + (f.key === "pool" || f.key === "network"
              ? optNamed(f.key, list, val)
              : opt(list, val));
          return `<label class="field"><span>${esc(f.label)}</span>
            <select data-field="${esc(f.key)}">${labels}</select></label>`;
        }
        if (f.key === "size_gb") {
          const unit = (d.fields.size_unit as StorageUnit) || STORAGE_DEFAULT;
          const bound = cap.disk == null ? null : convert(cap.disk, "GiB", unit);
          return `<label class="field"><span>${esc(f.label)}</span>
            <div class="unit-row">
              <input data-field="${esc(f.key)}" type="number" value="${esc(unitValue(Number(val), "GiB", unit))}"
                min="${unitValue(MIN_DISK_GIB, "GiB", unit)}" step="any"${maxAttr(bound)}
                placeholder="${esc(f.placeholder ?? "")}">
              <select data-field="size_unit">${unitOptions(STORAGE_UNITS, unit)}</select>
            </div></label>`;
        }
        const bound = f.max;
        return `<label class="field"><span>${esc(f.label)}</span>
          <input data-field="${esc(f.key)}" type="${f.type}" value="${esc(val)}"
            ${f.readonly ? "readonly" : ""}
            ${f.min != null ? `min="${f.min}"` : ""}${maxAttr(bound)}
            placeholder="${esc(f.placeholder ?? "")}"></label>`;
      }));
      // The "advanced options" toggle only appears for a device that has any.
      const hasAdvanced = t.fields.some((f) => f.advanced);
      const advToggle = hasAdvanced
        ? `<button class="btn hw-adv-toggle" id="hw-adv">${showAdv ? "Hide advanced options" : "Show advanced options"}</button>`
        : "";
      return `<div class="hw-page active">
        <h3 class="hw-title">${esc(t.label)}</h3>
        ${fields.join("")}
        <div class="actions">
          ${advToggle}
          <button class="btn danger" id="hw-remove">${ic.trash(14)} Remove Device</button>
        </div>
      </div>`;
    }

    switch (page) {
      case "processors":
        return `<div class="hw-page active">
          <h3 class="hw-title">Processors</h3>
          <label class="field"><span>vCPUs</span>
            <input id="g-cpus" type="number" min="1" max="${cap.cpu ?? 128}" value="${general.cpus}"></label>
        </div>`;
      case "memory":
        return `<div class="hw-page active">
          <h3 class="hw-title">Memory</h3>
          <label class="field"><span>Memory</span>
            <div class="unit-row">
              <input id="g-mem" type="number" min="${convert(128, "MiB", memUnit)}" step="any"
                ${maxAttr(cap.mem == null ? null : convert(cap.mem, "MiB", memUnit))}
                value="${unitValue(general.memory_mb, "MiB", memUnit)}">
              <select id="g-mem-unit">${unitOptions(MEMORY_UNITS, memUnit)}</select>
            </div></label>
        </div>`;
      case "disk": {
        // Volumes the user may boot from: their own, and not already another
        // guest's disk (one writable disk in two guests corrupts both). An
        // admin sees every free volume. ISOs are media, not boot disks.
        const all = await volumes(newDisk.pool);
        const usable = all.filter((v) => !v.name.toLowerCase().endsWith(".iso")
          && (hooks.canAdmin() || v.mine) && !(v.used_by?.length));
        const held = all.filter((v) => (hooks.canAdmin() || v.mine) && v.used_by?.length);
        if (newDisk.mode === "existing" && !usable.some((v) => v.name === newDisk.volume)) {
          newDisk.volume = usable[0]?.name ?? "";
        }
        const mode = `<label class="field"><span>Disk</span><select id="g-disk-mode">
            <option value="new" ${newDisk.mode === "new" ? "selected" : ""}>Create a New Volume</option>
            <option value="existing" ${newDisk.mode === "existing" ? "selected" : ""}>Use One of My Volumes</option>
          </select></label>`;
        const pool = `<label class="field"><span>Storage Pool</span>
            <select id="g-pool">${optNamed("pool", pools.map((p) => p.name), newDisk.pool)}</select></label>`;
        if (newDisk.mode === "existing") {
          return `<div class="hw-page active">
            <h3 class="hw-title">Hard Disk</h3>
            ${mode}${pool}
            ${usable.length ? `<label class="field"><span>Volume</span><select id="g-volume">
                ${usable.map((v) => `<option value="${esc(v.name)}" ${v.name === newDisk.volume ? "selected" : ""}>
                  ${esc(`${v.title || v.name} — ${fmtBytes(v.capacity)}`)}</option>`).join("")}
              </select></label>`
              : `<p class="hint">You have no free volumes in this pool${held.length ? " (the ones you have are already attached to a guest)" : ""}.
                  Create one from the pool's page, or choose "Create a New Volume".</p>`}
            ${held.length ? `<p class="hint">In use, not offered: ${held.map((v) => esc(`${v.name} (${v.used_by!.join(", ")})`)).join("; ")}.</p>` : ""}
          </div>`;
        }
        return `<div class="hw-page active">
          <h3 class="hw-title">Hard Disk</h3>
          ${mode}
          <label class="field"><span>Nickname</span>
            <input id="g-disk-title" value="${esc(newDisk.title)}"
              placeholder="${PRIMARY_DISK}"></label>
          ${pool}
          <label class="field"><span>Disk Size</span>
            <div class="unit-row">
              <input id="g-size" type="number" min="${unitValue(MIN_DISK_GIB, "GiB", diskUnit)}" step="any"
                ${maxAttr(cap.disk == null ? null : convert(cap.disk, "GiB", diskUnit))}
                value="${unitValue(newDisk.size_gb, "GiB", diskUnit)}">
              <select id="g-size-unit">${unitOptions(STORAGE_UNITS, diskUnit)}</select>
            </div></label>
          <label class="field"><span>Format</span>
            <select id="g-format">${opt(["qcow2", "raw"], newDisk.format)}</select></label>
        </div>`;
      }
      case "display":
        return `<div class="hw-page active">
          <h3 class="hw-title">Display</h3>
          <label class="field"><span>Protocol</span>
            <select id="g-gfx">${opt(["vnc", "spice"], general.graphics)}</select></label>
          <p class="muted hw-note" id="g-gfx-note">${gfxNote(general.graphics)}</p>
        </div>`;
      case "firmware":
        return `<div class="hw-page active">
          <h3 class="hw-title">BIOS</h3>
          <label class="field"><span>Firmware</span>
            <select id="g-fw">${opt(["uefi", "bios"], general.firmware)}</select></label>
        </div>`;
      default:
        return `<div class="hw-page active">
          <h3 class="hw-title">General</h3>
          <label class="field"><span>Name</span>
            <input id="g-name" value="${esc(general.name)}" placeholder="arch-box"></label>
        </div>`;
    }
  }

  async function render() {
    // The page first: it can settle choices the sidebar summarises (the disk
    // page picks a default volume), and a template literal evaluates the
    // sidebar before awaiting the page otherwise.
    const pageBody = await pageHtml();
    setHtml(bodyHost, `<div class="hw-body">
      <nav class="hw-side" id="hw-side">${sidebar()}</nav>
      <div class="hw-pane" id="hw-pane">${pageBody}</div>
    </div>`);
    if (!inline) {
      (bodyHost as HTMLElement).style.padding = "0";
      (bodyHost as HTMLElement).style.overflow = "hidden";
    }

    root.querySelectorAll("[data-hw]").forEach((b: Element) => {
      (b as HTMLElement).onclick = () => {
        page = (b as HTMLElement).dataset.hw!;
        void render();
      };
    });
    $("#hw-add", root).onclick = () => openAddHardware();

    // General/spec fields write straight back into the model.
    const bind = (sel: string, fn: (v: string) => void) => {
      const n = $(sel, root);
      if (!n) return;
      n.onchange = () => { fn(n.value); void render(); };
    };
    // Each field is clamped to its ceiling, but never below its floor: a
    // limit smaller than the minimum (an empty quota) must not produce a
    // value the input cannot hold.
    const clamp = (v: string, lo: number, hi: number | null) => {
      const n = Math.max(lo, Math.floor(+v || lo));
      return hi == null ? n : Math.min(hi, n);
    };
    // A value is read in the unit currently shown and stored in the canonical
    // one, so the number on screen is always the number that will be written.
    bind("#g-name", (v) => { general.name = v; });
    bind("#g-cpus", (v) => { general.cpus = Math.max(1, clamp(v, 1, cap.cpu)); });
    bind("#g-mem", (v) => {
      general.memory_mb = clamp(String(Math.round(convert(Number(v), memUnit, "MiB"))), 128, cap.mem);
    });
    bind("#g-mem-unit", (v) => { memUnit = v as MemoryUnit; });
    bind("#g-gfx", (v) => {
      general.graphics = v as "vnc" | "spice";
      // The note is part of the decision, so it must not show the old choice's
      // text until the page is left and returned to.
      const note = $("#g-gfx-note", root);
      if (note) note.textContent = gfxNote(general.graphics);
    });
    bind("#g-fw", (v) => { general.firmware = v as "uefi" | "bios"; });
    bind("#g-pool", (v) => { newDisk.pool = v; newDisk.volume = ""; });
    bind("#g-disk-title", (v) => { newDisk.title = v; });
    bind("#g-disk-mode", (v) => { newDisk.mode = v === "existing" ? "existing" : "new"; });
    bind("#g-volume", (v) => { newDisk.volume = v; });
    bind("#g-size", (v) => {
      newDisk.size_gb = clampGib(convert(Number(v), diskUnit, "GiB"), cap.disk);
    });
    bind("#g-size-unit", (v) => { diskUnit = v as StorageUnit; });
    bind("#g-format", (v) => { newDisk.format = v as "raw" | "qcow2"; });

    // Device field edits, plus removal.
    if (page.startsWith("dev-")) {
      const i = +page.slice(4);
      root.querySelectorAll("[data-field]").forEach((n: Element) => {
        (n as HTMLElement).onchange = () => {
          const key = (n as HTMLElement).dataset.field!;
          let val = (n as HTMLInputElement).value;
          // A new disk's size is storage the owner holds: read it in the unit
          // the field shows, store it in GiB, and cap it at what is left or the
          // create is refused after the volume is made.
          if (key === "size_gb") {
            const unit = (devices[i].fields.size_unit as StorageUnit) || STORAGE_DEFAULT;
            val = String(clampGib(convert(Number(val), unit, "GiB"), cap.disk));
          }
          devices[i].fields[key] = val;
          if (devices[i].kind === "disk") {
            if (key === "title") devices[i].fields.title_loaded = "1";
            // A different source is a different drive, so its nickname is
            // read again from the volume it now points at.
            else if (key === "mode" || key === "pool" || key === "volume") {
              delete devices[i].fields.title;
              delete devices[i].fields.title_original;
              delete devices[i].fields.title_loaded;
            }
          }
          // A NIC attaches by the network's bridge, so the two fields are one
          // setting: pick a network and its bridge follows, or the save would
          // write the bridge of the network that was chosen before.
          if (devices[i].kind === "interface" && key === "network") {
            devices[i].fields.bridge = nets.find((n) => n.name === val)?.bridge ?? "";
          }
          // A `*-gl` video device and 3D acceleration are one setting: libvirt
          // refuses a gl frontend without enabled 3d (and a plain device with
          // 3d is upgraded to gl). Moving one moves the other, so the save
          // cannot be refused for a mismatch the form let them create.
          if (devices[i].kind === "video") {
            const f = devices[i].fields;
            if (key === "device" && f.device.endsWith("-gl")) f.accel3d = "yes";
            if (key === "device" && !f.device.endsWith("-gl")) f.accel3d = "";
            if (key === "accel3d") {
              if (f.accel3d === "yes" && !f.device.endsWith("-gl")) {
                f.device = f.device === "virtio-gpu" ? "virtio-gpu-gl" : "virtio-vga-gl";
              } else if (f.accel3d !== "yes" && f.device.endsWith("-gl")) {
                f.device = f.device === "virtio-gpu-gl" ? "virtio-gpu" : "virtio-vga";
              }
            }
          }
          // Changing the pool invalidates the volume list.
          void render();
        };
      });
      const rm = $("#hw-remove", root);
      if (rm) rm.onclick = () => {
        devices.splice(i, 1);
        page = "general";
        void render();
      };
      const adv = $("#hw-adv", root);
      if (adv) adv.onclick = () => {
        showAdvanced[devices[i].kind] = !showAdvanced[devices[i].kind];
        void render();
      };
    }
  }

  /** Chooser for a new device; fields come from the catalogue. */
  function openAddHardware() {
    let kind = HW_CATALOG[0].id;
    const draft: Record<string, string> = {};
    // Whether the optional extras are revealed for the current kind. Reset when
    // the kind changes: it is a per-device choice, not a global one.
    let addAdvanced = false;
    // Single-instance devices (memballoon, watchdog, tpm, …) already present
    // cannot be added again: libvirt rejects the duplicate at define time.
    const alreadyAdded = (id: string) =>
      !!hwType(id)?.single && devices.some((d) => d.kind === id);

    const add = openModal("Add Hardware", "",
      `<button class="btn" id="ah-cancel">Cancel</button>
       <button class="btn primary" id="ah-save">Add</button>`);
    add.querySelector(".modal")?.classList.add("add-hw-modal");

    const renderAdd = async () => {
      const t = hwType(kind)!;
      const fieldValue = (key: string) => {
        if (draft[key] !== undefined) return draft[key];
        return t.fields.find((x) => x.key === key)?.value ?? "";
      };
      // What an empty nickname field stands for. An existing volume is named
      // by the dropdown's first entry even before the user touches it; a new
      // one is named by the file the save will create.
      let diskTitleDefault = "";
      let diskTitleAvailable = true;
      if (kind === "disk") {
        if ((draft.mode ?? "existing") === "new") {
          diskTitleAvailable = !!((draft.pool ?? pools[0]?.name ?? "").trim());
          diskTitleDefault = pendingDiskName({ kind, fields: draft }, devices.length);
        } else {
          const list = await volumes(draft.pool ?? pools[0]?.name ?? "");
          diskTitleAvailable = !!draft.volume || list.length > 0;
          diskTitleDefault = draft.volume ?? list[0]?.name ?? "";
        }
      }
      // `visibleField` keeps the form to the fields relevant to the current
      // choices (e.g. an existing-volume disk shows pool+volume, a new-volume
      // disk shows pool+size+format, and an optional extra stays behind the
      // "advanced" toggle). The chooser reads from `draft`, so an option stays
      // hidden until the user is editing it.
      const visible = t.fields
        .filter((f) => !(f.key === "title" && kind === "disk" && !diskTitleAvailable))
        .filter((f) => visibleField(f, fieldValue, addAdvanced));
      const fields = await Promise.all(visible.map(async (f) => {
        const val = draft[f.key] ?? f.value ?? "";
        if (f.key === "title" && kind === "disk") {
          return `<label class="field"><span>${esc(f.label)}</span>
            <input data-af="title" type="text" value="${esc(val)}"
              placeholder="${esc(diskTitleDefault)}"></label>`;
        }
        if (f.type === "select") {
          let list: string[] = (f.options ?? []).map((o) => o.value);
          if (f.key === "pool") list = kind === "cdrom" ? await isoPools() : pools.map((p) => p.name);
          if (f.key === "network") list = nets.map((n) => n.name);
          if (f.key === "volume") {
            const pool = kind === "cdrom"
              ? (draft.pool ?? (await isoPools())[0] ?? "")
              : (draft.pool ?? pools[0]?.name ?? "");
            list = (await volumes(pool)).map((v) => v.name);
            // The cdrom picks an ISO; a disk may pick any volume.
            if (kind === "cdrom") list = list.filter((v) => v.toLowerCase().endsWith(".iso"));
            // Keep the current value selectable; see the device page above.
            if (val && !list.includes(val)) list = [val, ...list];
          }
          // A cdrom may be empty: that is a valid empty drive.
          const emptyOpt = kind === "cdrom" && f.key === "volume"
            ? `<option value="" ${val ? "" : "selected"}>No Media</option>`
            : "";
          const labels = f.options?.length
            ? f.options.map((o) => `<option value="${esc(o.value)}" ${o.value === val ? "selected" : ""}>${esc(o.label)}</option>`).join("")
            : emptyOpt + (f.key === "pool" || f.key === "network"
              ? optNamed(f.key, list, val)
              : opt(list, val));
          return `<label class="field"><span>${esc(f.label)}</span>
            <select data-af="${esc(f.key)}">${labels}</select></label>`;
        }
        if (f.key === "size_gb") {
          const unit = (draft.size_unit as StorageUnit) || STORAGE_DEFAULT;
          const bound = cap.disk == null ? null : convert(cap.disk, "GiB", unit);
          return `<label class="field"><span>${esc(f.label)}</span>
            <div class="unit-row">
              <input data-af="${esc(f.key)}" type="number" value="${esc(unitValue(Number(val), "GiB", unit))}"
                min="${unitValue(MIN_DISK_GIB, "GiB", unit)}" step="any"${maxAttr(bound)}
                placeholder="${esc(f.placeholder ?? "")}">
              <select data-af="size_unit">${unitOptions(STORAGE_UNITS, unit)}</select>
            </div></label>`;
        }
        const bound = f.max;
        return `<label class="field"><span>${esc(f.label)}</span>
          <input data-af="${esc(f.key)}" type="${f.type}" value="${esc(val)}"
            ${f.min != null ? `min="${f.min}"` : ""}${maxAttr(bound)}
            placeholder="${esc(f.placeholder ?? "")}"></label>`;
      }));

      const preview = deviceXml({ kind, fields: draft }, devices.length + 1) || "—";
      setHtml(add.querySelector(".modal-body")!, `<div class="add-hw-body">
        <aside class="add-hw-types">
          ${HW_CATALOG.map((t2) => {
            const dup = alreadyAdded(t2.id);
            return `
            <button class="add-hw-type ${t2.id === kind ? "active" : ""} ${dup ? "dup" : ""}" data-at="${t2.id}" ${dup ? "disabled" : ""}>
              <b>${esc(t2.label)}</b><small>${dup ? "Already added" : esc(t2.blurb)}</small>
            </button>`;
          }).join("")}
        </aside>
        <div class="add-hw-form">
          <div class="hw-title">${esc(t.label)}</div>
          <div class="add-hw-fields">${fields.join("")}</div>
          ${t.fields.some((f) => f.advanced)
            ? `<button class="btn hw-adv-toggle" id="ah-adv">${addAdvanced ? "Hide advanced options" : "Show advanced options"}</button>`
            : ""}
          <div class="field add-hw-preview"><span>LIBVIRT XML PREVIEW</span>
            <code>${esc(preview)}</code></div>
        </div>
      </div>`);
      (add.querySelector(".modal-body") as HTMLElement).style.padding = "0";
      (add.querySelector(".modal-body") as HTMLElement).style.overflow = "hidden";

      add.querySelectorAll("[data-at]").forEach((b: Element) => {
        (b as HTMLElement).onclick = () => {
          kind = (b as HTMLElement).dataset.at!;
          for (const k of Object.keys(draft)) delete draft[k];
          addAdvanced = false;
          void renderAdd();
        };
      });
      const advToggle = add.querySelector("#ah-adv");
      if (advToggle) (advToggle as HTMLElement).onclick = () => {
        addAdvanced = !addAdvanced;
        void renderAdd();
      };
      add.querySelectorAll("[data-af]").forEach((n: Element) => {
        (n as HTMLElement).onchange = () => {
          const key = (n as HTMLElement).dataset.af!;
          // The size field shows the chosen unit; `deviceXml` and the volume
          // API speak GiB, so convert on the way in.
          draft[key] = key === "size_gb"
            ? String(clampGib(convert(Number((n as HTMLInputElement).value),
              (draft.size_unit as StorageUnit) || STORAGE_DEFAULT, "GiB"), cap.disk))
            : (n as HTMLInputElement).value;
          if (kind === "disk") {
            if (key === "title") draft.title_loaded = "1";
            // A different source is a different drive, so its nickname is
            // read again from the volume it now points at.
            else if (key === "mode" || key === "pool" || key === "volume") {
              delete draft.title;
              delete draft.title_original;
              delete draft.title_loaded;
            }
          }
          // Keep a gl device and 3D acceleration in step; see the device page.
          if (kind === "video") {
            if (key === "device" && draft.device.endsWith("-gl")) draft.accel3d = "yes";
            if (key === "device" && !draft.device.endsWith("-gl")) draft.accel3d = "";
            if (key === "accel3d") {
              const gl = (draft.device ?? "").endsWith("-gl");
              if (draft.accel3d === "yes" && !gl) {
                draft.device = draft.device === "virtio-gpu" ? "virtio-gpu-gl" : "virtio-vga-gl";
              } else if (draft.accel3d !== "yes" && gl) {
                draft.device = draft.device === "virtio-gpu-gl" ? "virtio-gpu" : "virtio-vga";
              }
            }
          }
          void renderAdd();
        };
      });
      $("#ah-cancel", add).onclick = () => add.remove();
      $("#ah-save", add).onclick = () => {
        // A duplicate single-instance device would fail at define time with
        // e.g. "only a single memory balloon device is supported".
        if (alreadyAdded(kind)) {
          return toast(`${hwType(kind)!.label} is already added`, "bad");
        }
        // Seed any untouched field with its rendered value, so a device added
        // without interaction is still valid. A size seeded this way is in the
        // unit on screen, not in GiB; a value the user edited was already
        // converted by the field's change handler.
        let sizeFromDom = false;
        add.querySelectorAll("[data-af]").forEach((n: Element) => {
          const key = (n as HTMLElement).dataset.af!;
          if (draft[key] === undefined) {
            draft[key] = (n as HTMLInputElement).value;
            if (key === "size_gb") sizeFromDom = true;
          }
        });
        if (sizeFromDom) {
          const unit = (draft.size_unit as StorageUnit) || STORAGE_DEFAULT;
          draft.size_gb = String(clampGib(convert(Number(draft.size_gb), unit, "GiB"), cap.disk));
        }
        // A NIC attaches by the network's bridge; the form picks by network,
        // and the bridge is what `deviceXml` writes.
        if (kind === "interface") {
          draft.bridge = nets.find((n) => n.name === draft.network)?.bridge ?? "";
        }
        const device: HwDevice = { kind, fields: { ...draft } };
        const bad = validateDevice(device);
        if (bad) return toast(bad, "bad");
        devices.push(device);
        add.remove();
        page = `dev-${devices.length - 1}`;
        void render();
      };
    };
    void renderAdd();
  }

  $("#hw-cancel", root).onclick = () => {
    if (modal) modal.remove();
    else void hooks.renderVm();
  };
  $("#hw-save", root).onclick = async () => {
    for (const d of devices) {
      const bad = validateDevice(d);
      if (bad) return toast(bad, "bad");
    }

    if (editing && opts.vm) {
      const oldName = opts.vm;
      // A guest named by its UUID is renamed by its title: its libvirt name
      // is its identity and never changes. An older guest, named by what
      // people call it, is renamed in libvirt as before.
      const byUuid = isUuid(oldName);
      const shown = general.name.trim();
      const renamed = !byUuid && shown && shown !== oldName;
      // Close only the modal; the inline editor stays visible and the
      // post-task renderVm() below refreshes it from the new definition.
      modal?.remove();
      void runTask(`Update ${shown || displayName(guestList(conn), oldName)}`, async (set) => {
        let target = oldName;
        if (renamed) {
          // Rename through the dedicated endpoint: it swaps the persistent
          // definition while keeping the UUID. A name baked into the XML PUT
          // below would instead define a second domain with a clashing UUID.
          set(`renaming ${displayName(guestList(conn), oldName)} → ${general.name.trim()}`);
          await api.post(
            `/proxy/${conn}/domains/${encodeURIComponent(oldName)}/rename`,
            { name: general.name.trim() });
          target = general.name.trim();
          if (state.selectedVm === oldName) state.selectedVm = target;
        }
        set("rebuilding domain definition");
        // Create any "new volume" disks before the XML references them.
        await createPendingVolumes(conn, target, devices, set);
        // Nicknames are volume titles, so they need the volume to exist.
        await applyDiskTitles(conn, devices);
        // Strip the devices this editor owns, then splice the current set back
        // in, so libvirt-added devices we do not model survive the round trip.
        const stripped = baseXml
          .replace(/<disk\b[^>]*device=['"](disk|cdrom)['"][\s\S]*?<\/disk>\s*/g, "")
          .replace(/<interface\b[^>]*type=['"](?:network|bridge)['"][\s\S]*?<\/interface>\s*/g, "")
          .replace(/<rng\b[\s\S]*?<\/rng>\s*/g, "")
          // Both self-closing and child-bearing shapes must be stripped:
          // libvirt writes <memballoon model='virtio'> with an <address>
          // child, and <watchdog> as self-closing, so matching only one shape
          // leaves the other behind and the splice re-adds a duplicate.
          .replace(/<memballoon\b(?:[^>]*\/>|[\s\S]*?<\/memballoon>)\s*/g, "")
          .replace(/<channel\b[\s\S]*?org\.qemu\.guest_agent[\s\S]*?<\/channel>\s*/g, "")
          .replace(/<tpm\b[\s\S]*?<\/tpm>\s*/g, "")
          .replace(/<watchdog\b(?:[^>]*\/>|[\s\S]*?<\/watchdog>)\s*/g, "")
          // The video adapter is owned by the device catalogue now, so it is
          // stripped and re-added through `withDevices` like the rest. Leaving
          // it in place would splice a second `<video>` into a definition that
          // already has one, and libvirt allows only one.
          .replace(/<video\b(?:[^>]*\/>|[\s\S]*?<\/video>)\s*/g, "")
          // The graphics device is stripped and re-added below, like the rest.
          // Leaving the old one in place appended the new one instead of
          // replacing it, so a guest ended up with *both* a VNC and a SPICE
          // device -- and each consumer picks a different one (`parse_graphics`
          // prefers VNC, the .vv endpoint prefers SPICE), so switching the
          // display type never took effect and always looked like it had
          // switched itself back. Its `<audio>` is stripped with it: libvirt
          // pairs the two, and a leftover `type='spice'` audio beside a VNC
          // display is the shape that was refused for a non-admin.
          .replace(/<graphics\b(?:[^>]*\/>|[\s\S]*?<\/graphics>)\s*/g, "")
          .replace(/<audio\b(?:[^>]*\/>|[\s\S]*?<\/audio>)\s*/g, "")
          .replace(/<vcpu([^>]*)>\d+<\/vcpu>/, `<vcpu$1>${general.cpus}</vcpu>`)
          .replace(/<memory([^>]*)>\d+<\/memory>/, `<memory$1>${general.memory_mb * 1024}</memory>`)
          .replace(/<currentMemory([^>]*)>\d+<\/currentMemory>/, `<currentMemory$1>${general.memory_mb * 1024}</currentMemory>`)
          .replace(/<name>[^<]*<\/name>/, `<name>${esc(target)}</name>`);
        // The display name lives in the title for a UUID-named guest.
        const titled = byUuid ? withTitle(stripped, shown || displayName(guestList(conn), oldName)) : stripped;
        // The graphics device is not a catalogue device (there is exactly one,
        // edited on the Display page), so it is re-added here rather than
        // through `withDevices`.
        const withDisplay = addDisplay(titled, general.graphics);
        set("defining domain");
        await api.putXml(`/proxy/${conn}/domains/${encodeURIComponent(target)}/xml`,
          withFirmware(withDevices(withDisplay, devices), general.firmware, target));
        await hooks.refreshTreeVms();
      }, { done: "Hardware saved", notify: toast }).then(() => void hooks.renderVm());
      return;
    }

    if (!general.name.trim()) return toast("Name is required", "bad");
    // Caught here, with the form still open, rather than after it closes.
    if (newDisk.mode === "existing" && !newDisk.volume) {
      page = "disk";
      void render();
      return toast("Choose a volume for the hard disk, or create a new one", "bad");
    }
    // A new guest is named by a fresh UUID in libvirt, with the name typed
    // here as its title, so two guests may share a display name. The VM UUID
    // remains its libvirt identity; new disks use the nickname plus random IDs.
    const vmUuid = newUuid();
    const form: DomainForm = {
      uuid: vmUuid,
      name: general.name.trim(),
      cpus: general.cpus,
      memory_mb: general.memory_mb,
      firmware: general.firmware,
      graphics: general.graphics,
      network: "",
      bridge: "",
      pool: newDisk.pool,
      new_disk: false,
      existing_volume: `${general.name.trim()}.qcow2`,
      disk_size_gb: newDisk.size_gb,
      disk_format: newDisk.format,
      iso: "",
      iso_pool: newDisk.pool,
    };
    modal?.remove();
    // Volumes made on the way, deleted again if the guest is not defined:
    // otherwise a refused create (quota, permission) leaves a disk behind that
    // still counts against the user's storage.
    const made: { pool: string; name: string }[] = [];
    void runTask(`Create VM ${form.name}`, async (set) => {
      try {
      let disk: { name: string; path: string };
      if (newDisk.mode === "existing") {
        // One of the user's own volumes, by path like a new one. The format
        // follows what the server will accept: qcow2 only for a volume made
        // empty here, since an uploaded qcow2 can name any host file as its
        // backing store; anything else goes on as raw.
        const v = (await volumes(newDisk.pool)).find((x) => x.name === newDisk.volume);
        if (!v) throw new Error("choose a volume for the hard disk, or create a new one");
        disk = { name: v.name, path: v.path };
        const qcow = v.name.toLowerCase().endsWith(".qcow2");
        form.disk_format = qcow && (v.blank || hooks.canAdmin()) ? "qcow2" : "raw";
      } else {
        set(`creating ${unitValue(newDisk.size_gb, "GiB", "GiB")} GiB volume`);
        disk = await api.post<{ name: string; path: string }>(
          `/proxy/${conn}/pools/${encodeURIComponent(newDisk.pool)}/volumes`,
          { xml: volumeXml({ name: `${general.name.trim()}-${newUuid().slice(0, 8)}.qcow2`, capacity_gb: newDisk.size_gb, format: newDisk.format }) });
        // Only a volume made here is deleted again if the create is refused;
        // a chosen one was the user's before and stays theirs.
        made.push({ pool: newDisk.pool, name: disk.name });
        await setVolumeTitle(conn, newDisk.pool, disk.name, newDisk.title.trim() || PRIMARY_DISK);
      }
      // Refer to the disk by its absolute path rather than by pool+volume: the
      // reference form cannot be resolved by every libvirt, and a disk libvirt
      // cannot resolve never reaches the guest's AppArmor profile, so qemu is
      // refused permission to open it and the guest will not start.
      form.disk_path = disk.path;
      // Create any additional "new volume" disks before defining the domain.
      await createPendingVolumes(conn, form.name, devices, set, made, true);
      await applyDiskTitles(conn, devices);
      set("defining domain");
      // Defined shut off: creating a guest must not also boot it, so a
      // mis-sized or mis-configured VM cannot start eating resources before
      // it has been reviewed. Start it deliberately from the VM view.
      // No IPAM step: the daemon's DHCP server gives each NIC an address the
      // first time it asks and keeps it until the VM is deleted. (Allocating
      // here rewrote the network definition, which restarted it and
      // disconnected every other guest on it.)
      await api.postXml(`/proxy/${conn}/domains?start=false`, withDevices(domainXml(form), devices));
      } catch (e) {
        for (const v of made) {
          await api.del(`/proxy/${conn}/pools/${encodeURIComponent(v.pool)}/volumes/${encodeURIComponent(v.name)}`)
            .catch(() => undefined);
        }
        throw e;
      }
      if (owner) {
        set("assigning owner");
        await assignOwner(conn, "vm", vmUuid, owner);
      }
    }, { done: `VM ${form.name} created`, notify: toast })
      .then(() => {
        void hooks.refreshTreeVms();
        if (opts.forUser) opts.forUser.after();
        else if (state.selectedConn === conn && !state.selectedVm) hooks.renderConn();
        // Created on a server that is not the app's selected one (the
        // Datacenter's tab, which lists every server's guests): the view that
        // is showing needs the repaint, not the server that was never open.
        else if (!state.selectedConn) hooks.renderDc();
      });
  };

  // Read each disk's nickname before the first paint, so the hardware list
  // names drives the way their owner does. Opening a disk page does this too,
  // but the sidebar would otherwise show the generated file name until then.
  for (const d of devices) {
    if (d.kind === "disk") await loadDiskTitle(d);
  }
  void render();
}
