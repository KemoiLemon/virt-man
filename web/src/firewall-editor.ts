import { api } from "./api";
import type { FirewallForward, FirewallPolicy, FirewallRule, NetworkSummary } from "./api";
import { ic } from "./icons";
import { skeleton } from "./skeleton";
import { attachSuggest } from "./suggest";
import { esc } from "./xml";
import { shownNet, state, treeVms } from "./state";
import { displayName } from "./util";
import { openModal, toast } from "./dialogs";
import { $, setHtml } from "./dom";
import { openAccessLogs } from "./access-logs";
import type { IpamHost, IpamInfo } from "./resource-common";

/// Rules + port-forward editor for one network.
/// Shared firewall-editor helpers: option lists, row templates, and reading
/// rows back from the DOM before a re-render.
export const FW_DIRS = ["in", "out", "forward"];
export const FW_ACTIONS = ["accept", "drop", "reject"];
export const FW_PROTOS = ["tcp", "udp", "icmp", "any"];
export const FW_ACTION_LABELS: Record<string, string> = { accept: "Accept", drop: "Drop", reject: "Reject" };

export function fwOpt(list: string[], sel: string, labels?: Record<string, string>): string {
  return list.map((v) => `<option value="${v}" ${v === sel ? "selected" : ""}>${esc(labels?.[v] ?? v)}</option>`).join("");
}

/// IP addresses worth suggesting in the firewall's source/dest fields: every
/// network's gateway, its DHCP endpoints, and each static allocation. Fetched
/// once per editor open and read lazily by the suggesters, so the list can
/// arrive after the rule table has already painted.
export async function collectIpCandidates(conn: string): Promise<string[]> {
  const nets = await api.get<NetworkSummary[]>(`/proxy/${conn}/networks`).catch(() => []);
  if (!Array.isArray(nets)) return [];
  // Each network contributes the addresses a rule might name; the callback
  // returns them rather than adding to an outer Set, so every arm of it
  // returns a value (array-callback-return).
  const found = await Promise.all(nets.map(async (n) => {
    const base = `/proxy/${conn}/networks/${encodeURIComponent(n.name)}`;
    const [ipam, hosts] = await Promise.all([
      api.get<IpamInfo>(`${base}/ipam`).catch(() => null),
      api.get<{ hosts: IpamHost[] }>(`${base}/hosts`).catch(() => null),
    ]);
    const addrs = (hosts?.hosts ?? []).map((h) => h.ip);
    if (ipam) addrs.push(ipam.gateway, ipam.range_start, ipam.range_end);
    return addrs;
  }));
  return [...new Set(found.flat())].sort();
}

/// Offer IP suggestions on a rule table's source/dest inputs. Called after
/// every render: the inputs are recreated each time, so listeners never stack.
export function attachRuleIpSuggest(root: Element, candidates: () => string[]) {
  root.querySelectorAll("[data-r-src], [data-r-dst]").forEach((n: Element) => {
    attachSuggest(n as HTMLInputElement, candidates);
  });
}

export function fwRuleRow(r: FirewallRule): string {
  return `
    <tr>
      <td><select data-r-dir>${fwOpt(FW_DIRS, r.direction)}</select></td>
      <td><select data-r-action>${fwOpt(FW_ACTIONS, r.action, FW_ACTION_LABELS)}</select></td>
      <td><select data-r-proto>${fwOpt(FW_PROTOS, r.proto)}</select></td>
      <td><input data-r-src placeholder="any" value="${esc(r.src_ip)}"></td>
      <td><input data-r-dst placeholder="any" value="${esc(r.dst_ip)}"></td>
      <td><input data-r-sport placeholder="any" value="${esc(r.src_port)}"></td>
      <td><input data-r-dport placeholder="any" value="${esc(r.dst_port)}"></td>
      <td><button class="btn danger icon" data-r-del title="Remove">${ic.trash()}</button></td>
    </tr>`;
}

export function fwFwdRow(f: FirewallForward, hosts: IpamHost[], label: (h: IpamHost) => string): string {
  // One port each side, not a range: DNAT maps a port to a port, and a range
  // had no single internal counterpart.
  //
  // The internal address is picked from the guests this network hands one to,
  // so a forward names the guest rather than an address someone has to look up.
  // An address no allocation knows (a forward saved before its guest got one,
  // or a host outside the app) stays selectable: dropping it would silently
  // point the forward at whichever guest happened to be first.
  const known = hosts.some((h) => h.ip === f.lan_ip);
  const current = f.lan_ip && !known
    ? `<option value="${esc(f.lan_ip)}" selected>${esc(f.lan_ip)} — not a guest</option>`
    : "";
  const placeholder = f.lan_ip ? "" : `<option value="" selected>Select a Guest…</option>`;
  return `
    <tr>
      <td><input data-f-wanip placeholder="0.0.0.0 = any" value="${f.wan_ip === "0.0.0.0" ? "" : esc(f.wan_ip)}"></td>
      <td><select data-f-proto>${fwOpt(["tcp", "udp"], f.proto)}</select></td>
      <td><input data-f-wport type="number" min="1" max="65535" value="${f.wan_port}"></td>
      <td><select data-f-lanip>
        ${placeholder}${current}
        ${hosts.map((h) => `<option value="${esc(h.ip)}" ${h.ip === f.lan_ip ? "selected" : ""}>${esc(label(h))} — ${esc(h.ip)}</option>`).join("")}
      </select></td>
      <td><input data-f-lport type="number" min="1" max="65535" value="${f.lan_port}"></td>
      <td><button class="btn danger icon" data-f-del title="Remove">${ic.trash()}</button></td>
    </tr>`;
}

export function blankRule(): FirewallRule {
  return { direction: "in", action: "accept", proto: "any", src_ip: "", dst_ip: "", src_port: "", dst_port: "" };
}

export function blankPolicy(): FirewallPolicy {
  return { in: "accept", out: "accept", forward: "accept" };
}

/// Boot devices a guest may try, and what to call them. The order they are
/// tried in is the document order of the `<os>` `<boot dev=.../>` elements,
/// which is how libvirt reads them. Defined in `hardware.ts` beside the code
/// that resolves them, so the dropdown and the list cannot disagree.

/// Three dropdowns for the per-direction default policies, in one row.
export function fwPolicyRow(p: FirewallPolicy): string {
  return `
    <div class="fw-policy">
      <label class="field"><span>In</span><select data-pol-in>${fwOpt(FW_ACTIONS, p.in, FW_ACTION_LABELS)}</select></label>
      <label class="field"><span>Out</span><select data-pol-out>${fwOpt(FW_ACTIONS, p.out, FW_ACTION_LABELS)}</select></label>
      <label class="field"><span>Forward</span><select data-pol-forward>${fwOpt(FW_ACTIONS, p.forward, FW_ACTION_LABELS)}</select></label>
    </div>`;
}

export function readFwPolicy(root: Element): FirewallPolicy {
  const v = (sel: string) => (root.querySelector(sel) as HTMLSelectElement)?.value as FirewallPolicy["in"];
  return {
    in: v("[data-pol-in]") ?? "accept",
    out: v("[data-pol-out]") ?? "accept",
    forward: v("[data-pol-forward]") ?? "accept",
  };
}

export function readFwRuleRows(root: Element): FirewallRule[] {
  return [...root.querySelectorAll("[data-r-dir]")].map((s) => {
    const tr = (s as HTMLElement).closest("tr")!;
    return {
      direction: (tr.querySelector("[data-r-dir]") as HTMLSelectElement).value as FirewallRule["direction"],
      action: (tr.querySelector("[data-r-action]") as HTMLSelectElement).value as FirewallRule["action"],
      proto: (tr.querySelector("[data-r-proto]") as HTMLSelectElement).value as FirewallRule["proto"],
      src_ip: (tr.querySelector("[data-r-src]") as HTMLInputElement).value,
      dst_ip: (tr.querySelector("[data-r-dst]") as HTMLInputElement).value,
      src_port: (tr.querySelector("[data-r-sport]") as HTMLInputElement).value,
      dst_port: (tr.querySelector("[data-r-dport]") as HTMLInputElement).value,
    };
  });
}

export function readFwFwdRows(root: Element): FirewallForward[] {
  return [...root.querySelectorAll("[data-f-lanip]")].map((s) => {
    const tr = (s as HTMLElement).closest("tr")!;
    const num = (sel: string) => {
      const v = +((tr.querySelector(sel) as HTMLInputElement)?.value ?? "");
      return Number.isFinite(v) ? v : 1;
    };
    return {
      proto: (tr.querySelector("[data-f-proto]") as HTMLSelectElement).value as FirewallForward["proto"],
      wan_ip: (tr.querySelector("[data-f-wanip]") as HTMLInputElement).value || "0.0.0.0",
      wan_port: num("[data-f-wport]"),
      lan_ip: (tr.querySelector("[data-f-lanip]") as HTMLSelectElement).value,
      lan_port: num("[data-f-lport]"),
    };
  });
}

/// Network firewall editor: rules and NAT port forwards, in a wide modal.
export function openFirewallEditor(netName: string) {
  if (!state.selectedConn) return;
  const conn = state.selectedConn;
  let rules: FirewallRule[] = [];
  let forwards: FirewallForward[] = [];
  let policy: FirewallPolicy = blankPolicy();
  // The addresses this network hands out, with the guest each belongs to, so a
  // forward picks a guest from a list instead of an IP typed from memory.
  let lanHosts: IpamHost[] = [];
  const lanLabel = (h: IpamHost) =>
    (h.vm_name ? displayName(treeVms[conn], h.vm_name) : "") || h.name || h.mac;
  // Every address this daemon knows about, for the source/dest fields.
  // Filled after the modal paints; the suggesters read it lazily per keystroke.
  let ipCandidates: string[] = [];
  void collectIpCandidates(conn).then((ips) => { ipCandidates = ips; });

  const el = openModal(
    `Firewall — ${esc(shownNet(netName))}`,
    `<div id="fw-body">${skeleton("table")}</div>`,
    `<button class="btn" id="fw-cancel">Cancel</button>
     <button class="btn primary" id="fw-save">Apply</button>`,
  );
  el.querySelector(".modal")?.classList.add("wide");

  const read = () => {
    rules = readFwRuleRows(el);
    forwards = readFwFwdRows(el);
    policy = readFwPolicy(el);
  };

  function render() {
    setHtml($("#fw-body", el), `
      <div class="section-title fw-cat"><span class="cat-ic">${ic.firewall()}</span>Default Policies</div>
      ${fwPolicyRow(policy)}

      <div class="section-title fw-cat"><span class="cat-ic">${ic.options()}</span>Rules</div>
      <table class="grid">
        <tr><th>Direction</th><th>Action</th><th>Proto</th><th>Source IP</th><th>Dest IP</th><th>Source Port</th><th>Dest Port</th><th></th></tr>
        ${rules.map(fwRuleRow).join("") || `<tr><td colspan="8" class="empty">No rules — traffic is accepted.</td></tr>`}
      </table>
      <div class="actions">
        <button class="btn" id="fw-add-rule">${ic.plus(14)} Add Rule</button>
        <button class="btn al-open" id="fw-logs">${ic.logs(14)} Access Logs</button>
      </div>

      <div class="section-title fw-cat" style="margin-top:18px"><span class="cat-ic">${ic.network()}</span>NAT Port Forwards</div>
      <table class="grid">
        <tr><th>WAN</th><th>Proto</th><th>WAN Port</th><th>Internal Guest</th><th>Internal Port</th><th></th></tr>
        ${forwards.map((f) => fwFwdRow(f, lanHosts, lanLabel)).join("") || `<tr><td colspan="6" class="empty">No port forwards.</td></tr>`}
      </table>
      <div class="actions"><button class="btn" id="fw-add-fwd">${ic.plus(14)} Add Forward</button></div>`);

    // Re-attached every render: render() rebuilds these inputs from scratch.
    attachRuleIpSuggest(el, () => ipCandidates);

    $("#fw-logs", el).onclick = () => openAccessLogs(conn);
    $("#fw-add-rule", el).onclick = () => {
      read();
      rules.push(blankRule());
      render();
    };
    $("#fw-add-fwd", el).onclick = () => {
      read();
      forwards.push({ wan_ip: "0.0.0.0", wan_port: 1, lan_ip: "", lan_port: 1, proto: "tcp" });
      render();
    };
    el.querySelectorAll("[data-r-del]").forEach((b) => {
      (b as HTMLElement).onclick = () => {
        read();
        const tr = (b as HTMLElement).closest("tr")!;
        // Index among the DATA rows only: tbody children include the header
        // <tr>, so counting them would splice the wrong rule (and the first
        // rule would never delete).
        const rows = [...el.querySelectorAll("[data-r-dir]")].map((s) => (s as HTMLElement).closest("tr")!);
        rules.splice(rows.indexOf(tr), 1);
        render();
      };
    });
    el.querySelectorAll("[data-f-del]").forEach((b) => {
      (b as HTMLElement).onclick = () => {
        read();
        const tr = (b as HTMLElement).closest("tr")!;
        const rows = [...el.querySelectorAll("[data-f-lanip]")].map((s) => (s as HTMLElement).closest("tr")!);
        forwards.splice(rows.indexOf(tr), 1);
        render();
      };
    });
  }

  api.get<{ rules: FirewallRule[]; forwards: FirewallForward[]; policy: FirewallPolicy }>(
    `/proxy/${state.selectedConn}/networks/${encodeURIComponent(netName)}/firewall`)
    .then(async (fw) => {
      rules = fw.rules ?? [];
      forwards = fw.forwards ?? [];
      policy = fw.policy ?? blankPolicy();
      const h = await api.get<{ hosts: IpamHost[] }>(
        `/proxy/${conn}/networks/${encodeURIComponent(netName)}/hosts`).catch(() => null);
      lanHosts = h?.hosts ?? [];
      render();
    })
    .catch((e) => toast((e as Error).message, "bad"));

  $("#fw-cancel", el).onclick = () => el.remove();
  $("#fw-save", el).onclick = async () => {
    read();
    if (forwards.some((f) => !f.lan_ip)) return toast("Pick a guest for each port forward", "bad");
    try {
      await api.put(`/proxy/${state.selectedConn}/networks/${encodeURIComponent(netName)}/firewall`,
        { rules, forwards, policy });
      toast("Firewall applied");
      el.remove();
    } catch (e) {
      toast((e as Error).message, "bad");
    }
  };
}
