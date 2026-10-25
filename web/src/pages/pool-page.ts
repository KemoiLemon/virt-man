import { api } from "../api";
import type { PoolSummary, VolumeSummary } from "../api";
import { ic } from "../icons";
import { poolUsed } from "../pool-usage";
import { fmtBytes } from "../stats";
import { esc } from "../xml";
import { state, treePools, volumeNames } from "../app/state";
import { displayName, emptyState } from "../app/util";
import { deletePool, deleteVolume, renameVolume } from "../app/actions";
import { hooks } from "../app/hooks";
import { beginRender, stillCurrent, syncUrl } from "../app/nav";
import { bindFetches, bindUploads, fetchesSection, openFetch, openUpload, pollFetches, pollUploads, uploadsSection } from "./transfers";
import { $, setHtml } from "../app/dom";
import { openCreatePool, openCreateVolume } from "../forms/create-resource";
import { isOffline, renderOffline } from "../app/boot";
import { selectPoolRow } from "../app/tree";
import { backToConn, leaveMissing } from "./resource-common";

/// Dedicated storage page: one pool's capacity plus full volume management.
/// Proxmox's words for what a volume holds.
const CONTENT_LABEL: Record<string, string> = { iso: "ISO image", vztmpl: "CT template", images: "Disk image" };

export async function renderPoolPage() {
  if (!state.selectedConn || !state.selectedPool) return;
  if (isOffline(state.selectedConn)) return renderOffline();
  const gen = beginRender();
  syncUrl();
  const content = $("#content");
  const name = state.selectedPool;
  const conn = state.selectedConn;
  try {
    // The single-pool read refreshes the pool before answering, so these
    // figures follow the filesystem; the all-pools list is served from
    // libvirt's cache and would show what was true when the page opened.
    const [p, vols] = await Promise.all([
      api.get<PoolSummary>(`/proxy/${state.selectedConn}/pools/${encodeURIComponent(name)}`),
      api.get<VolumeSummary[]>(`/proxy/${state.selectedConn}/pools/${encodeURIComponent(name)}/volumes`),
    ]);
    for (const v of vols) volumeNames[`${name}/${v.name}`] = v.title || v.name;
    // Same guard as the network page: never paint over a newer view.
    if (!stillCurrent(gen)) return;
    if (!p) return leaveMissing("storage", `Pool "${name}" no longer exists`);
    const used = poolUsed(p);
    const cap = p.capacity ?? 0;
    const usedPct = cap > 0 ? Math.max(0, Math.min(100, (used / cap) * 100)) : 0;

    setHtml(content, `
      <div class="dhead dhead-card">
        <span class="dhead-ic">${ic.storage(20)}</span>
        <span class="dtitle">${esc(displayName(treePools[state.selectedConn!], name))}</span>
        <div class="actions" style="margin:0 0 0 auto">
          <button class="btn" id="pool-upload">${ic.upload(14)} Upload ISO</button>
          <button class="btn" id="pool-fetch">${ic.download(14)} From URL</button>
          <button class="btn" id="pool-newvol">${ic.plus(14)} New Volume</button>
          <button class="btn" id="pool-tpl">${ic.container(14)} Templates</button>
          <button class="btn danger" id="pool-del">${ic.trash(14)} Delete Pool</button>
        </div>
      </div>
      <div class="metric">
        <div class="section-title fw-cat" style="margin:0"><span class="cat-ic">${ic.storage()}</span>Usage</div>
        <div class="meter"><div class="meter-fill ${usedPct > 85 ? "hot" : ""}" style="width:${usedPct.toFixed(1)}%"></div></div>
        <div class="metric-foot">
          <span>${fmtBytes(used)} used</span>
          <span>${fmtBytes(p.available)} free</span>
          <span>${fmtBytes(cap)} total</span>
        </div>
      </div>
      <div class="metric" id="pool-transfers" style="margin-top:18px">
        <div class="section-title fw-cat" style="margin:0"><span class="cat-ic">${ic.bidir()}</span>Transfers</div>
        <div id="pool-uploads">${uploadsSection(name)}</div>
        <div id="pool-fetches">${fetchesSection(conn, name)}</div>
        <div class="xfer" id="pool-no-transfers"${uploadsSection(name) || fetchesSection(conn, name) ? "hidden" : ""}>No current transfers</div>
      </div>
      <div class="metric" style="margin-top:18px">
      <div class="section-title fw-cat" style="margin:0"><span class="cat-ic">${ic.disk()}</span>Volumes</div>
      <table class="grid">
        <tr><th>Volume</th><th>Content</th><th>Size</th><th>Allocated</th><th class="col-actions"></th></tr>
        ${vols.map((v) => `
          <tr><td>${esc(v.title || v.name)}</td><td>${esc(CONTENT_LABEL[v.content ?? "images"] ?? "Disk image")}</td>
          <td>${fmtBytes(v.capacity)}</td><td>${fmtBytes(v.allocation)}</td>
          <td class="col-actions"><div class="row-actions">
            <button class="btn icon" data-pvol-title="${esc(v.name)}" title="Edit Nickname">${ic.edit()}</button>
            <button class="btn danger icon" data-pvol-del="${esc(v.name)}">${ic.trash()}</button>
          </div></td></tr>`).join("") || `<tr><td colspan="5" class="empty">No volumes.</td></tr>`}
      </table>
      </div>`);

    $("#pool-upload", content).onclick = () => openUpload(name);
    $("#pool-fetch", content).onclick = () => openFetch(name);
    $("#pool-newvol", content).onclick = () => openCreateVolume(name);
    $("#pool-tpl", content).onclick = () => void hooks.openTemplates(name);
    $("#pool-del", content).onclick = async () => {
      await deletePool(name);
      backToConn();
    };
    content.querySelectorAll("[data-pvol-title]").forEach((b: Element) => {
      (b as HTMLElement).onclick = () => void renameVolume(name, (b as HTMLElement).dataset.pvolTitle!).then(() => renderPoolPage());
    });
    content.querySelectorAll("[data-pvol-del]").forEach((b: Element) => {
      (b as HTMLElement).onclick = () => {
        void deleteVolume(name, (b as HTMLElement).dataset.pvolDel!).then(() => renderPoolPage());
      };
    });
    bindUploads();
    bindFetches(conn);
    void pollFetches(conn);
    // Refresh the transfers section on entry: the cached list may predate this
    // visit, and a transfer that finished meanwhile should not look stuck.
    void pollUploads();
  } catch (e) {
    if (!stillCurrent(gen)) return;
    // Same as the network page: a missing pool reads as a 500 from the daemon.
    const pools = await api.get<PoolSummary[]>(`/proxy/${conn}/pools`).catch(() => null);
    if (!stillCurrent(gen)) return;
    if (pools && !pools.some((x) => x.name === name)) {
      return leaveMissing("storage", `Pool "${name}" no longer exists`);
    }
    setHtml(content, `<div class="empty">${esc((e as Error).message)}</div>`);
  }
}

/// One pool card. `conn` is the owning server, carried on the card for the
/// same reason a network card carries it: the Datacenter lists every server's
/// pools at once. `volumeTable` is for the server's own Storage tab, which
/// expands a pool's volumes underneath its card.
export function poolCard(p: PoolSummary, conn: string, opts: { server?: string; volumeTable?: boolean } = {}): string {
  const used = poolUsed(p);
  const cap = p.capacity ?? 0;
  const usedPct = cap > 0 ? Math.max(0, Math.min(100, (used / cap) * 100)) : 0;
  return `
      <div class="storage-card ${p.active ? "ok" : ""}" data-open tabindex="0" data-pool-open="${esc(p.name)}" data-pool-card="${cssId(p.name)}" data-pool-conn="${esc(conn)}">
        <div class="storage-card-head">
          <span class="storage-card-icon">${ic.storage()}</span>
          <span class="storage-card-title" title="${esc(p.title || p.name)}">${esc(p.title || p.name)}</span>
          ${opts.server ? `<span class="badge">${esc(opts.server)}</span>` : ""}
          <span class="badge" data-pool-active>${p.active ? "Active" : "Inactive"}</span>
        </div>
        <div class="metric-value" data-pool-pct>${usedPct.toFixed(usedPct >= 10 ? 0 : 1)}%</div>
        <div class="meter"><div class="meter-fill ${usedPct > 85 ? "hot" : ""}" data-pool-fill style="width:${usedPct.toFixed(1)}%"></div></div>
        <div class="metric-foot">
          <span data-pool-used>${fmtBytes(used)} used</span>
          <span data-pool-free>${fmtBytes(p.available)} free</span>
          <span data-pool-cap>${fmtBytes(cap)} total</span>
        </div>
        <div class="storage-card-actions">
          <button class="btn" data-vols="${esc(p.name)}" data-vols-conn="${esc(conn)}">${ic.disk(14)} Volumes</button>
          <button class="btn" data-upload="${esc(p.name)}" data-upload-conn="${esc(conn)}">${ic.upload(14)} Upload ISO</button>
          <button class="btn danger icon" data-delpool="${esc(p.name)}" data-delpool-conn="${esc(conn)}">${ic.trash()}</button>
        </div>
      </div>${opts.volumeTable ? `<div id="vols-${cssId(p.name)}" style="margin-bottom:14px"></div>` : ""}`;
}

export async function loadPools() {
  if (!state.selectedConn) return;
  try {
    const pools = await api.get<PoolSummary[]>(`/proxy/${state.selectedConn}/pools`);
    const list = $("#pools-list");
    if (!list) return;
    const names = pools.map((p) => p.name);
    const shapeChanged = state.renderedPools?.join("|") !== names.join("|");
    if (shapeChanged) {
      state.renderedPools = names;
      // A full rebuild wipes the expanded pool's volume table, so the card is
      // the same builder the Datacenter uses and the table is a sibling.
      setHtml(list, pools.map((p) => poolCard(p, state.selectedConn!, { volumeTable: true })).join("")
        || emptyState("storage pools", "Create Pool", "pool-create-empty"));
      $("#pool-create-empty")?.addEventListener("click", () => openCreatePool());
      // The whole card opens the pool's page; its buttons keep their own jobs.
      list.querySelectorAll("[data-pool-open]").forEach((card: Element) => {
        (card as HTMLElement).onclick = (e) => {
          if ((e.target as Element).closest?.("button")) return;
          selectPoolRow(state.selectedConn!, (card as HTMLElement).dataset.poolOpen!);
        };
      });
      list.querySelectorAll("[data-vols]").forEach((b: Element) => {
        (b as HTMLElement).onclick = () => {
          const pool = (b as HTMLElement).dataset.vols!;
          // Toggle: clicking the open pool collapses it, otherwise expand it.
          state.volumesOpen = state.volumesOpen === pool ? null : pool;
          loadVolumes(pool);
        };
      });
      list.querySelectorAll("[data-upload]").forEach((b: Element) => {
        (b as HTMLElement).onclick = () => openUpload((b as HTMLElement).dataset.upload!);
      });
      list.querySelectorAll("[data-delpool]").forEach((b: Element) => {
        (b as HTMLElement).onclick = () => deletePool((b as HTMLElement).dataset.delpool!);
      });
      // A full rebuild wiped the expanded pool's table; restore it in place.
      if (state.volumesOpen) void loadVolumes(state.volumesOpen);
      return;
    }

    // Same pools as before: update the dynamic numbers in place, leaving the
    // volume table untouched so it does not flicker every poll tick.
    for (const p of pools) {
      const card = $(`[data-pool-card="${cssId(p.name)}"]`, list);
      if (!card) continue;
      const used = poolUsed(p);
      const cap = p.capacity ?? 0;
      const usedPct = cap > 0 ? Math.max(0, Math.min(100, (used / cap) * 100)) : 0;
      const fill = card.querySelector("[data-pool-fill]");
      if (fill) {
        (fill as HTMLElement).style.width = `${usedPct.toFixed(1)}%`;
        fill.classList.toggle("hot", usedPct > 85);
      }
      const pctEl = card.querySelector("[data-pool-pct]");
      if (pctEl) pctEl.textContent = `${usedPct.toFixed(usedPct >= 10 ? 0 : 1)}%`;
      const usedEl = card.querySelector("[data-pool-used]");
      if (usedEl) usedEl.textContent = `${fmtBytes(used)} used`;
      const freeEl = card.querySelector("[data-pool-free]");
      if (freeEl) freeEl.textContent = `${fmtBytes(p.available)} free`;
      const capEl = card.querySelector("[data-pool-cap]");
      if (capEl) capEl.textContent = `${fmtBytes(cap)} total`;
      const badge = card.querySelector("[data-pool-active]");
      if (badge) {
        badge.textContent = p.active ? "Active" : "Inactive";
        badge.classList.toggle("on", p.active);
      }
      card.classList.toggle("ok", p.active);
    }
  } catch (e) {
    const list = $("#pools-list");
    if (list) setHtml(list, `<div class="empty">${esc((e as Error).message)}</div>`);
  }
}

export async function loadVolumes(pool: string) {
  if (!state.selectedConn) return;
  const target = $(`#vols-${cssId(pool)}`);
  if (!target) return;
  // Collapsed: clear the table and stop.
  if (state.volumesOpen !== pool) {
    setHtml(target, "");
    return;
  }
  try {
    const vols = await api.get<VolumeSummary[]>(`/proxy/${state.selectedConn}/pools/${encodeURIComponent(pool)}/volumes`);
    for (const v of vols) volumeNames[`${pool}/${v.name}`] = v.title || v.name;
setHtml(target, `<table class="grid">
      <tr><th>Volume</th><th>Size</th><th>Allocated</th><th class="col-actions"></th></tr>
      ${vols.map((v) => `
        <tr><td>${esc(v.title || v.name)}</td><td>${fmtBytes(v.capacity)}</td><td>${fmtBytes(v.allocation)}</td>
        <td class="col-actions"><div class="row-actions">
          <button class="btn icon" data-vol-title="${esc(v.name)}" title="Edit Nickname">${ic.edit()}</button>
          <button class="btn danger icon" data-delvol="${esc(v.name)}">${ic.trash()}</button>
        </div></td></tr>`).join("") || `<tr><td colspan="4" class="empty">No volumes.</td></tr>`}
    </table>
    <div class="actions"><button class="btn" id="create-vol" data-pool="${esc(pool)}">New Volume</button></div>`);
    target.querySelectorAll("[data-vol-title]").forEach((b: Element) => {
      (b as HTMLElement).onclick = () => void renameVolume(pool, (b as HTMLElement).dataset.volTitle!).then(() => loadVolumes(pool));
    });
    target.querySelectorAll("[data-delvol]").forEach((b: Element) => {
      (b as HTMLElement).onclick = () => deleteVolume(pool, (b as HTMLElement).dataset.delvol!);
    });
    const cv = target.querySelector("#create-vol");
    if (cv) (cv as HTMLElement).onclick = () => openCreateVolume(pool);
  } catch (e) {
setHtml(target, `<div class="empty">${esc((e as Error).message)}</div>`);
  }
}

function cssId(s: string): string {
  return s.replace(/[^a-zA-Z0-9]/g, "_");
}
