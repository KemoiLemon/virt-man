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

function request<T>(method: string, url: string, body?: unknown): Promise<T> {
  const hasBody = body !== undefined;
  return fetch(url, {
    method,
    credentials: "same-origin",
    headers: hasBody ? { "Content-Type": "application/json" } : undefined,
    body: hasBody ? JSON.stringify(body) : undefined,
  }).then(handle<T>);
}

export const api = {
  get: <T>(url: string) => request<T>("GET", url),
  post: <T>(url: string, body?: unknown) => request<T>("POST", url, body),
  put: <T>(url: string, body?: unknown) => request<T>("PUT", url, body),
  del: <T>(url: string) => request<T>("DELETE", url),
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

export type Permission = "read" | "write" | "none";

export interface User {
  id: string;
  /// Role name, resolved against the stored roles. Custom roles are allowed,
  /// so this is a plain string rather than the old three-value union.
  role: string;
  username: string;
  nickname: string;
  avatar: string | null;
  scopes: Record<string, Permission>;
}

export interface RoleDef {
  name: string;
  builtin: boolean;
  scopes: Record<string, Permission>;
}

export interface Connection {
  id: string;
  name: string;
  url: string;
  fingerprint: string | null;
  has_token: boolean;
  has_cert: boolean;
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
  scopes?: Record<string, Permission>;
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
}

// Daemon calls go through the proxy.
export function daemonUrl(connId: string, path: string): string {
  return `/proxy/${connId}${path}`;
}

export function consoleUrl(connId: string, domain: string): string {
  const proto = location.protocol === "https:" ? "wss" : "ws";
  return `${proto}://${location.host}/proxy/${connId}/console/domains/${encodeURIComponent(domain)}`;
}
