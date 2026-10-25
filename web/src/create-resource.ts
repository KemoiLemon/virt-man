import { api } from "./api";
import { ic } from "./icons";
import { STORAGE_DEFAULT, STORAGE_UNITS, convert, unitOptions, unitValue } from "./units";
import type { StorageUnit } from "./units";
import { runTask } from "./task";
import { esc, newUuid, poolXml, volumeXml } from "./xml";
import type { PoolForm } from "./xml";
import { shownPool, state } from "./state";
import { openModal, toast } from "./dialogs";
import { hooks } from "./hooks";
import { $ } from "./dom";
import { openCreateVm } from "./create-vm";
import { MIN_DISK_GIB, clampGib } from "./vm-form";
import { openCreateNetwork } from "./network-form";

/// Modal listing the saved daemons; resolves to the chosen connection id, or
/// null when dismissed. Used before creating a guest from the Datacenter.
function pickDaemon(): Promise<string | null> {
  return new Promise((resolve) => {
    if (!state.connections.length) {
      toast("Add a server first", "bad");
      resolve(null);
      return;
    }
    const el = openModal(
      "Create Guest — Select Daemon",
      `<div class="create-resource-grid">
        ${state.connections.map((c) => `
          <button class="create-resource-card" data-pick-conn="${c.id}">
            <span class="metric-ic">${ic.server()}</span>
            <b>${esc(c.name)}</b>
            <small>${esc(c.url)}</small>
          </button>`).join("")}
      </div>`,
      `<button class="btn" id="pick-cancel">Cancel</button>`,
    );
    $("#pick-cancel", el).onclick = () => { el.remove(); resolve(null); };
    el.querySelectorAll("[data-pick-conn]").forEach((b: Element) => {
      (b as HTMLElement).onclick = () => {
        const id = (b as HTMLElement).dataset.pickConn!;
        el.remove();
        resolve(id);
      };
    });
  });
}

/// The resource kinds offered by both the sidebar's Add Resource button and
/// the server dashboard's Create Resource button. Daemons are deliberately not
/// one of them: adding a server is an administrative act, and the only place
/// that offers it is the admin page's Daemons section.
export function resourceCards(): string {
  return `<div class="create-resource-grid">
      <button class="create-resource-card" data-create="vm">
        <span class="metric-ic">${ic.vm()}</span>
        <b>Virtual Machine</b>
        <small>Full hardware editor: CPU, memory, disks and more</small>
      </button>
      <button class="create-resource-card" data-create="ct">
        <span class="metric-ic">${ic.container()}</span>
        <b>Container</b>
        <small>From a template: CPU, memory, root disk and network</small>
      </button>
      <button class="create-resource-card" data-create="network">
        <span class="metric-ic">${ic.network()}</span>
        <b>Network</b>
        <small>NAT, bridge or isolated with DHCP and DNS</small>
      </button>
      <button class="create-resource-card" data-create="pool">
        <span class="metric-ic">${ic.storage()}</span>
        <b>Storage Pool</b>
        <small>Directory-backed pool</small>
      </button>
    </div>`;
}

/// Wire a resource chooser's cards to their create flows. Every kind offered
/// needs a daemon, so it is picked first when none is open.
export function bindResourceCards(el: Element, close: () => void) {
  el.querySelectorAll("[data-create]").forEach((b: Element) => {
    (b as HTMLElement).onclick = () => {
      const kind = (b as HTMLElement).dataset.create!;
      close();
      if (kind === "vm") openCreateVm();
      else if (kind === "ct") withDaemon((conn) => void hooks.openCreateCt(conn));
      else if (kind === "network") withDaemon((conn) => openCreateNetwork(conn));
      else withDaemon((conn) => openCreatePool(conn));
    };
  });
}

/// Run a create flow against a daemon, asking which one when none is selected.
/// Choosing a daemon here is a choice about the resource, not a navigation: the
/// id goes into the flow and `state.selectedConn` is deliberately left alone, so the
/// Datacenter page the user is standing on stays on screen behind the form.
export function withDaemon(run: (conn: string) => void) {
  if (state.selectedConn) {
    run(state.selectedConn);
    return;
  }
  void pickDaemon().then((id) => {
    if (id) run(id);
  });
}

export function dcCreate(run: (conn: string) => void) {
  withDaemon(run);
}

/// Sidebar "Add Resource": daemon, VM, network or storage pool.
export function openAddResource() {
  const el = openModal(
    "Add Resource",
    resourceCards(),
    `<button class="btn" id="ar-cancel">Cancel</button>`,
  );
  $("#ar-cancel", el).onclick = () => el.remove();
  bindResourceCards(el, () => el.remove());
}

/// Dashboard "Create Resource": pick VM / Network / Storage Pool, then hand
/// off to the existing create flows.
export function openCreateResource() {
  if (!state.selectedConn) return;
  const el = openModal(
    "Create Resource",
    resourceCards(),
    `<button class="btn" id="cr-cancel">Cancel</button>`,
  );
  $("#cr-cancel", el).onclick = () => el.remove();
  bindResourceCards(el, () => el.remove());
}

/// Define a storage pool on `conn`. `after` is the admin panel's repaint, which
/// also keeps the flow on the admin page; without it the pool's own server view
/// is refreshed as before.
export function openCreatePool(conn = state.selectedConn, after?: () => void) {
  if (!conn) return;
  const el = openModal(
    "Create Storage Pool",
    `<div class="grid2">
      <label class="field"><span>Name</span><input id="pool-name"></label>
      <label class="field"><span>Target Path</span><input id="pool-target" placeholder="/var/lib/libvirt/images"></label>
    </div>`,
    `<button class="btn primary" id="pool-create">Create</button>`,
  );
  $("#pool-create", el).onclick = async () => {
    const form: PoolForm = {
      name: $("#pool-name", el).value,
      type: "dir",
      target: $("#pool-target", el).value,
      source: "",
      source_format: "auto",
    };
    if (!form.name || !form.target) return toast("Name and target are required", "bad");
    el.remove();
    void runTask(`Create pool ${form.name}`, async () => {
      // Named by a fresh UUID; a pool has no <title>, so its display name
      // goes as ?title= and the daemon keeps it.
      await api.postXml(`/proxy/${conn}/pools?title=${encodeURIComponent(form.name)}`,
        poolXml(form, newUuid()));
    }, { done: `Pool ${form.name} created`, notify: toast })
      .then(() => {
        if (after) { after(); return; }
        void hooks.refreshTreeVms();
        if (state.selectedConn === conn) hooks.renderConn(); else hooks.renderDc();
      });
  };
}

export function openCreateVolume(pool: string) {
  if (!state.selectedConn) return;
  let unit: StorageUnit = STORAGE_DEFAULT;
  const el = openModal(
    `New Volume in ${esc(shownPool(pool))}`,
    `<div class="grid2">
      <label class="field"><span>Name</span><input id="vol-name"></label>
      <label class="field"><span>Size</span>
        <div class="unit-row">
          <input id="vol-size" type="number" value="${unitValue(10, "GiB", unit)}"
            min="${unitValue(MIN_DISK_GIB, "GiB", unit)}" step="any">
          <select id="vol-size-unit">${unitOptions(STORAGE_UNITS, unit)}</select>
        </div></label>
      <label class="field"><span>Format</span><select id="vol-format"><option>qcow2</option><option>raw</option></select></label>
    </div>`,
    `<button class="btn primary" id="vol-create">Create</button>`,
  );
  // Convert the number in place when the unit changes, so the size it means
  // stays the same; the canonical GiB value is read at create time.
  ($("#vol-size-unit", el) as HTMLSelectElement).onchange = () => {
    const next = ($("#vol-size-unit", el) as HTMLSelectElement).value as StorageUnit;
    const input = $("#vol-size", el) as HTMLInputElement;
    if (input.value.trim() !== "") input.value = unitValue(Number(input.value), unit, next);
    unit = next;
  };
  $("#vol-create", el).onclick = async () => {
    const name = $("#vol-name", el).value;
    const raw = ($("#vol-size", el) as HTMLInputElement).value.trim();
    const size = raw === "" ? 0 : clampGib(convert(Number(raw), unit, "GiB"), null);
    const format = ($("#vol-format", el) as HTMLSelectElement).value as "raw" | "qcow2";
    if (!name || !(size > 0)) return toast("Name and size required", "bad");
    el.remove();
    void runTask(`Create volume ${name}`, async () => {
      await api.post(`/proxy/${state.selectedConn}/pools/${encodeURIComponent(pool)}/volumes`, {
        xml: volumeXml({ name, capacity_gb: size, format }),
      });
    }, { done: `Volume ${name} created`, notify: toast });
  };
}
