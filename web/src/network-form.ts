import { api } from "./api";
import { assignOwner } from "./admin";
import { ic } from "./icons";
import { enhanceSelects } from "./listbox";
import { runTask } from "./task";
import { esc, networkFormFromXml, networkUuid, networkXml, newUuid } from "./xml";
import type { NetworkForm } from "./xml";
import { state } from "./state";
import { openModal, toast } from "./dialogs";
import { hooks } from "./hooks";
import { $ } from "./dom";

export async function openEditNetwork(name: string) {
  if (!state.selectedConn) return;
  let xml = "";
  try {
    xml = (await api.get<{ xml: string }>(`/proxy/${state.selectedConn}/networks/${encodeURIComponent(name)}/xml`)).xml;
  } catch (e) {
    return toast((e as Error).message, "bad");
  }
  // `name` is the libvirt name (a UUID, for any network made or migrated
  // since UUIDs); the form edits the display name. Carried separately so an
  // edit keeps the libvirt name and changes only the title.
  openNetworkForm(networkFormFromXml(xml), networkUuid(xml), state.selectedConn, undefined, name);
}

/// `conn` defaults to the selected server. The admin page passes its own
/// (with an owner and a callback), so creating a network for a user happens
/// over the admin page instead of navigating away from it.
export function openCreateNetwork(conn = state.selectedConn, forUser?: { owner: string; after: () => void }) {
  openNetworkForm(
    { name: "", forward: "nat", bridge: "", ip: "", dhcp_start: "", dhcp_end: "", dns: "", hosts: [] },
    "",
    conn,
    forUser,
  );
}

/// One network form for create and edit. Fields appear and disappear with the
/// forward mode: bridge mode offers a bridge picker and no IP/DHCP/DNS (the
/// bridge owns addressing); NAT offers IP, DHCP and DNS forwarders; isolated
/// offers IP/DHCP but no bridge.
export function openNetworkForm(
  initial: NetworkForm,
  uuid: string,
  conn: string | null = state.selectedConn,
  forUser?: { owner: string; after: () => void },
  libvirtName = "",
) {
  if (!conn) return;
  const el = openModal(
    uuid ? `Edit Network — ${esc(initial.name)}` : "Create Network",
    `<div class="grid2">
      <label class="field"><span>Name</span><input id="net-name" value="${esc(initial.name)}"></label>
      <label class="field"><span>Mode</span><select id="net-mode">
        <option value="nat" ${initial.forward === "nat" ? "selected" : ""}>NAT</option>
        <option value="bridge" ${initial.forward === "bridge" ? "selected" : ""}>Bridge</option>
        <option value="none" ${initial.forward === "none" ? "selected" : ""}>Isolated</option>
      </select></label>
      <label class="field" data-net-field="bridge"><span>Bridge</span><select id="net-bridge">
        <option value="">Select a Bridge…</option>
        ${initial.bridge ? `<option value="${esc(initial.bridge)}" selected>${esc(initial.bridge)}</option>` : ""}
      </select></label>
      <label class="field" data-net-field="ip"><span>Network (CIDR)</span><span style="display:flex;gap:7px;align-items:center"><input id="net-ip" placeholder="IP-Less Network" value="${esc(initial.ip)}" readonly><button class="btn icon field-btn" id="net-ip-reroll" type="button" title="Pick Another Subnet">${ic.reroll()}</button><button class="btn icon field-btn" id="net-ip-clear" type="button" title="No Subnet">${ic.trash()}</button></span></label>
      <label class="field" data-net-field="dns"><span>DNS servers (NAT, comma separated)</span><input id="net-dns" placeholder="1.1.1.1, 8.8.8.8" value="${esc(initial.dns)}"></label>
    </div>`,
    `<button class="btn primary" id="net-save">${uuid ? "Save" : "Create"}</button>`,
  );

  const modeEl = () => $("#net-mode", el) as HTMLSelectElement;
  const mode = () => modeEl().value as NetworkForm["forward"];
  // A network with a subnet is served by this daemon from the start; one
  // without is left alone. Only NAT needs a subnet (it must route and hand out
  // addresses); an isolated network is usually a private segment the operator
  // routes themselves, so clearing the subnet is a first-class choice there --
  // which is what a dummy/private routed network wants.
  const sync = () => {
    const m = mode();
    el.querySelectorAll("[data-net-field]").forEach((f: Element) => {
      const field = (f as HTMLElement).dataset.netField!;
      let show: boolean;
      if (m === "bridge") {
        show = field === "bridge";
      } else if (m === "nat") {
        show = field !== "bridge";
      } else {
        // Isolated: DNS forwards nowhere. Nothing else to hide -- a cleared
        // subnet is expressed by the field being empty, not by hiding it, so
        // the trash/re-roll pair can put one back.
        show = field !== "bridge" && field !== "dns";
      }
      (f as HTMLElement).hidden = !show;
    });
    // The trash offers to clear a subnet, so it is shown only when there is a
    // value to clear and the network may have none (isolated). Re-rolling is
    // the way back from an emptied field, so it is always available and the
    // two swap places. NAT gets no trash: it cannot run without a subnet.
    const clear = el.querySelector("#net-ip-clear") as HTMLButtonElement | null;
    if (clear) clear.hidden = m !== "none" || $("#net-ip", el).value === "";
  };
  modeEl().addEventListener("change", sync);

  // Bridge mode picks from the host's real bridge devices.
  const bridgeSelect = $("#net-bridge", el) as HTMLSelectElement;
  api.get<string[]>(`/proxy/${conn}/networks/bridges`)
    .then((bridges) => {
      const current = initial.bridge;
      bridgeSelect.replaceChildren();
      const ph = document.createElement("option");
      ph.value = "";
      ph.textContent = "Select a Bridge…";
      bridgeSelect.append(ph);
      for (const b of bridges) {
        const o = document.createElement("option");
        o.value = b;
        o.textContent = b;
        o.selected = b === current;
        bridgeSelect.append(o);
      }
      enhanceSelects(bridgeSelect);
    })
    .catch(() => {}); // no bridges: the picker shows just the placeholder

  // The subnet is the daemon's to choose, for an existing network as much as
  // for a new one: an address typed here is how two networks end up on one
  // /24, or on the host's own LAN. The field is read-only and the re-roll is
  // the only way to change it -- it asks the daemon for another range, which
  // re-checks every subnet in use, so the form cannot be talked into a
  // collision. Bridge mode takes its addressing from the bridge.
  const ipInput = $("#net-ip", el) as HTMLInputElement;
  const reroll = $("#net-ip-reroll", el) as HTMLButtonElement;
  const clear = $("#net-ip-clear", el) as HTMLButtonElement;
  const suggest = () => {
    if (mode() === "bridge") return;
    // Cleared first so a request that fails leaves no stale address behind
    // for the form to submit as if it were this network's own.
    ipInput.value = "";
    sync();
    api.get<{ subnet: string }>(`/proxy/${conn}/networks/suggest-subnet`)
      .then((r) => {
        ipInput.value = r.subnet;
        sync();
      })
      .catch(() => toast("Could not get a subnet from the server", "bad"));
  };
  // Clearing is offered only where a network may have no subnet at all
  // (isolated), and re-rolling is what brings one back -- so the two buttons
  // swap places rather than either being permanently unavailable.
  clear.addEventListener("click", () => {
    ipInput.value = "";
    sync();
  });
  reroll.addEventListener("click", suggest);
  // A new network starts from a suggestion; an edit keeps the subnet it has
  // until someone re-rolls it, and a mode change never silently re-addresses
  // a network that is already running.
  if (!uuid) suggest();
  $("#net-mode", el).addEventListener("change", () => {
    if (!uuid || (mode() === "nat" && !ipInput.value)) suggest();
  });

  $("#net-save", el).onclick = async () => {
    const form: NetworkForm = {
      name: $("#net-name", el).value,
      forward: mode(),
      bridge: (bridgeSelect as HTMLSelectElement).value,
      // An empty value means no subnet at all: the daemon must not serve the
      // network, and libvirt gets no <ip>. That is how an isolated network the
      // operator routes themselves (a dummy interface, a private routed
      // segment) is expressed -- the trash button clears it.
      ip: $("#net-ip", el).value,
      // The DHCP range is no longer a form field: the daemon's own DHCP hands
      // out the whole subnet (every address permanent), so a range here would
      // only be a second, conflicting answer. Kept from the definition on an
      // edit so an existing range is not silently dropped.
      dhcp_start: initial.dhcp_start,
      dhcp_end: initial.dhcp_end,
      dns: $("#net-dns", el).value,
      hosts: initial.hosts ?? [],
    };
    if (!form.name) return toast("Name is required", "bad");
    if (form.forward === "bridge" && !form.bridge) {
      return toast("Select a bridge", "bad");
    }
    el.remove();
    // Editing keeps the network's libvirt name and identity; a new network
    // is named by a fresh UUID, with the name typed here as its title.
    const id = uuid || newUuid();
    const libvirt = uuid ? (libvirtName || initial.name) : id;
    const xml = networkXml(form, id, libvirt);
    const owner = uuid ? null : forUser?.owner ?? null;
    void runTask(`${uuid ? "Update" : "Create"} network ${form.name}`, async () => {
      if (uuid) await api.putXml(`/proxy/${conn}/networks/${encodeURIComponent(libvirt)}/xml`, xml);
      else await api.postXml(`/proxy/${conn}/networks`, xml);
      if (owner) await assignOwner(conn, "network", libvirt, owner);
    }, { done: `Network ${form.name} ${uuid ? "updated" : "created"}`, notify: toast })
      .then(() => {
        void hooks.refreshTreeVms();
        if (forUser) forUser.after();
        else if (state.selectedConn === conn && !state.selectedVm) void hooks.renderConn();
        // Made on a server that is not the selected one (the Datacenter's
        // Networks tab): repaint the view actually on screen.
        else if (!state.selectedConn) hooks.renderDc();
      });
  };

  sync();
}
