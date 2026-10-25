import { api } from "../api";
import type { NetworkSummary } from "../api";
import { ic } from "../icons";
import { esc, networkFormFromXml, nextFreeIp } from "../xml";
import { shownNet, state, treeNets, treeVms } from "../app/state";
import { displayName, emptyState } from "../app/util";
import { openModal, toast } from "../app/dialogs";
import { deleteNetwork } from "../app/actions";
import { hooks } from "../app/hooks";
import { beginRender, stillCurrent, syncUrl } from "../app/nav";
import { $, setHtml } from "../app/dom";
import { openCreateNetwork, openEditNetwork } from "../forms/network-form";
import { ago, isOffline, renderOffline } from "../app/boot";
import { selectNetRow } from "../app/tree";
import { backToConn, leaveMissing } from "./resource-common";
import type { IpamHost, IpamInfo } from "./resource-common";

/// Dedicated network page: card-based information, an IPAM allocation list
/// (MAC -> IP DHCP hosts) and the lifecycle/edit actions.
/// A network's DHCP range, DNS forwarders and domain. Stored by this host's
/// IPAM, so saving never touches the libvirt network or its guests.
function openIpamSettings(conn: string, net: string, ipam: IpamInfo) {
  const el = openModal(`DHCP & DNS — ${esc(shownNet(net))}`, `
    <div class="grid2">
      <label class="field"><span>Range Start</span><input id="ipam-rs" value="${esc(ipam.range_start)}"></label>
      <label class="field"><span>Range End</span><input id="ipam-re" value="${esc(ipam.range_end)}"></label>
    </div>
    <label class="field"><span>DNS forwarders (blank = the host's own resolvers)</span>
      <input id="ipam-fwd" value="${esc(ipam.dns_forwarders.join(", "))}" placeholder="1.1.1.1, 9.9.9.9"></label>
    <label class="field"><span>Domain (optional)</span>
      <input id="ipam-dom" value="${esc(ipam.domain ?? "")}" placeholder="lab"></label>
`,
    `<button class="btn primary" id="ipam-set-ok">Save</button>`);
  $("#ipam-set-ok", el).onclick = async () => {
    try {
      await api.put(`/proxy/${conn}/networks/${encodeURIComponent(net)}/ipam`, {
        range_start: ($("#ipam-rs", el) as HTMLInputElement).value.trim(),
        range_end: ($("#ipam-re", el) as HTMLInputElement).value.trim(),
        dns_forwarders: ($("#ipam-fwd", el) as HTMLInputElement).value.split(/[\s,]+/).filter(Boolean),
        domain: ($("#ipam-dom", el) as HTMLInputElement).value.trim() || null,
      });
      el.remove();
      toast("DHCP & DNS settings saved");
      if (state.selectedNet === net) void renderNetPage();
    } catch (e) { toast((e as Error).message, "bad"); }
  };
}

export async function renderNetPage() {
  if (!state.selectedConn || !state.selectedNet) return;
  if (isOffline(state.selectedConn)) return renderOffline();
  const gen = beginRender();
  syncUrl();
  const conn = state.selectedConn; // narrowed copy for nested closures
  const content = $("#content");
  const name = state.selectedNet;
  try {
    const netBase = `/proxy/${state.selectedConn}/networks/${encodeURIComponent(name)}`;
    const [summary, xml, ipamState] = await Promise.all([
      api.get<NetworkSummary[]>(`/proxy/${state.selectedConn}/networks`),
      api.get<{ xml: string }>(`${netBase}/xml`),
      // Absent for a network this host does not serve (bridge mode, or no
      // IPv4 subnet): no IPAM section then.
      Promise.all([
        api.get<IpamInfo>(`${netBase}/ipam`),
        api.get<{ hosts: IpamHost[] }>(`${netBase}/hosts`),
      ]).catch(() => null),
    ]);
    // The poll may have started this render and the user then switched panes;
    // painting now would yank their view back to the network page.
    if (!stillCurrent(gen)) return;
    const net = summary.find((n) => n.name === name);
    if (!net) return leaveMissing("networks", `Network "${name}" no longer exists`);
    const form = networkFormFromXml(xml.xml);
    const ipam = ipamState?.[0] ?? null;
    const typeLabel = form.forward === "bridge" ? "Bridge" : form.forward === "nat" ? "NAT" : "Isolated";
    const gateway = form.ip ? form.ip.split("/")[0] : "—";
    const hosts = ipamState?.[1].hosts ?? [];
    const dhcpRange = ipam ? { start: ipam.range_start, end: ipam.range_end } : null;
    const remaining = ipam ? { free: ipam.range_size - ipam.range_used, total: ipam.range_size } : null;
    // Guests ask the gateway, which answers their names and forwards the rest.
    const dnsLabel = ipam
      ? `${ipam.gateway}${ipam.dns_forwarders.length ? ` → ${ipam.dns_forwarders.join(", ")}` : ""}`
      : "—";
    const hostsUrl = `${netBase}/hosts`;

    setHtml(content, `
      <div class="dhead dhead-card">
        <span class="dhead-ic">${ic.network(20)}</span>
        <span class="dtitle">${esc(displayName(treeNets[state.selectedConn!], name))}</span>
        <div class="actions" style="margin:0 0 0 auto">
          <button class="btn ${net.active ? "on" : ""}" id="net-toggle">${net.active ? ic.stop(14) : ic.start(14)} ${net.active ? "Stop" : "Start"}</button>
          <button class="btn primary" id="net-edit">${ic.edit(14)} Edit</button>
          <button class="btn" id="net-fw">${ic.firewall(14)} Firewall</button>
          <button class="btn danger" id="net-del">${ic.trash(14)} Delete</button>
        </div>
      </div>
      <div class="cards">
        <div class="card"><span class="card-ic">${ic.network(20)}</span><div><b>${esc(typeLabel)}</b><i>TYPE</i></div></div>
        <div class="card"><span class="card-ic">${ic.circleDot(20)}</span><div><b>${esc(form.ip || "—")}</b><i>SUBNET</i></div></div>
        <div class="card"><span class="card-ic">${ic.chevron(20)}</span><div><b>${esc(gateway)}</b><i>GATEWAY</i></div></div>
        <div class="card"><span class="card-ic">${ic.circle(20)}</span><div><b>${remaining ? `${remaining.free} / ${remaining.total}` : "—"}</b><i>DHCP IPS REMAINING</i></div></div>
        <div class="card"><span class="card-ic">${ic.circleDot(20)}</span><div><b>${dhcpRange ? `${esc(dhcpRange.start)} – ${esc(dhcpRange.end.split(".").pop() ?? "")}` : "—"}</b><i>DHCP RANGE</i></div></div>
        <div class="card"><span class="card-ic">${ic.options(20)}</span><div><b>${esc(dnsLabel)}</b><i>DNS</i></div></div>
      </div>

      ${ipam ? `
      <div class="metric">
      <div style="display:flex;align-items:center;justify-content:space-between;gap:10px;flex-wrap:wrap">
        <div class="section-title fw-cat" style="margin:0;padding-left:2px"><span class="cat-ic">${ic.ip()}</span>IPAM Allocations</div>
        <div class="actions" style="margin:0">
          <button class="btn" id="ipam-settings">${ic.options(14)} DHCP &amp; DNS settings</button>
          <button class="btn" id="ipam-add">${ic.plus(14)} Add Allocation</button>
        </div>
      </div>
      ${ipam.serving ? "" : `<div class="dsub" style="margin:0">Not being served right now (the network is stopped).</div>`}
      <table class="grid" id="ipam-hosts">
        <tr><th>Name</th><th>VM</th><th>MAC Address</th><th>IP Address</th><th>Last Seen</th><th></th></tr>
        ${hosts.map((h, i) => `
          <tr data-ipam-mac="${esc(h.mac)}">
            <td>${esc(h.name || "—")}</td>
            <td>${esc(h.vm_name ?? "—")}</td>
            <td>${esc(h.mac)}</td>
            <td>${esc(h.ip)}${h.dynamic ? ` <span class="state" title="Assigned from the range when the guest first asked">auto</span>` : ""}</td>
            <td>${h.seen_ms ? `${ago(h.seen_ms)} ago` : "—"}</td>
            <td class="col-actions"><div class="row-actions">
              <button class="btn icon" data-ipam-edit="${i}" title="Edit">${ic.edit()}</button>
              <button class="btn danger icon" data-ipam-del="${i}" title="Release">${ic.trash()}</button>
            </div></td>
          </tr>`).join("") || `<tr><td colspan="6" class="empty">No allocations yet — guests get one from the range when they first ask.</td></tr>`}
      </table>
      </div>` : ""}`);

    const editHost = (idx: number | null) => {
      const h = idx === null ? { mac: "", ip: "", name: "" } : hosts[idx];
      const vms = treeVms[conn] ?? [];
      const el = openModal(
        idx === null ? "Add IPAM Allocation" : "Edit IPAM Allocation",
        `<div class="grid2">
          <label class="field"><span>VM (optional)</span>
            <select id="ipam-vm">
              <option value="">Manual Entry</option>
              ${vms.map((v) => `<option value="${esc(v.name)}" ${v.name === h.name ? "selected" : ""}>${esc(v.title || v.name)}</option>`).join("")}
            </select></label>
          <label class="field" id="ipam-nic-wrap" hidden><span>Interface</span><select id="ipam-nic"><option value="">Select an Interface…</option></select></label>
          <label class="field"><span>Name (optional)</span><input id="ipam-name" value="${esc(h.name ?? "")}" placeholder="guest hostname"></label>
          <label class="field"><span>IP Address</span><input id="ipam-ip" value="${esc(h.ip)}" placeholder="192.168.100.50"></label>
          <label class="field"><span>MAC Address</span><input id="ipam-mac" value="${esc(h.mac)}" placeholder="52:54:00:aa:bb:cc"></label>
        </div>`,
        `<button class="btn primary" id="ipam-save">${idx === null ? "Add" : "Save"}</button>`,
      );

      const vmSel = $("#ipam-vm", el) as HTMLSelectElement;
      const nicSel = $("#ipam-nic", el) as HTMLSelectElement;
      const nicWrap = $("#ipam-nic-wrap", el) as HTMLElement;
      const nameInp = $("#ipam-name", el) as HTMLInputElement;
      const ipInp = $("#ipam-ip", el) as HTMLInputElement;
      const macInp = $("#ipam-mac", el) as HTMLInputElement;

      // Picking a VM offers its NIC MACs; picking one fills MAC, name and a
      // free IP from the DHCP range. Manual entry still works by hand-typing
      // the MAC.
      const pickVm = async (vmName: string) => {
        if (!vmName) {
          nicWrap.hidden = true;
          return;
        }
        try {
          const res = await api.get<{ interfaces: { mac: string | null }[] }>(
            `/proxy/${state.selectedConn}/domains/${encodeURIComponent(vmName)}/interfaces`);
          const macs = (res.interfaces ?? []).map((x) => x.mac).filter(Boolean) as string[];
          setHtml(nicSel, macs.map((m) => `<option value="${esc(m)}">${esc(m)}</option>`).join("") || `<option value="">No NIC</option>`);
          nicWrap.hidden = false;
          if (macs.length === 1) {
            nicSel.value = macs[0];
            macInp.value = macs[0];
            if (!nameInp.value) nameInp.value = vmName;
            if (!ipInp.value) {
              ipInp.value = nextFreeIp(form.ip, hosts.map((x) => x.ip), dhcpRange ?? undefined) ?? "";
            }
          }
        } catch {
          nicWrap.hidden = true;
        }
      };
      vmSel.addEventListener("change", () => void pickVm(vmSel.value));
      nicSel.addEventListener("change", () => {
        macInp.value = nicSel.value;
        if (!ipInp.value) {
          ipInp.value = nextFreeIp(form.ip, hosts.map((x) => x.ip), dhcpRange ?? undefined) ?? "";
        }
      });

      $("#ipam-save", el).onclick = async () => {
        const mac = macInp.value.trim();
        const ip = ipInp.value.trim();
        if (!mac || !ip) return toast("MAC and IP are required", "bad");
        try {
          // Editing an allocation to a different MAC is a move: drop the old.
          const old = idx === null ? null : hosts[idx];
          if (old && old.mac !== mac.toLowerCase()) await api.del(`${hostsUrl}/${encodeURIComponent(old.mac)}`);
          await api.put(hostsUrl, { mac, ip, name: nameInp.value.trim() || undefined });
          el.remove();
          toast("Allocation saved — the guest gets it at its next DHCP exchange");
          void renderNetPage();
        } catch (e) { toast((e as Error).message, "bad"); }
      };
    };

    $("#net-toggle", content).onclick = async () => {
      await netAction(net.active ? "stop" : "start", name);
      void renderNetPage();
    };
    $("#net-edit", content).onclick = () => openEditNetwork(name);
    $("#net-fw", content).onclick = () => hooks.openFirewallEditor(name);
    $("#net-del", content).onclick = async () => {
      await deleteNetwork(name);
      backToConn();
    };
    const addBtn = $("#ipam-add", content);
    if (addBtn) addBtn.onclick = () => editHost(null);
    const setBtn = $("#ipam-settings", content);
    if (setBtn && ipam) setBtn.onclick = () => openIpamSettings(conn, name, ipam);
    content.querySelectorAll("[data-ipam-edit]").forEach((b: Element) => {
      (b as HTMLElement).onclick = () => editHost(+(b as HTMLElement).dataset.ipamEdit!);
    });
    content.querySelectorAll("[data-ipam-del]").forEach((b: Element) => {
      (b as HTMLElement).onclick = async () => {
        const h = hosts[+(b as HTMLElement).dataset.ipamDel!];
        if (!h) return;
        try {
          await api.del(`${hostsUrl}/${encodeURIComponent(h.mac)}`);
          toast(h.vm_name
            ? `Released — ${h.vm_name} gets a new address from the range at its next DHCP`
            : "Allocation released");
          void renderNetPage();
        } catch (e) { toast((e as Error).message, "bad"); }
      };
    });
  } catch (e) {
    if (!stillCurrent(gen)) return;
    // The XML read fails first when the network is gone (a bookmark, or it was
    // deleted elsewhere), and the daemon reports that as a 500: ask the list.
    const nets = await api.get<NetworkSummary[]>(`/proxy/${conn}/networks`).catch(() => null);
    if (!stillCurrent(gen)) return;
    if (nets && !nets.some((n) => n.name === name)) {
      return leaveMissing("networks", `Network "${name}" no longer exists`);
    }
    setHtml(content, `<div class="empty">${esc((e as Error).message)}</div>`);
  }
}

/// One network card. `conn` is the owning server, carried on the card because
/// the Datacenter tab lists networks from every server beside each other: each
/// card's actions have to name their own, not the app's selected one.
export function netCard(n: NetworkSummary, conn: string, server?: string): string {
  const typeLabel = n.forward === "bridge" ? "Bridge" : n.forward === "nat" ? "NAT" : "Isolated";
  const gateway = n.ip ? n.ip.split("/")[0] : "";
  return `
      <div class="storage-card net-card ${n.active ? "ok" : ""}" data-open tabindex="0" data-net-open="${esc(n.name)}" data-net-conn="${esc(conn)}">
        <div class="storage-card-head">
          <span class="storage-card-icon">${ic.network()}</span>
          <span class="storage-card-title">${esc(n.title || n.name)}</span>
          ${server ? `<span class="badge">${esc(server)}</span>` : ""}
          <span class="badge">${esc(typeLabel)}</span>
          <span class="badge ${n.active ? "on" : ""}">${n.active ? "Active" : "Inactive"}</span>
        </div>
        <div class="specs">
          <div class="spec"><b>Subnet</b><span>${n.ip ? esc(n.ip) : "—"}</span></div>
          <div class="spec"><b>Gateway</b><span>${gateway ? esc(gateway) : "—"}</span></div>
        </div>
        <div class="storage-card-actions net-actions">
          <button class="btn icon ${n.active ? "on" : ""}" data-net-toggle="${esc(n.name)}" data-net-conn="${esc(conn)}" data-net-action="${n.active ? "stop" : "start"}" title="${n.active ? "Stop" : "Start"}">${n.active ? ic.stop() : ic.start()}</button>
          <button class="btn icon" data-net-edit="${esc(n.name)}" data-net-conn="${esc(conn)}" title="Edit">${ic.edit()}</button>
          <button class="btn danger icon" data-delnet="${esc(n.name)}" data-delnet-conn="${esc(conn)}" title="Delete">${ic.trash()}</button>
        </div>
      </div>`;
}

export async function loadNetworks() {
  if (!state.selectedConn) return;
  try {
    const nets = await api.get<NetworkSummary[]>(`/proxy/${state.selectedConn}/networks`);
    const list = $("#nets-list");
    if (!list) return;
    const conn = state.selectedConn;
    setHtml(list, nets.map((n) => netCard(n, conn)).join("") || emptyState("networks", "Create Network", "net-create-empty"));
    const createBtn = $("#net-create-empty");
    if (createBtn) createBtn.onclick = () => openCreateNetwork();
    // The whole card opens the network's page; its buttons stop propagation.
    list.querySelectorAll("[data-net-open]").forEach((card: Element) => {
      (card as HTMLElement).onclick = (e) => {
        if ((e.target as Element).closest?.("button")) return;
        selectNetRow(state.selectedConn!, (card as HTMLElement).dataset.netOpen!);
      };
    });
    list.querySelectorAll("[data-net-toggle]").forEach((b: Element) => {
      (b as HTMLElement).onclick = (e) => {
        e.stopPropagation();
        const el = b as HTMLElement;
        void netActionOn(el.dataset.netConn!, el.dataset.netAction === "start" ? "start" : "stop", el.dataset.netToggle!);
      };
    });
    list.querySelectorAll("[data-net-edit]").forEach((b: Element) => {
      (b as HTMLElement).onclick = (e) => {
        e.stopPropagation();
        const el = b as HTMLElement;
        state.selectedConn = el.dataset.netConn!;
        void openEditNetwork(el.dataset.netEdit!);
      };
    });
    list.querySelectorAll("[data-delnet]").forEach((b: Element) => {
      (b as HTMLElement).onclick = (e) => {
        e.stopPropagation();
        const el = b as HTMLElement;
        state.selectedConn = el.dataset.delnetConn!;
        void deleteNetwork(el.dataset.delnet!);
      };
    });
  } catch (e) {
    const list = $("#nets-list");
    if (list) setHtml(list, `<div class="empty">${esc((e as Error).message)}</div>`);
  }
}

export async function netAction(action: "start" | "stop", name: string) {
  if (!state.selectedConn) return;
  await netActionOn(state.selectedConn, action, name);
}

async function netActionOn(conn: string, action: "start" | "stop", name: string) {
  try {
    await api.post(`/proxy/${conn}/networks/${encodeURIComponent(name)}/${action}`);
    if (state.selectedConn === conn) await loadNetworks();
  } catch (e) {
    toast((e as Error).message, "bad");
  }
}
