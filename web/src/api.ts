// Backend client. The SPA talks same-origin to virt-man-web: /auth, /users,
// /me, /keys, /connections for the backend API, and /proxy/{id}/... for the
// daemon. Fetch always sends credentials (session cookie).

export class ApiError extends Error {
  status: number;
  constructor(status: number, message: string) {
    super(message);
    this.status = status;
  }
}

async function handle<T>(res: Response): Promise<T> {
  if (res.status === 204) return undefined as T;
  const data = await res.json().catch(() => ({}));
  if (!res.ok) {
    throw new ApiError(res.status, (data as any).error ?? res.statusText);
  }
  return data as T;
}

/// Set only for the synchronous span of `cacheFirst`: GETs issued while it is
/// set ask the web server for its last known answer, and report back whether
/// they got one.
let cacheWindow: { stale: boolean } | null = null;

function request<T>(method: string, url: string, body?: unknown): Promise<T> {
  const hasBody = body !== undefined;
  const headers: Record<string, string> = {};
  if (hasBody) headers["Content-Type"] = "application/json";
  const win = method === "GET" ? cacheWindow : null;
  if (win) headers["X-Virt-Man-Cache"] = "1";
  return fetch(url, {
    method,
    credentials: "same-origin",
    headers,
    body: hasBody ? JSON.stringify(body) : undefined,
  }).then((res) => {
    if (win && res.headers.has("X-Virt-Man-Cached")) win.stale = true;
    return handle<T>(res);
  });
}

/// Run `load` with its daemon reads answered from the web server's cache where
/// it has one, and resolve to whether any answer was cached (so the caller
/// knows to re-read for real). A remote daemon costs a round trip per read;
/// this lets a page paint at once instead of after it.
///
/// The window only covers requests `load` issues synchronously, which is what
/// every loader's leading `Promise.all` does. Reads issued after an await are
/// ordinary reads, which is the safe direction to be wrong in.
export async function cacheFirst(load: () => Promise<unknown>): Promise<boolean> {
  const win = { stale: false };
  cacheWindow = win;
  let p: Promise<unknown>;
  try {
    p = load();
  } finally {
    cacheWindow = null;
  }
  await p;
  return win.stale;
}

export const api = {
  get: <T>(url: string) => request<T>("GET", url),
  post: <T>(url: string, body?: unknown) => request<T>("POST", url, body),
  put: <T>(url: string, body?: unknown) => request<T>("PUT", url, body),
  // A body only where the route names its target in it (`DELETE /acl`).
  del: <T>(url: string, body?: unknown) => request<T>("DELETE", url, body),
  postRaw: <T = unknown>(url: string, body: Blob | ArrayBuffer | Uint8Array, contentType = "application/octet-stream") =>
    fetch(url, {
      method: "POST",
      credentials: "same-origin",
      headers: { "Content-Type": contentType },
      body: body as BodyInit,
    }).then(handle<T>),
  // Raw XML body: the daemon's define/create endpoints read a bare String, so
  // JSON-stringifying would wrap the XML in quotes and libvirt rejects it.
  postXml: <T = unknown>(url: string, xml: string) => sendXml<T>("POST", url, xml),
  putXml: <T = unknown>(url: string, xml: string) => sendXml<T>("PUT", url, xml),
  uploads: () => request<StagedUpload[]>("GET", "/uploads"),
  upload: (id: string) => request<StagedUpload>("GET", `/uploads/${encodeURIComponent(id)}`),
  pushUpload: (id: string) => request<StagedUpload>("POST", `/uploads/${encodeURIComponent(id)}/push`),
  discardUpload: (id: string) => request<StagedUpload>("DELETE", `/uploads/${encodeURIComponent(id)}`),
};

/// Upload a file with progress.
///
/// `fetch` cannot report upload progress, so this is the one place that uses
/// XMLHttpRequest: without byte-level feedback a multi-GB ISO looks hung.
/// Returns the in-flight promise plus an abort handle for a Cancel button.
export function uploadWithProgress(
  url: string,
  file: Blob,
  onProgress: (sent: number, total: number) => void,
): { promise: Promise<StagedUpload>; cancel: () => void } {
  const xhr = new XMLHttpRequest();
  const promise = new Promise<StagedUpload>((resolve, reject) => {
    xhr.open("POST", url);
    xhr.withCredentials = true;
    xhr.setRequestHeader("Content-Type", "application/octet-stream");
    xhr.upload.onprogress = (e) => {
      if (e.lengthComputable) onProgress(e.loaded, e.total);
    };
    xhr.onload = () => {
      if (xhr.status >= 200 && xhr.status < 300) {
        try {
          resolve(JSON.parse(xhr.responseText) as StagedUpload);
        } catch {
          reject(new ApiError(xhr.status, "malformed response from server"));
        }
        return;
      }
      let message = xhr.statusText || `HTTP ${xhr.status}`;
      try {
        message = (JSON.parse(xhr.responseText) as { error?: string }).error ?? message;
      } catch {
        // Non-JSON error body; the status text is the best we have.
      }
      reject(new ApiError(xhr.status, message));
    };
    xhr.onerror = () => reject(new ApiError(0, "network error during upload"));
    xhr.onabort = () => reject(new ApiError(0, "upload cancelled"));
    xhr.send(file);
  });
  return { promise, cancel: () => xhr.abort() };
}

function sendXml<T>(method: string, url: string, xml: string): Promise<T> {
  return fetch(url, {
    method,
    credentials: "same-origin",
    headers: { "Content-Type": "application/xml" },
    body: xml,
  }).then(handle<T>);
}

// ---------------------------------------------------------------- types

export interface Limit {
  vms?: number | null;
  vcpus?: number | null;
  memory_mb?: number | null;
  storage_gb?: number | null;
  networks?: number | null;
}

/// Per-user resource limits: `default` on every daemon separately, or pooled
/// across all of them when `shared`; `daemons` overrides one daemon.
export interface Quota {
  shared: boolean;
  default: Limit;
  daemons: Record<string, Limit>;
}

export interface User {
  id: string;
  username: string;
  nickname: string;
  avatar: string | null;
  /// Only on /auth/me: what this user may do at `/`. Cosmetic -- the API
  /// checks every call itself.
  admin?: boolean;
  privs?: string[];
  quota?: Quota;
  /// Only on the admin user list.
  groups?: string[];
}

export interface RoleDef {
  name: string;
  builtin: boolean;
  privs: string[];
}

export interface GroupDef {
  name: string;
  comment: string;
  members: string[];
}

export interface AclEntry {
  path: string;
  ugid: string;
  role: string;
  propagate: boolean;
  label?: string;
}

/// A row of the admin Resources page.
export interface AdminResource {
  kind: "vm" | "network" | "pool" | "volume";
  conn: string;
  conn_name: string;
  name: string;
  pool?: string;
  owner?: string;
  owner_name?: string | null;
  suspended?: boolean;
  state?: string;
  vcpus?: number;
  memory_mb?: number;
  capacity?: number;
  forward?: string;
  ip?: string;
  bridge?: string;
  allocation?: number;
}

export interface IpRow {
  conn: string;
  conn_name: string;
  network: string;
  mac: string;
  ip: string;
  name?: string | null;
  dynamic?: boolean;
  vm_name?: string | null;
  owner?: string;
  owner_name?: string | null;
}

export interface Connection {
  id: string;
  name: string;
  url: string;
  fingerprint: string | null;
  has_token: boolean;
  has_cert: boolean;
  /// Reachability, from the web server's own probes (health.rs).
  online?: boolean;
  offline_since_ms?: number | null;
  offline_error?: string | null;
}

/// `POST /connections` result. When the daemon is not reachable yet, the
/// backend saves the connection pending a cert and hands back an install
/// script carrying the freshly generated token.
export interface CreateConnectionResult extends Connection {
  daemon_unreachable?: boolean;
  install_script?: string;
}

export interface ApiKey {
  id: string;
  name: string;
  prefix: string;
  user_id?: string;
  privs?: string[];
  key?: string;
}

export interface DomainSummary {
  name: string;
  title?: string;
  id: number;
  state: string;
  active: boolean;
  persistent: boolean;
  vcpus: number;
  memory_mb: number;
  /// Set by the web server's proxy: who owns it, and whether it is the caller.
  owner_name?: string;
  mine?: boolean;
  suspended?: boolean;
}

export interface Graphics {
  kind: string;
  host: string;
  port: number;
}

export interface NetworkSummary {
  name: string;
  bridge: string;
  active: boolean;
  persistent: boolean;
  autostart: boolean;
  forward: "nat" | "bridge" | "none" | string;
  ip: string;
}

export interface FirewallRule {
  direction: "in" | "out" | "forward";
  action: "accept" | "drop" | "reject";
  proto: "tcp" | "udp" | "icmp" | "any";
  src_ip: string;
  dst_ip: string;
  src_port: string;
  dst_port: string;
}

export interface FirewallForward {
  wan_ip: string;
  wan_start: number;
  wan_end: number;
  lan_ip: string;
  lan_start: number;
  lan_end: number;
  proto: "tcp" | "udp";
}

export interface FirewallPolicy {
  in: "accept" | "reject" | "drop";
  out: "accept" | "reject" | "drop";
  forward: "accept" | "reject" | "drop";
}

export interface PoolSummary {
  name: string;
  active: boolean;
  persistent: boolean;
  capacity: number | null;
  allocation: number | null;
  available: number | null;
}

export interface VolumeSummary {
  name: string;
  path: string;
  capacity: number;
  allocation: number;
  /// Guests that have it attached (from the daemon).
  used_by?: string[];
  /// From the web server's proxy: the caller owns it; and it was made empty
  /// here, so it may be attached as qcow2.
  mine?: boolean;
  blank?: boolean;
  owner_name?: string;
}

/// One row of the server's persistent, per-user activity log.
export interface ActivityEntry {
  id: string;
  user_id: string;
  username: string;
  label: string;
  status: string;
  message: string;
  /// Unix milliseconds.
  at_ms: number;
}

/// Where a two-phase upload is. Mirrors the backend's `Phase`.
export type UploadPhase = "staged" | "uploading" | "verifying" | "done" | "failed";

/// A file staged on the web server, on its way to the daemon.
export interface StagedUpload {
  id: string;
  conn: string;
  pool: string;
  name: string;
  /// Total bytes staged.
  size: number;
  /// Bytes handed to the daemon so far.
  sent: number;
  phase: UploadPhase;
  sha256: string | null;
  error: string | null;
  at_ms: number;
  /// Average bytes/second of the daemon leg, measured by the server.
  speed_bps: number;
  /// The daemon's own receive and write rates, known once it has answered.
  daemon_recv_bps: number;
  daemon_write_bps: number;
}

// Daemon calls go through the proxy.
export function daemonUrl(connId: string, path: string): string {
  return `/proxy/${connId}${path}`;
}

export function consoleUrl(connId: string, domain: string): string {
  const proto = location.protocol === "https:" ? "wss" : "ws";
  return `${proto}://${location.host}/proxy/${connId}/console/domains/${encodeURIComponent(domain)}`;
}
