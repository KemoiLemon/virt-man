import { api } from "../api";
import type { Connection, CreateConnectionResult } from "../api";
import { esc } from "../xml";
import { state } from "../app/state";
import { openModal, toast } from "../app/dialogs";
import { hooks } from "../app/hooks";
import { renderView } from "../app/nav";
import { $ } from "../app/dom";

/// Add a server, or edit an existing one when `editId` is given. Daemons
/// autoconnect, so editing (not a manual connection test) is the useful
/// per-row action.
export function openConnectionDialog(editId?: string) {
  const existing = editId ? state.connections.find((c) => c.id === editId) : undefined;
  const el = openModal(
    existing ? `Edit Server — ${esc(existing.name)}` : "Add Server",
    `
    <label class="field"><span>Name</span><input id="conn-name" value="${esc(existing?.name ?? "")}"></label>
    <label class="field"><span>URL (https://host:8443)</span><input id="conn-url" placeholder="https://host:8443" value="${esc(existing?.url ?? "")}"></label>
    <label class="field"><span>Auth Token</span>
      <input id="conn-token" placeholder="${existing ? "leave blank to keep the current token" : "paste existing token, or leave blank to generate one"}">
    </label>
    <p class="hint" id="conn-fp">${existing?.fingerprint ? `Pinned certificate: ${esc(existing.fingerprint)}` : ""}</p>`,
    `<button class="btn" id="conn-save">Save</button>`,
  );
  $("#conn-save", el).onclick = async () => {
    try {
      const body: any = {
        name: $("#conn-name", el).value,
        url: $("#conn-url", el).value.trim(),
      };
      const typedToken = $("#conn-token", el).value.trim();
      // Never ask the server to mint a token on edit: a blank field means
      // "keep the current one", not "replace it with a new one". Creating
      // with a blank field still generates one server-side, which is also what
      // makes the install script carry that same token.
      if (typedToken) body.token = typedToken;
      else if (!existing) body.token = "";

      if (existing) {
        await api.put(`/connections/${existing.id}`, body);
        toast("Server updated");
        el.remove();
        await reloadConnections();
        return;
      }

      const created = await api.post<CreateConnectionResult>("/connections", body);
      el.remove();
      await reloadConnections();
      if (created.daemon_unreachable) {
        const statusEl = offerInstallScript(created.install_script ?? "");
        void pollUntilCertPinned(created.id, statusEl);
        return;
      }
      toast("Server added");
    } catch (e) {
      toast((e as Error).message, "bad");
    }
  };
}

async function reloadConnections() {
  state.connections = await api.get<Connection[]>("/connections");
  hooks.renderTree();
  if (!state.selectedConn) renderView();
  void hooks.refreshTreeVms();
}

/// Show the generated installer for a daemon that was saved while unreachable.
/// Returns the status line element so the poller can update it in place.
function offerInstallScript(script: string): HTMLElement | null {
  const el = openModal(
    "Daemon Not Reachable",
    `
    <label class="field"><span>Install Script</span>
      <textarea id="install-script" rows="14" readonly spellcheck="false">${esc(script)}</textarea>
    </label>
    <p class="hint" id="pending-status">Waiting for the daemon…</p>`,
    `<button class="btn" id="dl-script">Download Script</button>`,
  );
  $("#dl-script", el).onclick = () => {
    const blob = new Blob([script], { type: "application/x-sh" });
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    a.href = url;
    a.download = "install-virt-man-daemon.sh";
    a.click();
    URL.revokeObjectURL(url);
  };
  return $("#pending-status", el);
}

/// Poll a pending daemon every second until its cert can be fetched, then pin
/// it. The generated install script may still be running.
function pollUntilCertPinned(connId: string, statusEl: HTMLElement | null) {
  stopPendingPoll();
  state.pendingPoll = window.setInterval(async () => {
    try {
      const r = await api.get<{ cert: string; fingerprint: string }>(`/connections/${connId}/cert`);
      stopPendingPoll();
      const conn = state.connections.find((c) => c.id === connId);
      if (conn) await api.put(`/connections/${connId}`, { name: conn.name, url: conn.url, cert: r.cert });
      toast("Daemon connected — certificate pinned");
      await reloadConnections();
    } catch {
      if (statusEl) statusEl.textContent = "Waiting for the daemon…";
    }
  }, 1000);
}

export function stopPendingPoll() {
  if (state.pendingPoll !== null) {
    window.clearInterval(state.pendingPoll);
    state.pendingPoll = null;
  }
}
