// Pure helpers shared by the views: formatting, and the names a guest, network
// or pool goes by. Nothing here reads app state, so it can be imported from
// anywhere without creating a cycle.
import { ic } from "./icons";
import type { NetworkSummary } from "./api";
import { esc } from "./xml";

/// Title-case a status/type token for display. The API keeps lowercase values
/// (the frontend compares against them), so capitalizing happens only here.
export function capLabel(s: string): string {
  if (!s) return s;
  const pretty: Record<string, string> = {
    nostate: "No state",
    pmsuspended: "Suspended",
    shutdown: "Shutdown",
  };
  return pretty[s] ?? s[0].toUpperCase() + s.slice(1);
}

/// Recent-first timestamp for the activity log: relative while it is recent,
/// a clock time for today, and a dated time for anything older. Absolute
/// timestamps on every row would be noise; "just now" is what matters for an
/// action you just ran.
export function relTime(ms: number): string {
  const now = Date.now();
  const delta = now - ms;
  if (delta < 0) return "just now"; // clock skew between host and browser
  if (delta < 45_000) return "just now";
  if (delta < 3_600_000) return `${Math.max(1, Math.round(delta / 60_000))}m ago`;
  const then = new Date(ms);
  const time = then.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  // Same calendar day reads as a clock time; anything older needs the date.
  return then.toDateString() === new Date(now).toDateString()
    ? time
    : `${then.toLocaleDateString([], { month: "short", day: "numeric" })} ${time}`;
}

/// What an empty list shows: a line saying so, and a small button to create
/// the first one. `id` names the button so the caller can bind it.
export function emptyState(what: string, label: string, id: string): string {
  return `<div class="empty empty-create"><span class="empty-msg">No ${what} available.</span>
    <button class="btn" id="${id}">${ic.plus(14)} ${label}</button></div>`;
}

/// Whether `s` is a UUID in libvirt's canonical form: a guest, network or pool
/// named by its identity rather than by what people call it.
export function isUuid(s: string): boolean {
  return /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(s);
}

/// Put `title` in a definition's `<title>`, replacing one or adding it after
/// `<name>` (libvirt wants it there).
export function withTitle(xml: string, title: string): string {
  const t = `<title>${esc(title)}</title>`;
  if (/<title>[^<]*<\/title>/.test(xml)) return xml.replace(/<title>[^<]*<\/title>/, t);
  return xml.replace(/(<name>[^<]*<\/name>)/, `$1\n  ${t}`);
}

/// Undo XML's five escapes, for text read out of a definition.
export function unescapeXml(s: string): string {
  return s.replace(/&lt;/g, "<").replace(/&gt;/g, ">").replace(/&quot;/g, "\"")
    .replace(/&apos;/g, "'").replace(/&amp;/g, "&");
}

/// A resource's UUID from a loaded list, or `name` if it is not there.
export function uuidOf(list: { name: string; uuid?: string }[] | undefined, name: string): string {
  return list?.find((x) => x.name === name)?.uuid || name;
}

/// The libvirt name for what a URL named: a UUID, or (old links) a name.
export function nameOf(list: { name: string; uuid?: string }[] | undefined, id: string): string {
  const low = id.toLowerCase();
  return list?.find((x) => x.uuid?.toLowerCase() === low)?.name ?? id;
}

/// What people call a guest, network or pool: its title, else its libvirt
/// name. The libvirt name is a UUID for anything made or migrated since
/// UUIDs, so it is never shown when there is a title.
export function displayName(list: { name: string; title?: string | null }[] | undefined, name: string): string {
  return list?.find((x) => x.name === name)?.title || name;
}

/// The network a NIC's `<source>` names: `network` in a definition written
/// before networks became the daemon's own, or the network whose bridge the
/// guest is plugged into now. An operator's own bridge belongs to no network
/// here, and stays its own name.
export function nicNetwork(nets: NetworkSummary[] | undefined, source: string): string {
  const at = (k: string) => source.match(new RegExp(`${k}\\s*=\\s*['"]([^'"]*)['"]`))?.[1] ?? "";
  const net = at("network");
  if (net) return net;
  const bridge = at("bridge");
  return nets?.find((n) => n.bridge === bridge)?.name ?? bridge;
}

/// The network key behind a NIC's bridge, for a form that picks by network.
/// Unknown (an operator's bridge) leaves the network empty and keeps the
/// bridge, so the device round-trips unchanged.
export function bridgeNetwork(nets: { name: string; bridge: string }[], bridge: string): string {
  return nets.find((n) => n.bridge === bridge)?.name ?? "";
}
