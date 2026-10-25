// Access log view: what guests connected to, from the web server's two-week
// store (`GET /access-logs`). Pure helpers for the table; pages/access-logs.ts owns the
// dialog, since it needs setHtml/openModal.

export type Group = "" | "domain" | "ip" | "proto" | "port" | "vm";
/// Which records: the guests' DNS lookups, the connections the kernel
/// tracked, or both.
export type Kind = "both" | "dns" | "connections";
/// Which way a connection went: the guest opened it (`out`), or someone
/// reached the guest (`in`, a port forward). Empty is both.
export type FlowDir = "" | "in" | "out";

export const DIR_LABEL: Record<FlowDir, string> = {
  "": "In and out",
  out: "Outgoing",
  in: "Incoming",
};

export const KIND_LABEL: Record<Kind, string> = {
  both: "DNS lookups + connections",
  dns: "DNS lookups",
  connections: "Connections",
};
export type Dir = "asc" | "desc";

export interface LogFilter {
  vm: string;
  proto: string;
  q: string;
  port: string;
  range: string; // "1h" | "24h" | "7d" | "14d"
  group: Group;
  sort: string;
  dir: Dir;
  kind: Kind;
  direction: FlowDir;
  page: number;
}

export const PAGE = 100;

export const RANGE_MS: Record<string, number> = {
  "1h": 3_600_000,
  "24h": 86_400_000,
  "7d": 7 * 86_400_000,
  "14d": 14 * 86_400_000,
};

export const GROUP_LABEL: Record<Group, string> = {
  "": "Every connection",
  domain: "Most accessed domains",
  ip: "Most accessed IPs",
  proto: "Most used protocols",
  port: "Most used ports",
  vm: "Busiest VMs",
};

export function defaultFilter(vm = ""): LogFilter {
  return { vm, proto: "", q: "", port: "", range: "24h", group: "", sort: "time", dir: "desc", kind: "both", direction: "", page: 0 };
}

/// The query string for a filter.
export function logQuery(conn: string, f: LogFilter, now: number): string {
  const p = new URLSearchParams({ conn, sort: f.sort, dir: f.dir, limit: String(PAGE), offset: String(f.page * PAGE) });
  p.set("since_ms", String(now - (RANGE_MS[f.range] ?? RANGE_MS["24h"])));
  if (f.vm) p.set("vm", f.vm);
  if (f.proto) p.set("proto", f.proto);
  if (f.q.trim()) p.set("q", f.q.trim());
  if (/^\d+$/.test(f.port.trim())) p.set("port", f.port.trim());
  if (f.group) p.set("group", f.group);
  if (f.kind !== "both") p.set("kind", f.kind);
  if (f.direction) p.set("direction", f.direction);
  return p.toString();
}

/// Clicking a column header: the same column flips direction, another column
/// starts at its natural direction (biggest/newest first; names A→Z).
export function toggleSort(f: LogFilter, col: string): LogFilter {
  if (f.sort === col) return { ...f, dir: f.dir === "asc" ? "desc" : "asc", page: 0 };
  const alpha = ["key", "domain", "ip", "vm", "proto"].includes(col);
  return { ...f, sort: col, dir: alpha ? "asc" : "desc", page: 0 };
}

/// Grouping changes what the sortable columns are; keep the sort valid.
export function withGroup(f: LogFilter, group: Group): LogFilter {
  return { ...f, group, sort: group ? "count" : "time", dir: "desc", page: 0 };
}

/// Clicking a group opens the connections behind it.
export function drillDown(f: LogFilter, key: string): LogFilter {
  const next: LogFilter = { ...f, group: "", sort: "time", dir: "desc", page: 0 };
  switch (f.group) {
    case "domain": case "ip": return { ...next, q: key };
    case "proto": return { ...next, proto: key };
    case "port": return { ...next, port: key };
    case "vm": return { ...next, vm: key };
    default: return next;
  }
}

export function fmtWhen(ms: number): string {
  const d = new Date(ms);
  const two = (n: number) => String(n).padStart(2, "0");
  return `${d.getMonth() + 1}/${d.getDate()} ${two(d.getHours())}:${two(d.getMinutes())}:${two(d.getSeconds())}`;
}

/// `▲`/`▼` on the active sort column.
export function sortMark(f: LogFilter, col: string): string {
  return f.sort === col ? (f.dir === "asc" ? " ▲" : " ▼") : "";
}

/// Live tail only makes sense where new rows appear: the newest-first,
/// ungrouped first page. Anywhere else it would reshuffle what is being read.
export function liveApplies(f: LogFilter): boolean {
  return !f.group && f.sort === "time" && f.dir === "desc" && f.page === 0;
}

/// The view a Live toggle switches to: newest first, first page, same filters.
export function forLive(f: LogFilter): LogFilter {
  return { ...f, group: "", sort: "time", dir: "desc", page: 0 };
}
