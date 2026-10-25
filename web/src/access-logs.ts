import { api } from "./api";
import { ic } from "./icons";
import { skeleton } from "./skeleton";
import { holdFocus } from "./focus";
import { DIR_LABEL, GROUP_LABEL, KIND_LABEL, PAGE, defaultFilter, drillDown, fmtWhen, forLive, liveApplies, logQuery, sortMark, toggleSort, withGroup } from "./access";
import type { FlowDir, Group, Kind, LogFilter } from "./access";
import { esc } from "./xml";
import { openModal } from "./dialogs";
import { $, setHtml } from "./dom";
import { busyScrolling } from "./scroll-busy";

/// Access log: every connection this server's guests made, two weeks back.
/// `vm` pre-selects one guest (the VM Network tab); from the Firewall page it
/// starts on every VM with a filter to pick one.
export function openAccessLogs(conn: string, vm = "", ct = false) {
  let f: LogFilter = defaultFilter(vm);
  let vms: string[] = [];
  const el = openModal(`Access Logs${vm ? ` — ${esc(vm)}` : ""}`, `<div id="al-body">${skeleton("table")}</div>`, "");
  el.querySelector(".modal")?.classList.add("wide", "al-modal");
  void api.get<{ vms: string[] }>(`/access-logs/vms?conn=${encodeURIComponent(conn)}`)
    .then((r) => { vms = r.vms; paint(null); }).catch(() => {});
  let data: { total: number; rows?: AccessRow[]; groups?: AccessGroup[] } | null = null;
  let gen = 0;
  // Live tail: re-read the newest page every couple of seconds while the
  // dialog is open. New rows land within the web server's 5 s pull.
  let live = false;
  let liveTimer = 0;
  const stopLive = () => { live = false; window.clearInterval(liveTimer); };
  const startLive = () => {
    live = true;
    window.clearInterval(liveTimer);
    liveTimer = window.setInterval(() => {
      if (!el.isConnected) return stopLive();
      // Same rule as the 1s poll: never rebuild the table mid-scroll.
      if (busyScrolling()) return;
      void load();
    }, 2000);
  };

  const load = async () => {
    const mine = ++gen;
    try {
      // A container's own log is checked against its container permission
      // (CT.Audit), not VM.Audit; the rows are the same.
      const ctq = ct && vm && f.vm === vm ? "&ct=true" : "";
      const r = await api.get<typeof data>(`/access-logs?${logQuery(conn, f, Date.now())}${ctq}`);
      if (mine !== gen) return; // a newer filter won
      data = r;
    } catch (e) {
      if (mine !== gen) return;
      data = null;
      return paint((e as Error).message);
    }
    paint(null);
  };

  const opt = (list: [string, string][], sel: string) =>
    list.map(([v, l]) => `<option value="${esc(v)}" ${v === sel ? "selected" : ""}>${esc(l)}</option>`).join("");
  const th = (col: string, label: string) =>
    `<th class="al-sort" data-al-sort="${col}" tabindex="0">${label}${sortMark(f, col)}</th>`;

  function paint(error: string | null) {
    const body = $("#al-body", el) as HTMLElement | null;
    if (!body) return;
    const vmList = [...new Set([...vms, ...(f.vm ? [f.vm] : [])])];
    const total = data?.total ?? 0;
    const pages = Math.max(1, Math.ceil(total / PAGE));
    let table = "";
    if (error) {
      table = `<div class="empty">${esc(error)}</div>`;
    } else if (!data) {
      table = skeleton("table");
    } else if (f.group) {
      table = `<table class="grid al-table">
        <tr>${th("key", esc(GROUP_LABEL[f.group].replace(/^(Most accessed |Most used |Busiest )/, "")))}${th("count", "Connections")}<th>VMs</th>${th("time", "Last")}</tr>
        ${(data.groups ?? []).map((g) => `
          <tr class="row-open" tabindex="0" data-al-drill="${esc(g.key)}">
            <td>${esc(g.key || "(no name)")}</td><td>${g.count}</td>
            <td>${g.vms}</td><td>${fmtWhen(g.last_ms)}</td></tr>`).join("")
          || `<tr><td colspan="4" class="empty">Nothing logged for this filter.</td></tr>`}
      </table>`;
    } else {
      table = `<table class="grid al-table">
        <tr>${th("time", "Time")}${f.vm ? "" : th("vm", "VM")}<th>Dir</th>${th("proto", "Proto")}${th("domain", "Domain")}${th("ip", "Remote")}${th("port", "Port")}</tr>
        ${(data.rows ?? []).map((r) => `
          <tr>
            <td>${fmtWhen(r.opened_ms)}</td>
            ${f.vm ? "" : `<td>${esc(r.vm ?? "—")}</td>`}
            <td class="al-dir ${r.dir === "in" ? "al-in" : "al-out"}" title="${r.dir === "in" ? "Someone reached this guest" : "The guest opened this connection"}">${r.dir === "in" ? "in" : "out"}</td>
            <td>${r.kind === "dns" ? "dns" : esc(r.proto)}</td>
            <td class="al-domain">${esc(r.domain ?? "—")}</td>
            <td><code>${esc(r.dst)}</code></td>
            <td>${r.dport || "—"}</td>
          </tr>`).join("")
          || `<tr><td colspan="${f.vm ? 6 : 7}" class="empty">Nothing logged for this filter.</td></tr>`}
      </table>`;
    }
    const restore = holdFocus(body, "[data-al-sort], [data-al-drill], select, input");
    setHtml(body, `
      <div class="al-filters">
        <label class="field"><span>VM</span><select id="al-vm">${opt([["", "All VMs"], ...vmList.map((v): [string, string] => [v, v])], f.vm)}</select></label>
        <label class="field"><span>Show</span><select id="al-group">${opt(Object.entries(GROUP_LABEL) as [string, string][], f.group)}</select></label>
        <label class="field"><span>Direction</span><select id="al-direction">${opt(Object.entries(DIR_LABEL) as [string, string][], f.direction)}</select></label>
        <label class="field"><span>Protocol</span><select id="al-proto">${opt([["", "Any"], ["tcp", "TCP"], ["udp", "UDP"], ["icmp", "ICMP"]], f.proto)}</select></label>
        <label class="field"><span>Port</span><input id="al-port" value="${esc(f.port)}" placeholder="any" inputmode="numeric"></label>
        <label class="field al-grow"><span>Domain or IP</span><input id="al-q" value="${esc(f.q)}" placeholder="github.com, 140.82."></label>
        <label class="field"><span>Period</span><select id="al-range">${opt([["1h", "Last hour"], ["24h", "Last 24 h"], ["7d", "Last 7 days"], ["14d", "Last 14 days"]], f.range)}</select></label>
        <label class="field"><span>Records</span><select id="al-kind">${opt(Object.entries(KIND_LABEL) as [string, string][], f.kind)}</select></label>
        <button class="btn al-live ${live ? "on" : ""}" id="al-live" title="Show new entries as they arrive">${ic.circleDot(14)} Live</button>
      </div>
      ${table}
      <div class="al-foot">
        <span class="dsub">${total} ${f.group ? "groups" : "entries"} · kept 14 days; oldest removed first if disk space runs low</span>
        <span class="al-pager">
          <button class="btn icon" id="al-prev" ${f.page ? "" : "disabled"} title="Newer Page">${ic.back()}</button>
          <span>${f.page + 1} / ${pages}</span>
          <button class="btn icon" id="al-next" ${f.page + 1 < pages ? "" : "disabled"} title="Older Page">${ic.forward()}</button>
          <button class="btn icon" id="al-reload" title="Refresh">${ic.reboot()}</button>
        </span>
      </div>`);
    restore();

    const set = (next: LogFilter) => {
      f = next;
      if (live && !liveApplies(f)) stopLive();
      void load();
    };
    const sel = (id: string, fn: (v: string) => LogFilter) =>
      ($(id, body) as HTMLSelectElement).addEventListener("change", (e) => set(fn((e.target as HTMLSelectElement).value)));
    sel("#al-vm", (v) => ({ ...f, vm: v, page: 0 }));
    sel("#al-group", (v) => withGroup(f, v as Group));
    sel("#al-proto", (v) => ({ ...f, proto: v, page: 0 }));
    sel("#al-direction", (v) => ({ ...f, direction: v as FlowDir, page: 0 }));
    sel("#al-range", (v) => ({ ...f, range: v, page: 0 }));
    sel("#al-kind", (v) => ({ ...f, kind: v as Kind, page: 0 }));
    ($("#al-live", body) as HTMLButtonElement).onclick = () => {
      if (live) { stopLive(); paint(null); return; }
      // Live means newest first: switch to that view, keeping the filters.
      f = forLive(f);
      startLive();
      void load();
    };
    // Typing filters as you go, after a pause, instead of on every key.
    let timer = 0;
    for (const [id, key] of [["#al-q", "q"], ["#al-port", "port"]] as const) {
      ($(id, body) as HTMLInputElement).oninput = (e) => {
        window.clearTimeout(timer);
        const v = (e.target as HTMLInputElement).value;
        timer = window.setTimeout(() => set({ ...f, [key]: v, page: 0 }), 350);
      };
    }
    body.querySelectorAll<HTMLElement>("[data-al-sort]").forEach((h) => {
      h.onclick = () => set(toggleSort(f, h.dataset.alSort!));
      h.onkeydown = (e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); h.click(); } };
    });
    body.querySelectorAll<HTMLElement>("[data-al-drill]").forEach((r) => {
      r.onclick = () => set(drillDown(f, r.dataset.alDrill!));
    });
    ($("#al-prev", body) as HTMLButtonElement).onclick = () => set({ ...f, page: Math.max(0, f.page - 1) });
    ($("#al-next", body) as HTMLButtonElement).onclick = () => set({ ...f, page: f.page + 1 });
    ($("#al-reload", body) as HTMLButtonElement).onclick = () => void load();
  }
  paint(null);
  void load();
}

interface AccessRow {
  opened_ms: number; closed_ms: number; vm: string | null; proto: string; dst: string; dport: number;
  domain: string | null; dir: FlowDir;
  kind: "dns" | "connection";
}
interface AccessGroup { key: string; count: number; vms: number; last_ms: number }
