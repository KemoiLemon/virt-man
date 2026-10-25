import { pendingReads, pollWindow } from "./api";
import { state } from "./state";
import { busyScrolling } from "./scroll-busy";
import { isOffline, refreshHealth } from "./boot";
import { refreshTreeVms } from "./tree";
import { loadDcDashboard } from "./datacenter";
import { loadNetworks, renderNetPage } from "./network-page";
import { loadPools, renderPoolPage } from "./pool-page";
import { loadCts } from "./containers";
import { loadDashboard, loadFilters, loadVms } from "./connection";
import { renderVmBody } from "./vm-page";

// ---------------------------------------------------------------- polling

export function startPolling() {
  if (state.pollTimer) window.clearInterval(state.pollTimer);
  // One round at a time: a tick that finds reads still undelivered does
  // nothing. `api.ts` delivers a round at most once a second, and the tick is
  // finer than that so the next round starts as soon as the last one has
  // landed, which keeps the cadence at the delivery pace instead of at every
  // other tick.
  state.pollTimer = window.setInterval(() => {
    if (!state.user || pendingReads() > 0) return;
    // A repaint replaces the node the browser is scrolling, which detaches the
    // touch gesture: a drag over a table would stop dead mid-flick.
    if (busyScrolling()) return;
    pollWindow(pollRound);
  }, 250);
}

/// One poll round: the reads it starts are delivered together at the next
/// update slot (`api.ts`), so the views repaint once.
function pollRound() {
  void refreshHealth();
  // A down server's views show the offline panel; polling its daemon would
  // only fail again and paint the raw error over it.
  if (isOffline(state.selectedConn)) return;
  // Keep the sidebar's guest states live regardless of the open pane.
  void refreshTreeVms();
  if (!state.selectedConn && !state.selectedVm && state.dcTab === "dashboard") {
    loadDcDashboard();
  } else if (state.selectedConn && !state.selectedVm) {
    // Resource pages poll their own data; only repaint when they are the
    // view actually on screen.
    if (state.selectedNet) renderNetPage();
    else if (state.selectedPool) renderPoolPage();
    else if (state.connTab === "dashboard") loadDashboard();
    else if (state.connTab === "vms") loadVms();
    else if (state.connTab === "containers") loadCts();
    else if (state.connTab === "networks") loadNetworks();
    else if (state.connTab === "storage") loadPools();
    // The shell is a live session, not data to re-read; nothing to poll.
    else if (state.connTab === "firewall") loadFilters();
  } else if (state.selectedConn && state.selectedVm && state.vmTab === "summary") {
    renderVmBody(state.selectedVm);
  }
}
