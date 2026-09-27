// URL <-> view mapping, so a reload (or a shared link) lands where you were.
//
//   /                              datacenter dashboard
//   /servers, /guests              datacenter tabs
//   /<node>                        server dashboard
//   /<node>/vms|networks|storage|firewall
//   /<node>/vms/<vm>               guest summary
//   /<node>/vms/<vm>/<tab>         console | hardware | options | network | xml
//   /<node>/networks/<net>         network page
//   /<node>/storage/<pool>         pool page
//
// A node is named by its connection name. The web server answers its own API
// on top-level paths (/users, /activity, ...), so a node whose name is one of
// those, or is shared by another node, is addressed by its connection id
// instead: a reload of /users would otherwise get JSON, not the app.

export type DcTab = "dashboard" | "servers" | "guests";
export type ConnTab = "dashboard" | "vms" | "networks" | "storage" | "firewall";
export type VmTab = "summary" | "hardware" | "options" | "network" | "console" | "xml";

export type Route =
  | { view: "dc"; tab: DcTab }
  | { view: "conn"; conn: string; tab: ConnTab }
  | { view: "vm"; conn: string; vm: string; tab: VmTab }
  | { view: "net"; conn: string; net: string }
  | { view: "pool"; conn: string; pool: string };

export interface NodeRef {
  id: string;
  name: string;
}

/// Top-level paths the web server owns, plus the datacenter tabs.
export const RESERVED = new Set([
  "assets", "auth", "avatars", "proxy", "users", "activity", "connections",
  "roles", "keys", "me", "health", "uploads", "history", "favicon.svg", "icons.svg",
  "servers", "guests",
]);

const CONN_TABS: ConnTab[] = ["vms", "networks", "storage", "firewall"];
const VM_TABS: VmTab[] = ["hardware", "options", "network", "console", "xml"];

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
  const node = `/${enc(nodeSegment(r.conn, nodes))}`;
  switch (r.view) {
    case "conn":
      return r.tab === "dashboard" ? node : `${node}/${r.tab}`;
    case "vm":
      return `${node}/vms/${enc(r.vm)}${r.tab === "summary" ? "" : `/${r.tab}`}`;
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
  if (parts.length === 1 && (first === "servers" || first === "guests")) {
    return { view: "dc", tab: first };
  }
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
  if (section === "networks") return { view: "net", conn, net: name };
  if (section === "storage") return { view: "pool", conn, pool: name };
  return { view: "conn", conn, tab: section as ConnTab };
}
