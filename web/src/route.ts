// URL <-> view mapping, so a reload (or a shared link) lands where you were.
//
//   /                              datacenter dashboard
//   /guests                        datacenter guests tab
//   /servers                       old Servers tab: the admin Daemons page now
//   /containers                    datacenter containers tab
//   /networks, /storage            datacenter networks and storage tabs
//   /<node>                        server dashboard
//   /<node>/vms|containers|networks|storage|firewall
//   /<node>/vms/<vm>               guest summary
//   /<node>/vms/<vm>/<tab>         console | hardware | options | network | xml
//   /<node>/containers/<ct>[/<tab>] console | hardware | options | network | xml
//   /<node>/networks/<net>         network page
//   /<node>/storage/<pool>         pool page
//   /admin/<section>[/<tab>]       administration (/admin/dashboard/usage, ...)
//   /admin                         the admin dashboard's first tab
//
// A node is named by its connection name. The web server answers its own API
// on top-level paths (/users, /activity, ...), so a node whose name is one of
// those, or is shared by another node, is addressed by its connection id
// instead: a reload of /users would otherwise get JSON, not the app.

export type DcTab = "dashboard" | "guests" | "containers" | "networks" | "storage";
export type ConnTab = "dashboard" | "vms" | "containers" | "networks" | "storage" | "firewall";
export type VmTab = "summary" | "hardware" | "options" | "network" | "console" | "xml";
/// A container's Hardware tab edits its resources, not devices.
export type CtTab = "summary" | "hardware" | "options" | "network" | "console" | "xml";
export type AdminSection =
  | "dashboard" | "users" | "groups" | "roles" | "permissions" | "daemons"
  | "vms" | "containers" | "networks" | "pools" | "quotas" | "ips" | "keys" | "activity";
/// A tabbed section's tab. For the dashboard it is one of ADMIN_TABS; for the
/// VMs, Networks and Storage Pools sections it is a daemon (connection id),
/// written in the URL by its node segment like every other server link.
export type AdminTab = string;
/// Sections with fixed tabs. Every admin page follows the rest of the app's
/// pattern: a section is a path segment, and so is its tab.
export const ADMIN_TABS: Partial<Record<AdminSection, AdminTab[]>> = {
  dashboard: ["overview", "usage", "ownership", "activity"],
};
/// Sections tabbed by daemon: one tab per server.
export const DAEMON_TABBED: AdminSection[] = ["vms", "containers", "networks", "pools"];
export const ADMIN_SECTIONS: AdminSection[] = [
  "dashboard", "users", "groups", "roles", "permissions", "daemons",
  "vms", "containers", "networks", "pools", "quotas", "ips", "keys", "activity",
];

export type Route =
  | { view: "dc"; tab: DcTab }
  | { view: "conn"; conn: string; tab: ConnTab }
  | { view: "vm"; conn: string; vm: string; tab: VmTab }
  | { view: "ct"; conn: string; ct: string; tab: CtTab }
  | { view: "net"; conn: string; net: string }
  | { view: "pool"; conn: string; pool: string }
  | { view: "admin"; section: AdminSection; tab?: AdminTab };

export interface NodeRef {
  id: string;
  name: string;
}

/// Top-level paths the web server owns, plus the datacenter tabs.
export const RESERVED = new Set([
  "assets", "auth", "avatars", "proxy", "users", "activity", "connections",
  "roles", "keys", "me", "health", "uploads", "history", "access-logs", "favicon.svg", "icons.svg",
  "groups", "acl", "resources", "ips", "usage", "shares",
  "servers", "guests", "containers", "networks", "storage", "templates", "admin",
]);

/// Top-level paths that are a Datacenter tab. They are in RESERVED (the server
/// answers nothing there), which is what keeps `parsePath` from reading one as
/// a node segment: a server named "networks" is addressed by its id instead.
const DC_TABS: DcTab[] = ["guests", "containers", "networks", "storage"];

const CONN_TABS: ConnTab[] = ["vms", "containers", "networks", "storage", "firewall"];
const VM_TABS: VmTab[] = ["hardware", "options", "network", "console", "xml"];
const CT_TABS: CtTab[] = ["hardware", "options", "network", "console", "xml"];

/// The path segment for a node: its name when that is unambiguous, else its id.
export function nodeSegment(id: string, nodes: NodeRef[]): string {
  const node = nodes.find((n) => n.id === id);
  if (!node) return id;
  const name = node.name;
  const shared = nodes.some((n) => n.id !== id && n.name === name);
  const clashesWithId = nodes.some((n) => n.id === name && n.id !== id);
  if (!name || shared || clashesWithId || RESERVED.has(name.toLowerCase())) return id;
  return name;
}

const enc = encodeURIComponent;

export function routeToPath(r: Route, nodes: NodeRef[]): string {
  if (r.view === "dc") return r.tab === "dashboard" ? "/" : `/${r.tab}`;
  if (r.view === "admin") {
    // Every section has its own path, the dashboard included; the first tab
    // of a tabbed section is its bare path, as a server's Dashboard is.
    if (DAEMON_TABBED.includes(r.section)) {
      const first = nodes[0]?.id;
      return r.tab && r.tab !== first
        ? `/admin/${r.section}/${enc(nodeSegment(r.tab, nodes))}`
        : `/admin/${r.section}`;
    }
    const first = ADMIN_TABS[r.section]?.[0];
    return r.tab && r.tab !== first ? `/admin/${r.section}/${r.tab}` : `/admin/${r.section}`;
  }
  const node = `/${enc(nodeSegment(r.conn, nodes))}`;
  switch (r.view) {
    case "conn":
      return r.tab === "dashboard" ? node : `${node}/${r.tab}`;
    case "vm":
      return `${node}/vms/${enc(r.vm)}${r.tab === "summary" ? "" : `/${r.tab}`}`;
    case "ct":
      return `${node}/containers/${enc(r.ct)}${r.tab === "summary" ? "" : `/${r.tab}`}`;
    case "net":
      return `${node}/networks/${enc(r.net)}`;
    case "pool":
      return `${node}/storage/${enc(r.pool)}`;
  }
}

/// Resolve a node segment back to a connection id: id first, then a unique name.
function resolveNode(seg: string, nodes: NodeRef[]): string | null {
  if (nodes.some((n) => n.id === seg)) return seg;
  const byName = nodes.filter((n) => n.name === seg);
  return byName.length === 1 ? byName[0].id : null;
}

/// Parse a pathname. Anything unrecognised (a deleted node, a mistyped tab)
/// lands on the nearest view that still exists rather than on an error.
export function parsePath(path: string, nodes: NodeRef[]): Route {
  let parts: string[];
  try {
    parts = path.split("/").filter(Boolean).map(decodeURIComponent);
  } catch {
    return { view: "dc", tab: "dashboard" };
  }
  const [first, section, name, tab] = parts;
  if (!first) return { view: "dc", tab: "dashboard" };
  if (first === "admin" && parts.length <= 3) {
    // Resources was one page before it split by kind; keep old links working.
    const alias = section === "resources" ? "vms" : section;
    const s = ADMIN_SECTIONS.includes(alias as AdminSection) ? (alias as AdminSection) : "dashboard";
    if (DAEMON_TABBED.includes(s)) {
      // No daemons yet: the section still opens, with nothing to tab.
      const conn = (name && resolveNode(name, nodes)) || nodes[0]?.id;
      return conn ? { view: "admin", section: s, tab: conn } : { view: "admin", section: s };
    }
    const tabs = ADMIN_TABS[s];
    if (!tabs) return { view: "admin", section: s };
    // An unknown tab lands on the section's first, like a mistyped server tab.
    const t = tabs.includes(name as AdminTab) ? (name as AdminTab) : tabs[0];
    return { view: "admin", section: s, tab: t };
  }
  if (parts.length === 1 && DC_TABS.includes(first as DcTab)) {
    return { view: "dc", tab: first as DcTab };
  }
  // The Datacenter's Servers tab moved to the admin page's Daemons section;
  // an old link still lands there (main.ts sends a non-admin to "/").
  if (parts.length === 1 && first === "servers") return { view: "admin", section: "daemons" };
  const conn = resolveNode(first, nodes);
  if (!conn) return { view: "dc", tab: "dashboard" };
  if (!section || !CONN_TABS.includes(section as ConnTab)) {
    return { view: "conn", conn, tab: "dashboard" };
  }
  if (!name) return { view: "conn", conn, tab: section as ConnTab };
  if (section === "vms") {
    // The Firewall tab became part of Network; keep old links working.
    const t = tab === "firewall" ? "network" : tab;
    const vmTab = VM_TABS.includes(t as VmTab) ? (t as VmTab) : "summary";
    return { view: "vm", conn, vm: name, tab: vmTab };
  }
  if (section === "containers") {
    const ctTab = CT_TABS.includes(tab as CtTab) ? (tab as CtTab) : "summary";
    return { view: "ct", conn, ct: name, tab: ctTab };
  }
  if (section === "networks") return { view: "net", conn, net: name };
  if (section === "storage") return { view: "pool", conn, pool: name };
  return { view: "conn", conn, tab: section as ConnTab };
}
