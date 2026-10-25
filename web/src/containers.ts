import { api } from "./api";
import type { ContainerSummary, NetworkSummary, PoolSummary, VolumeSummary } from "./api";
import { assignOwner } from "./admin";
import { ic } from "./icons";
import { skeleton } from "./skeleton";
import { MEMORY_DEFAULT, MEMORY_UNITS, STORAGE_DEFAULT, convert, unitOptions, unitValue } from "./units";
import type { Unit } from "./units";
import { fmtBytes } from "./stats";
import { runTask } from "./task";
import { esc } from "./xml";
import { state, treeCts, treePools } from "./state";
import { capLabel, displayName, emptyState } from "./util";
import { askConfirm, askText, openModal, toast } from "./dialogs";
import type { DomainDeleteResult } from "./actions";
import { hooks } from "./hooks";
import { clearResourceSelection, guestBase, stillCurrentVm } from "./nav";
import { pollFetches } from "./transfers";
import { $, setHtml } from "./dom";
import { withDaemon } from "./create-resource";
import { canAdmin } from "./boot";
import { refreshTreeVms, selectCtRow, treeCtPower } from "./tree";
import { renderPoolPage } from "./pool-page";

// ---------------------------------------------------------------- containers

/// One container as a card: shared by a server's Containers tab and the
/// Datacenter's, so both offer the same actions. `conn` is the owning server,
/// carried on the card and on every button, because the Datacenter lists
/// containers from every server side by side: an action names its own daemon,
/// never the app's selected one.
export function ctCard(v: ContainerSummary, conn: string, server?: string): string {
  return `
      <div class="storage-card ${v.active ? "ok" : ""}" data-open tabindex="0" data-dc-ct="${esc(v.name)}" data-dc-ct-conn="${esc(conn)}">
        <div class="storage-card-head">
          <span class="storage-card-icon">${ic.container()}</span>
          <span class="storage-card-title">${esc(v.title || v.name)}</span>
          ${server ? `<span class="badge">${esc(server)}</span>` : ""}
          ${v.privileged ? `<span class="badge bad" title="Its root is the host's root">privileged</span>` : ""}
          <span class="badge ${v.active ? "on" : ""}">${v.active ? "Active" : "Inactive"}</span>
        </div>
        <div class="specs">
          <div class="spec"><b>vCPUs</b><span>${v.vcpus || "—"}</span></div>
          <div class="spec"><b>Memory</b><span>${v.memory_mb ? fmtBytes(v.memory_mb * 1024 * 1024) : "—"}</span></div>
        </div>
        <div class="storage-card-actions net-actions">
          <button class="btn icon ${v.active ? "on" : ""}" data-ct-power="${esc(v.name)}" data-ct-conn="${esc(conn)}"
            data-ct-action="${v.active ? "shutdown" : "start"}" title="${v.active ? "Shutdown" : "Start"}">${v.active ? ic.stop() : ic.start()}</button>
          <button class="btn icon" data-ct-console="${esc(v.name)}" data-ct-conn="${esc(conn)}" title="Console">${ic.keyboard()}</button>
          <button class="btn icon" data-ct-hw="${esc(v.name)}" data-ct-conn="${esc(conn)}" title="Hardware">${ic.cpu()}</button>
          <button class="btn danger icon" data-ct-del="${esc(v.name)}" data-ct-conn="${esc(conn)}" title="Delete">${ic.trash()}</button>
        </div>
      </div>`;
}

/// A rendered set of container cards' handlers. Every one acts on the card's
/// own server (`data-ct-conn`); only Open, Console and Hardware navigate.
export function bindCtCards(root: Element) {
  root.querySelectorAll<HTMLElement>("[data-dc-ct]").forEach((card) => {
    card.onclick = (e) => {
      if ((e.target as Element).closest?.("button")) return;
      selectCtRow(card.dataset.dcCtConn!, card.dataset.dcCt!);
    };
  });
  root.querySelectorAll<HTMLElement>("[data-ct-power]").forEach((b) => {
    b.onclick = (e) => {
      e.stopPropagation();
      void treeCtPower(b.dataset.ctConn!, b.dataset.ctPower!, `/${b.dataset.ctAction}`);
    };
  });
  root.querySelectorAll<HTMLElement>("[data-ct-console]").forEach((b) => {
    b.onclick = (e) => {
      e.stopPropagation();
      selectCtRow(b.dataset.ctConn!, b.dataset.ctConsole!, "console");
    };
  });
  root.querySelectorAll<HTMLElement>("[data-ct-hw]").forEach((b) => {
    b.onclick = (e) => {
      e.stopPropagation();
      selectCtRow(b.dataset.ctConn!, b.dataset.ctHw!, "hardware");
    };
  });
  root.querySelectorAll<HTMLElement>("[data-ct-del]").forEach((b) => {
    b.onclick = (e) => {
      e.stopPropagation();
      const conn = b.dataset.ctConn!;
      void deleteCtOn(conn, b.dataset.ctDel!, () => afterCtDelete(conn));
    };
  });
}

/// A server's Containers tab.
export async function loadCts() {
  if (!state.selectedConn) return;
  const conn = state.selectedConn;
  try {
    const cts = await api.get<ContainerSummary[]>(`/proxy/${conn}/containers`);
    const list = $("#cts-list");
    // The poll or a click may have moved on while the daemon answered.
    if (state.selectedConn !== conn || !list) return;
    setHtml(list, cts.length
      ? `<div class="card-grid">${cts.map((v) => ctCard(v, conn)).join("")}</div>`
      : emptyState("containers", "Create CT", "ct-create-empty"));
    const b = $("#ct-create-empty");
    if (b) b.onclick = () => void openCreateCt(conn);
    bindCtCards(list);
  } catch (e) {
    const list = $("#cts-list");
    if (list && state.selectedConn === conn) setHtml(list, `<div class="empty">${esc((e as Error).message)}</div>`);
  }
}

/// A size field with its unit picker. The number is shown in `unit` and read
/// back in `base` (the unit the daemon speaks), so the picker is presentation
/// only: changing it converts the number in place, never the size it means.
function unitInput(id: string, canonical: number, base: Unit, units: readonly Unit[], unit: Unit, minCanonical = 0): string {
  return `<div class="unit-row">
      <input id="${id}" type="number" step="any" data-base="${base}" data-min="${minCanonical}"
        min="${unitValue(minCanonical, base, unit)}" value="${unitValue(canonical, base, unit)}">
      <select id="${id}-unit" data-unit-for="${id}">${unitOptions(units, unit)}</select>
    </div>`;
}

/// Wire every `unitInput` under `root`: a unit change rewrites the number and
/// its floor in the new unit.
function bindUnitInputs(root: Element) {
  root.querySelectorAll<HTMLSelectElement>("select[data-unit-for]").forEach((sel) => {
    let prev = sel.value as Unit;
    sel.onchange = () => {
      const input = root.querySelector<HTMLInputElement>(`#${sel.dataset.unitFor}`);
      const next = sel.value as Unit;
      if (input) {
        if (input.value.trim() !== "") input.value = unitValue(Number(input.value), prev, next);
        input.min = unitValue(Number(input.dataset.min ?? 0), input.dataset.base as Unit, next);
      }
      prev = next;
    };
  });
}

/// A `unitInput`'s value in its base unit, or NaN when it is not a number.
function readUnitInput(root: Element, id: string): number {
  const input = root.querySelector<HTMLInputElement>(`#${id}`);
  const sel = root.querySelector<HTMLSelectElement>(`#${id}-unit`);
  if (!input || !sel || input.value.trim() === "") return NaN;
  return convert(Number(input.value), sel.value as Unit, input.dataset.base as Unit);
}

/// A container's root disk is sized in whole GiB.
const CT_DISK_UNITS = ["GiB", "TiB"] as const;

/// A container's resources (`GET/PUT /containers/{ct}/hardware`).
interface CtHardware {
  vcpus: number; memory_mb: number; swap_mb: number; cpu_limit_pct: number; disk_gb: number;
}

/// A container's Hardware tab: its resources, in the rows the VM editor uses.
/// Loaded once and never repainted by the poll, so what is typed stays.
export async function renderCtHardware(body: HTMLElement, conn: string, ct: string) {
  let hw: CtHardware;
  try {
    hw = await api.get<CtHardware>(`${guestBase(conn, ct, true)}/hardware`);
  } catch (e) {
    if (stillCurrentVm(conn, ct, "hardware")) setHtml(body, `<div class="empty">${esc((e as Error).message)}</div>`);
    return;
  }
  if (!stillCurrentVm(conn, ct, "hardware")) return;
  const row = (icon: string, label: string, ctl: string) => `
    <div class="hwrow">
      <span class="hwrow-ic">${icon}</span>
      <div class="hwrow-label"><b>${label}</b></div>
      <div class="hwctl">${ctl}</div>
    </div>`;
  const paint = () => {
    setHtml(body, `
      <div class="actions">
        <button class="btn primary" id="cthw-save">${ic.edit(14)} Save Hardware</button>
      </div>
      <div class="hwtable">
        ${row(ic.cpu(), "vCPUs", `<input id="cthw-cpu" type="number" min="1" step="1" value="${hw.vcpus}">`)}
        ${row(ic.quota(), "CPU limit (% of one CPU, 0 = none)", `<input id="cthw-cap" type="number" min="0" step="1" value="${hw.cpu_limit_pct}">`)}
        ${row(ic.memory(), "Memory", unitInput("cthw-mem", hw.memory_mb, "MiB", MEMORY_UNITS, MEMORY_DEFAULT, 16))}
        ${row(ic.memory(), "Swap", unitInput("cthw-swap", hw.swap_mb, "MiB", MEMORY_UNITS, MEMORY_DEFAULT, 0))}
        ${row(ic.disk(), "Root disk (grow only)", unitInput("cthw-disk", hw.disk_gb, "GiB", CT_DISK_UNITS, STORAGE_DEFAULT, hw.disk_gb))}
      </div>`);
    bindUnitInputs(body);
    $("#cthw-save", body).onclick = save;
  };
  const save = () => {
    const int = (id: string) => Math.round(Number(($(id, body) as HTMLInputElement).value));
    const next: CtHardware = {
      vcpus: int("#cthw-cpu"),
      cpu_limit_pct: int("#cthw-cap"),
      memory_mb: Math.round(readUnitInput(body, "cthw-mem")),
      swap_mb: Math.round(readUnitInput(body, "cthw-swap")),
      // Whole GiB, rounded up: a size typed in TiB must not land below it.
      disk_gb: Math.ceil(readUnitInput(body, "cthw-disk") - 1e-9),
    };
    if (Object.values(next).some((v) => !Number.isFinite(v))) return toast("Every field needs a number", "bad");
    if (next.vcpus < 1) return toast("A container needs at least one vCPU", "bad");
    if (next.disk_gb < hw.disk_gb) return toast(`The root disk can only grow (${hw.disk_gb} GiB now)`, "bad");
    const shown = displayName(treeCts[conn], ct);
    void (async () => {
      let pending: string[] = [];
      let grown = false;
      const ok = await runTask(`Update hardware for ${shown}`, async () => {
        const r = await api.put<{ pending?: string[]; disk_grown?: boolean; hardware?: CtHardware }>(
          `${guestBase(conn, ct, true)}/hardware`, next);
        pending = r?.pending ?? [];
        grown = r?.disk_grown === true;
        hw = r?.hardware ?? next;
      }, { done: "", notify: toast });
      if (!ok) return;
      // Say what is not in effect yet, rather than claim it all applied.
      toast(pending.length
        ? `Saved. ${capLabel(pending.join(", "))} take${pending.length === 1 ? "s" : ""} effect at the next start.`
        : grown ? "Hardware updated; the root disk was grown" : "Hardware updated");
      await refreshTreeVms();
      if (stillCurrentVm(conn, ct, "hardware")) paint();
    })();
  };
  paint();
}

/// A container's Options tab: its name, start-with-host, and what it is.
/// The name is a title (the container is named by its UUID, which stays), so
/// it is changed through the rename endpoint, never by rewriting the id.
export async function renderCtOptions(body: HTMLElement, conn: string, ct: string) {
  let info: ContainerSummary;
  try {
    info = await api.get<ContainerSummary>(guestBase(conn, ct, true));
  } catch (e) {
    if (stillCurrentVm(conn, ct, "options")) setHtml(body, `<div class="empty">${esc((e as Error).message)}</div>`);
    return;
  }
  if (!stillCurrentVm(conn, ct, "options")) return;
  const title = info.title || "";
  setHtml(body, `
    <div class="actions">
      <button class="btn primary" id="ctopt-save">${ic.edit(14)} Save Options</button>
    </div>
    <div class="hwtable">
      <div class="hwrow">
        <span class="hwrow-ic">${ic.edit()}</span>
        <div class="hwrow-label"><b>Name</b></div>
        <div class="hwctl"><input id="ctopt-title" value="${esc(title)}" placeholder="web-1"></div>
      </div>
      <div class="hwrow">
        <span class="hwrow-ic">${ic.autostart()}</span>
        <div class="hwrow-label"><b>Start with the Host</b></div>
        <div class="hwctl"><label class="check"><input type="checkbox" id="ctopt-auto" ${info.autostart ? "checked" : ""}> Autostart</label></div>
      </div>
      <div class="hwrow">
        <span class="hwrow-ic">${ic.key()}</span>
        <div class="hwrow-label"><b>UUID</b></div>
        <div class="hwctl hwctl-static">${esc(info.uuid ?? info.name)}</div>
      </div>
      <div class="hwrow">
        <span class="hwrow-ic">${ic.lock()}</span>
        <div class="hwrow-label"><b>Isolation</b></div>
        <div class="hwctl hwctl-static">${info.privileged ? "Privileged (host root)" : "User namespace"}</div>
      </div>
      <div class="hwrow">
        <span class="hwrow-ic">${ic.disk()}</span>
        <div class="hwrow-label"><b>Root Filesystem</b></div>
        <div class="hwctl hwctl-static">${esc(info.rootfs || "—")}</div>
      </div>
    </div>`);
  $("#ctopt-save", body).onclick = () => {
    const nextTitle = ($("#ctopt-title", body) as HTMLInputElement).value.trim();
    const nextAuto = ($("#ctopt-auto", body) as HTMLInputElement).checked;
    if (!nextTitle) return toast("Give the container a name", "bad");
    const shown = nextTitle || displayName(treeCts[conn], ct);
    void runTask(`Update options for ${shown}`, async (set) => {
      if (nextTitle !== title) {
        set("renaming");
        await api.post(`${guestBase(conn, ct, true)}/rename`, { name: nextTitle });
      }
      if (nextAuto !== !!info.autostart) {
        set(nextAuto ? "enabling autostart" : "disabling autostart");
        await api.post(`${guestBase(conn, ct, true)}/autostart`, { autostart: nextAuto });
      }
    }, { done: `Options for ${shown} saved`, notify: toast }).then(async (ok) => {
      if (!ok) return;
      await refreshTreeVms();
      if (stillCurrentVm(conn, ct, "options")) void hooks.renderVm();
    });
  };
}

/// A template from the catalog (`GET /templates/catalog`).
interface CatalogEntry {
  id: string; source: string; section: string; os: string; release: string;
  description: string; file: string;
}

/// What a catalog section is called; an unknown one is shown as it is named.
const TEMPLATE_SECTION: Record<string, string> = {
  system: "System (linuxcontainers.org)",
  turnkeylinux: "Applications (TurnKey)",
};

/// Proxmox-style template store: browse the catalog, download into a pool.
/// The download is the daemon's own (the bytes never pass through the web
/// server), tracked with the pool's other fetches.
export async function openTemplates(pool: string, conn = state.selectedConn) {
  if (!conn) return;
  const el = openModal(`Templates — ${esc(displayName(treePools[conn], pool))}`,
    `<div id="tpl-body">${skeleton("table")}</div>`, `<button class="btn" id="tpl-close">Close</button>`);
  el.querySelector(".modal")?.classList.add("wide");
  $("#tpl-close", el).onclick = () => el.remove();
  let cat: { templates: CatalogEntry[]; errors?: string[] };
  let have: VolumeSummary[] = [];
  try {
    [cat, have] = await Promise.all([
      api.get<typeof cat>(`/proxy/${conn}/templates/catalog`),
      api.get<VolumeSummary[]>(`/proxy/${conn}/pools/${encodeURIComponent(pool)}/volumes`),
    ]);
  } catch (e) {
    if (el.isConnected) setHtml($("#tpl-body", el), `<div class="empty">${esc((e as Error).message)}</div>`);
    return;
  }
  if (!el.isConnected) return;
  const templates = cat.templates ?? [];
  const errors = cat.errors ?? [];
  const sections = [...new Set(templates.map((t) => t.section))];
  let section = sections.includes("system") ? "system" : sections[0] ?? "";
  let filter = "";
  const paint = () => {
    const f = filter.toLowerCase();
    const rows = templates.filter((t) => t.section === section
      && (!f || `${t.os} ${t.release} ${t.description}`.toLowerCase().includes(f)));
    setHtml($("#tpl-body", el), `
      ${errors.length ? `<div class="dsub">Some sources did not answer: ${esc(errors.join("; "))}</div>` : ""}
      <div class="actions admin-filters">
        <label class="field"><span>Section</span><select id="tpl-sec">
          ${sections.map((sct) => `<option value="${esc(sct)}" ${sct === section ? "selected" : ""}>${esc(TEMPLATE_SECTION[sct] ?? sct)}</option>`).join("")}
        </select></label>
        <label class="field"><span>Search</span><input id="tpl-q" value="${esc(filter)}" placeholder="debian, alpine, wordpress…"></label>
      </div>
      <table class="grid">
        <tr><th>OS</th><th>Release</th><th>Description</th><th class="col-actions"></th></tr>
        ${rows.map((t, i) => {
          const got = have.some((v) => v.name === t.file);
          return `<tr><td class="tname">${esc(t.os)}</td><td>${esc(t.release)}</td><td>${esc(t.description)}</td>
            <td class="col-actions">${got ? `<span class="state on">Downloaded</span>`
              : `<button class="btn" data-tpl="${i}">${ic.download(14)} Download</button>`}</td></tr>`;
        }).join("") || `<tr><td colspan="4" class="empty">No templates match.</td></tr>`}
      </table>`);
    const sec = $("#tpl-sec", el) as HTMLSelectElement | null;
    if (sec) sec.onchange = () => { section = sec.value; paint(); };
    const q = $("#tpl-q", el) as HTMLInputElement;
    q.oninput = () => {
      filter = q.value;
      paint();
      // The repaint replaced the field: put the caret back where typing was.
      const again = $("#tpl-q", el) as HTMLInputElement | null;
      again?.focus();
      again?.setSelectionRange(filter.length, filter.length);
    };
    el.querySelectorAll<HTMLElement>("[data-tpl]").forEach((b) => {
      b.onclick = async () => {
        const t = rows[Number(b.dataset.tpl)];
        try {
          await api.post(`/proxy/${conn}/pools/${encodeURIComponent(pool)}/templates`, { id: t.id });
          toast(`Downloading ${t.os} ${t.release}: progress is on the pool page`);
          el.remove();
          void pollFetches(conn);
          if (state.selectedConn === conn && state.selectedPool === pool) void renderPoolPage();
        } catch (e) { toast((e as Error).message, "bad"); }
      };
    });
  };
  paint();
}

/// Create a container, Proxmox's CT wizard in one form: general, template,
/// disk, CPU/memory, network. `conn` is explicit (the Datacenter and the admin
/// page create on a daemon without switching the app to it); `forUser` is the
/// admin page creating it for someone, who is made its owner, with `after`
/// repainting that page. The privileged box is shown only to someone who holds
/// CT.Privileged here (the server checks the same thing).
export async function openCreateCt(conn = state.selectedConn, forUser?: { owner: string; after: () => void }) {
  if (!conn) return withDaemon((id) => void openCreateCt(id, forUser));
  let pools: PoolSummary[] = [];
  let nets: NetworkSummary[] = [];
  let canPriv = false;
  const templates: { pool: string; vol: VolumeSummary }[] = [];
  try {
    [pools, nets] = await Promise.all([
      api.get<PoolSummary[]>(`/proxy/${conn}/pools`),
      api.get<NetworkSummary[]>(`/proxy/${conn}/networks`),
    ]);
    const perm = await api.get<{ privs: string[] }>(`/auth/privs?path=${encodeURIComponent(`/nodes/${conn}/containers`)}`)
      .catch(() => ({ privs: [] as string[] }));
    canPriv = canAdmin() || perm.privs.includes("CT.Privileged");
    const lists = await Promise.all(pools.filter((p) => p.active).map(async (p) =>
      [p.name, await api.get<VolumeSummary[]>(`/proxy/${conn}/pools/${encodeURIComponent(p.name)}/volumes`).catch(() => [])] as const));
    for (const [pool, vols] of lists) {
      for (const v of vols) if (v.content === "vztmpl") templates.push({ pool, vol: v });
    }
  } catch (e) {
    return toast((e as Error).message, "bad");
  }
  const active = pools.filter((p) => p.active);
  // The first running network is the default: a container is rarely wanted
  // without one.
  const firstActive = nets.find((n) => n.active);
  const el = openModal(forUser ? "Create Container for a User" : "Create Container", `
    <div class="section-title">General</div>
    <div class="grid2">
      <label class="field"><span>Name</span><input id="ct-title" placeholder="web-1"></label>
      <label class="field"><span>Hostname (optional)</span><input id="ct-host" placeholder="from the name"></label>
      <label class="field"><span>Root Password</span><input id="ct-pass" type="password" autocomplete="new-password"></label>
      <label class="field"><span>SSH Public Key (optional)</span><input id="ct-keys" placeholder="ssh-ed25519 AAAA…"></label>
    </div>
    ${canPriv ? `<label class="check"><input type="checkbox" id="ct-priv"> Privileged container</label>` : ""}
    <div class="section-title">Template</div>
    ${templates.length ? `<label class="field"><span>Template</span><select id="ct-tpl">
      ${templates.map((t, i) => `<option value="${i}">${esc(t.vol.title || t.vol.name)} (${esc(displayName(pools, t.pool))})</option>`).join("")}
    </select></label>` : `<div class="empty">No templates yet. Open a storage pool and choose <b>Templates</b> to download one.</div>`}
    <div class="section-title">Disk</div>
    <div class="grid2">
      <label class="field"><span>Storage Pool</span><select id="ct-pool">
        ${active.map((p) => `<option value="${esc(p.name)}">${esc(p.title || p.name)}</option>`).join("")}
      </select></label>
      <label class="field"><span>Root Disk</span>${unitInput("ct-size", 8, "GiB", CT_DISK_UNITS, STORAGE_DEFAULT, 1)}</label>
    </div>
    <div class="section-title">CPU and Memory</div>
    <div class="grid2">
      <label class="field"><span>vCPUs</span><input id="ct-cpu" type="number" min="1" step="1" value="1"></label>
      <label class="field"><span>Memory</span>${unitInput("ct-mem", 512, "MiB", MEMORY_UNITS, MEMORY_DEFAULT, 16)}</label>
      <label class="field"><span>Swap</span>${unitInput("ct-swap", 512, "MiB", MEMORY_UNITS, MEMORY_DEFAULT, 0)}</label>
    </div>
    <div class="section-title">Network</div>
    <label class="field"><span>Network</span><select id="ct-net">
      <option value="">None</option>
      ${nets.map((n) => `<option value="${esc(n.name)}" ${n.active ? "" : "disabled"} ${n === firstActive ? "selected" : ""}>${esc(n.title || n.name)}${n.active ? "" : " (inactive)"}</option>`).join("")}
    </select></label>
    <label class="check"><input type="checkbox" id="ct-start" checked> Start after creating</label>
    <label class="check"><input type="checkbox" id="ct-auto"> Start with the host</label>`,
    `<button class="btn" id="ct-cancel">Cancel</button><button class="btn primary" id="ct-ok" ${templates.length ? "" : "disabled"}>${ic.plus(14)} Create</button>`);
  bindUnitInputs(el);
  const netSel = $("#ct-net", el) as HTMLSelectElement;
  $("#ct-cancel", el).onclick = () => el.remove();
  $("#ct-ok", el).onclick = () => {
    const v = (id: string) => ($(id, el) as HTMLInputElement).value.trim();
    const title = v("#ct-title");
    if (!title) return toast("Give the container a name", "bad");
    const t = templates[Number(($("#ct-tpl", el) as HTMLSelectElement).value)];
    if (!t) return toast("Choose a template", "bad");
    const pool = ($("#ct-pool", el) as HTMLSelectElement | null)?.value ?? "";
    if (!pool) return toast("Choose a storage pool for the root disk", "bad");
    const size = Math.ceil(readUnitInput(el, "ct-size") - 1e-9);
    const memory = Math.round(readUnitInput(el, "ct-mem"));
    const swap = Math.round(readUnitInput(el, "ct-swap"));
    const vcpus = Math.round(Number(v("#ct-cpu")));
    if (![size, memory, swap, vcpus].every(Number.isFinite) || size < 1 || memory < 16 || vcpus < 1) {
      return toast("Root disk, memory and vCPUs need a size", "bad");
    }
    const body = {
      title,
      hostname: v("#ct-host") || undefined,
      template: { pool: t.pool, volume: t.vol.name },
      rootfs: { pool, size_gb: size },
      memory_mb: memory,
      swap_mb: Math.max(0, swap),
      vcpus,
      networks: netSel.value ? [netSel.value] : [],
      password: v("#ct-pass") || undefined,
      ssh_keys: v("#ct-keys") || undefined,
      privileged: ($("#ct-priv", el) as HTMLInputElement | null)?.checked ?? false,
      start: ($("#ct-start", el) as HTMLInputElement).checked,
      autostart: ($("#ct-auto", el) as HTMLInputElement).checked,
    };
    el.remove();
    void runTask(`Create container ${title}`, async (set) => {
      set("unpacking the template");
      const r = await api.post<{ name: string; uuid?: string; started?: boolean }>(`/proxy/${conn}/containers`, body);
      if (forUser) {
        set("assigning owner");
        await assignOwner(conn, "ct", r.name, forUser.owner);
      }
    }, { done: `Container ${title} created`, notify: toast }).then(async (ok) => {
      if (!ok) return;
      await refreshTreeVms();
      if (forUser) forUser.after();
      else if (state.selectedConn === conn && state.connTab === "containers" && !state.selectedVm) void loadCts();
      // Created from the Datacenter (or a server that is not open): the view
      // on screen needs the repaint, and it stays where it is.
      else if (!state.selectedConn) hooks.renderDc();
    });
  };
}

/// Delete a container on `conn`. Its rootfs is the container, so it always goes
/// with it; its template stays in its pool. A running container is stopped
/// first by the daemon, which can take a few seconds, so this is a task.
/// Shared by the container's view, its cards and menu, and the admin panel.
export async function deleteCtOn(conn: string, name: string, after: () => void) {
  const shown = displayName(treeCts[conn], name);
  if (!(await askConfirm("Delete Container",
    `Delete container "${shown}" and its root filesystem? The template it was made from is kept.`, "Delete", true))) return;
  let outcome = `Container ${shown} deleted`;
  const ok = await runTask(`Delete container ${shown}`, async (set) => {
    set("Stopping the container if it is running…");
    const res = await api.del<DomainDeleteResult>(guestBase(conn, name, true));
    const failed = res?.failed ?? [];
    if (failed.length) throw new Error(`deleted, but its root filesystem could not be: ${failed.map((f) => f.disk).join(", ")}`);
    if (res?.stopped) outcome = `Container ${shown} deleted (stopped it first)`;
  }, { done: "", notify: toast });
  if (ok) toast(outcome);
  // Its own page was open: land on the list it was in, not on a dead page.
  if (ok && state.selectedConn === conn && state.selectedVm === name && state.ctMode) {
    clearResourceSelection();
    state.connTab = "containers";
  }
  await refreshTreeVms();
  after();
}

/// What to repaint once a container is gone, wherever its delete was started:
/// the server's Containers tab (the container's own page lands there too) or
/// the Datacenter.
export function afterCtDelete(conn: string) {
  if (!state.selectedConn) { hooks.renderDc(); return; }
  if (state.selectedConn !== conn || state.selectedVm || state.selectedNet || state.selectedPool) return;
  hooks.renderConn();
}

export async function deleteCt(name: string) {
  const conn = state.selectedConn;
  if (!conn) return;
  await deleteCtOn(conn, name, () => afterCtDelete(conn));
}

/// Rename a container. It is named by its UUID, so this sets its title: the
/// id -- the URL and every stored key -- never changes.
export async function renameCt(conn: string, name: string) {
  const next = await askText("Rename Container", "New name", displayName(treeCts[conn], name), "web-1");
  if (!next || next === displayName(treeCts[conn], name)) return;
  try {
    await api.post(`${guestBase(conn, name, true)}/rename`, { name: next });
    toast("Renamed");
    await refreshTreeVms();
    if (state.selectedConn === conn && state.selectedVm === name && state.ctMode) void hooks.renderVm();
  } catch (e) {
    toast((e as Error).message, "bad");
  }
}
