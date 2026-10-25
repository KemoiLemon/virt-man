// Shared app state. ES modules cannot reassign another module's `let`, so the
// variables that views read and write live on one exported object; a view
// module imports `state` and reads `state.selectedConn` like it did the bare
// variable. State private to one section (the sidebar, uploads, fetches) stays
// in that section's own module.
import type { Connection, ContainerSummary, DomainSummary, NetworkSummary, PoolSummary, User } from "./api";
import type { AdminSection, AdminTab, ConnTab, DcTab } from "./route";
import { displayName } from "./util";

export type VmTab = "summary" | "hardware" | "options" | "network" | "console" | "xml";

export const state = {
  user: null as User | null,
  authDisabled: false,
  connections: [] as Connection[],
  selectedConn: null as string | null,
  connTab: "dashboard" as ConnTab,
  selectedVm: null as string | null,
  /// The open guest is a container. Containers are served by the same view as
  /// VMs (summary, console, hardware, options, network, XML); this picks the
  /// daemon's `/containers` routes instead of `/domains`, and swaps in what a
  /// container has instead (a text console, a resources editor) while hiding what
  /// it does not (graphics, reset, devices). One flag rather than a second copy
  /// of the guest view, so the two cannot drift apart.
  ctMode: false,
  vmTab: "summary" as VmTab,
  dcTab: "dashboard" as DcTab,
  /// The network/pool currently open in the content pane (mutually exclusive
  /// with selectedVm, like the server tabs).
  selectedNet: null as string | null,
  selectedPool: null as string | null,

  /// Pool whose volume table is expanded; survives the 1s pool poll so the
  /// table is re-rendered in place instead of vanishing every second.
  volumesOpen: null as string | null,
  /// Pool names last rendered, so the poll can diff and skip a full rebuild.
  // null = nothing painted yet. An empty array would mean "painted an empty
  // list", and the two used to be the same value: a server with no pools
  // compared equal to the fresh container and stayed on its skeleton forever.
  renderedPools: null as string[] | null,
  /// The admin window's open section, or null when it is closed. A window over
  /// the page that was open, not a view: the page underneath keeps its place,
  /// and the URL still names that page.
  adminSection: null as AdminSection | null,
  /// The open tab of a tabbed admin section (the dashboard).
  adminTab: undefined as AdminTab | undefined,
  adminEl: null as HTMLElement | null,
  /// Bumped per admin paint and on close, so a read that lands late is dropped.
  adminGen: 0,
  /// The entrance to play once the section just chosen has painted: +1 slides in
  /// from below (a row further down), -1 from above. Null for a same-section repaint.
  adminEnter: null as 1 | -1 | null,
  /// Which way an admin tab switch went (1 = to the right); see renderAdmin.
  adminTabDir: null as 1 | -1 | null,
  adminTabAt: 0,
  adminIdx: null as number | null,
  /// An /admin link, opened over the datacenter once the first render is done.
  pendingAdmin: null as { section: AdminSection; tab?: AdminTab } | null,
  pollTimer: null as number | null,
  /// Retries for a daemon saved while unreachable: re-fetches its cert every
  /// second until it is up, then pins it.
  pendingPoll: null as number | null,
  /// Bumped by every full-pane render. A renderer that has to await a request
  /// captures this first and bails if it has moved, so a slow response cannot
  /// paint over the view the user has since switched to. Without it the 1s poll
  /// races the user: an in-flight network fetch resolves after they clicked a
  /// VM and replaces it, which is why the page "just came back".
  renderGen: 0,
  /// Console view settings, kept across tab switches and reconnects.
  consoleZoom: 0, // 0 = fit to the stage
  consoleResizeObserver: null as ResizeObserver | null,
  /// The text console (xterm.js), loaded when a container console or a host
  /// shell opens: it would otherwise more than double the bundle every page
  /// load pays for.
  terminal: null as typeof import("./terminal") | null,
  /// Set while a URL is being restored (boot, Back/Forward): the address-bar
  /// write is then a replaceState, not a push.
  restoringRoute: false,
  /// A real navigation must not carry a table's scroll offset into the next
  /// view. `syncUrl` sets this when the path changes; the next `setHtml` consumes
  /// it, while the 1s poll (same path) keeps the offsets.
  tableScrollReset: false,
};

/// Which disk the summary's Disks card is graphing, per VM, so the 1s poll
/// does not reset the user's selection back to the first disk.
export const summaryDiskSel: Record<string, string> = {};
/// Which NIC the summary's Network card is graphing, per VM.
export const summaryNetSel: Record<string, string> = {};

/// VMs per connection, so the sidebar tree can nest guests under their server
/// without every render refetching.
export const treeVms: Record<string, DomainSummary[]> = {};
/// Containers per connection, shown in the tree below the VMs.
export const treeCts: Record<string, ContainerSummary[]> = {};
/// Networks and storage pools per connection, shown in the sidebar alongside
/// guests so a network or pool is one click away from its management page.
export const treeNets: Record<string, NetworkSummary[]> = {};
export const treePools: Record<string, PoolSummary[]> = {};
/// Volume titles by `<pool>/<name>`, for display.
export const volumeNames: Record<string, string> = {};

/// `displayName` against the selected server's cached lists, for a caller
/// holding only a network or pool name (a modal title, a confirm message).
/// The name stays the identity everywhere it is sent; this is display only.
export function shownNet(name: string): string {
  return displayName(state.selectedConn ? treeNets[state.selectedConn] : undefined, name);
}
export function shownVolume(pool: string, name: string): string {
  return volumeNames[`${pool}/${name}`] || name;
}

export function shownPool(name: string): string {
  return displayName(state.selectedConn ? treePools[state.selectedConn] : undefined, name);
}
