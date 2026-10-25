import { closeTayne, tayneOpen } from "./tayne";
import { initIndicators } from "./indicator";
import { closeContextMenu, contextMenuOpen } from "./ctxmenu";

import { state } from "./state";

import { hooks } from "./hooks";
import { profileHooks } from "./profile";
import { transferHooks } from "./transfers";
import { dcCreate } from "./create-resource";
import { boot, canAdmin, isOffline, renderOffline, roleLabel } from "./boot";
import { closeAdmin, openAdmin, showSecret } from "./admin-window";
import { renderProfileBtn } from "./menus"; // also installs the contextmenu/long-press listeners
import { openCommandPalette } from "./palette";
import { refreshTreeVms, renderTree, selectCtRow, selectNetRow, selectPoolRow, selectVmRow } from "./tree";
import { loadDcDashboard, renderDc } from "./datacenter";
import { loadNetworks, renderNetPage } from "./network-page";
import { loadPools, loadVolumes, renderPoolPage } from "./pool-page";
import { afterCtDelete, deleteCt, deleteCtOn, openCreateCt, openTemplates, renameCt } from "./containers";
import { loadDashboard, renderConn } from "./connection";
import { renderVm, renderVmBody, syncVmActions } from "./vm-page";
import { openFirewallEditor } from "./firewall-editor";
import { startPolling } from "./polling"; // also installs the scroll/pointer listeners

// Moved modules cannot import the views (a cycle), so they reach them through
// `hooks`. Wrappers, so nothing here depends on declaration order.
hooks.renderTree = () => renderTree();
hooks.renderOffline = () => renderOffline();
hooks.isOffline = (conn) => isOffline(conn);
hooks.canAdmin = () => canAdmin();
hooks.openAdmin = (...a) => openAdmin(...(a as Parameters<typeof openAdmin>));
hooks.closeAdmin = () => closeAdmin();
hooks.renderDc = () => renderDc();
hooks.loadDcDashboard = () => loadDcDashboard();
hooks.renderConn = () => renderConn();
hooks.loadDashboard = () => loadDashboard();
hooks.renderVm = () => renderVm();
hooks.renderVmBody = (vm: string) => renderVmBody(vm);
hooks.renderNetPage = () => renderNetPage();
hooks.renderPoolPage = () => renderPoolPage();
hooks.refreshTreeVms = () => refreshTreeVms();
hooks.loadNetworks = () => loadNetworks();
hooks.loadPools = () => loadPools();
hooks.loadVolumes = (pool: string) => loadVolumes(pool);
hooks.deleteCtOn = (...a) => deleteCtOn(...(a as Parameters<typeof deleteCtOn>));
hooks.dcCreate = (...a) => dcCreate(...(a as Parameters<typeof dcCreate>));
hooks.openCreateCt = (...a) => openCreateCt(...(a as Parameters<typeof openCreateCt>));
hooks.openCommandPalette = () => openCommandPalette();
hooks.renderProfileBtn = () => renderProfileBtn();
hooks.startPolling = () => startPolling();
hooks.syncVmActions = () => syncVmActions();
hooks.renameCt = (...a) => renameCt(...(a as Parameters<typeof renameCt>));
hooks.deleteCt = (...a) => deleteCt(...(a as Parameters<typeof deleteCt>));
hooks.afterCtDelete = (...a) => afterCtDelete(...(a as Parameters<typeof afterCtDelete>));
hooks.openFirewallEditor = (...a) => openFirewallEditor(...(a as Parameters<typeof openFirewallEditor>));
hooks.openTemplates = (...a) => openTemplates(...(a as Parameters<typeof openTemplates>));
hooks.selectVmRow = (...a) => selectVmRow(...(a as Parameters<typeof selectVmRow>));
hooks.selectCtRow = (...a) => selectCtRow(...(a as Parameters<typeof selectCtRow>));
hooks.selectNetRow = (...a) => selectNetRow(...(a as Parameters<typeof selectNetRow>));
hooks.selectPoolRow = (...a) => selectPoolRow(...(a as Parameters<typeof selectPoolRow>));
// transfers.ts repaints the pool page through its own hook.
transferHooks.renderPoolPage = renderPoolPage;
// Likewise profile.ts: wrappers, so nothing here depends on declaration order.
profileHooks.renderProfileBtn = () => renderProfileBtn();
profileHooks.roleLabel = (admin) => roleLabel(admin);
profileHooks.showSecret = (title, secret) => showSecret(title, secret);

// ------------------------------------------------------------- global keys

/// Ctrl+K opens the palette; Escape closes the palette, then the topmost
/// modal (dispatched as a synthetic event so askText/askConfirm can resolve
/// their promise). The guest owns the keyboard while the console is focused,
/// so the palette stays out of the way there.
document.addEventListener("keydown", (e) => {
  if ((e.ctrlKey || e.metaKey) && (e.key === "k" || e.key === "K")) {
    // Only inside the real app: the login/setup screens have no commands to
    // offer, and the guest owns the keyboard while the console is focused.
    if (!state.user) return;
    const active = document.activeElement as HTMLElement | null;
    if (active?.closest?.(".console-stage")) return;
    e.preventDefault();
    if (tayneOpen()) closeTayne();
    else openCommandPalette();
    return;
  }
  if (e.key === "Escape") {
    // One press dismisses one layer, topmost first: the context menu sits
    // above the palette, which sits above any modal.
    if (contextMenuOpen()) {
      closeContextMenu();
      return;
    }
    if (tayneOpen()) {
      closeTayne();
      return;
    }
    const modals = document.querySelectorAll(".modal-back.open");
    const top = modals[modals.length - 1];
    if (top) top.dispatchEvent(new Event("modal-escape", { bubbles: false }));
  }
});

initIndicators();
boot();
