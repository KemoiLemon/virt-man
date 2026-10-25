// The pool page's Transfers: browser uploads staged on the web server and URL
// downloads run by the daemon. Both are views of server-side state, polled, so
// navigating away mid-transfer is safe. Their state is private to this module;
// the one thing it needs from the app, repainting the pool page, comes in
// through `transferHooks` (set once at start-up) to avoid an import cycle.
import { api, uploadWithProgress } from "./api";
import type { StagedUpload } from "./api";
import { $, setHtml } from "./dom";
import { openModal, toast } from "./dialogs";
import { ic } from "./icons";
import { shownPool, state } from "./state";
import { avgRateKib, fmtBytes, fmtRate } from "./stats";
import { autoVolumeName, esc, volumeNameFromFile } from "./xml";

export const transferHooks = {
  renderPoolPage: (): Promise<void> | void => {},
};

// ---------------------------------------------------------------- uploads
//
// A staged transfer runs to completion on the server whether or not this page
// is open, so this list is a *view* of the server's state rather than a
// client-side task queue: it is refetched, and only the transfers section is
// redrawn. That is why navigating away mid-transfer is safe.

/// Uploads known to the server, oldest first.
let activeUploads: StagedUpload[] = [];
let uploadTimer: number | null = null;
/// Last phase seen per id, so a completion is announced exactly once.
const uploadSeen = new Map<string, string>();

function isLiveUpload(u: StagedUpload): boolean {
  return u.phase === "staged" || u.phase === "uploading" || u.phase === "verifying";
}

const PHASE_LABEL: Record<string, string> = {
  staged: "Ready",
  uploading: "To daemon",
  verifying: "Verifying",
  done: "Stored",
  failed: "Failed",
};

/// Start the poll timer. Separate from `pollUploads` so a poll can restart the
/// timer without recursing into another poll.
function startUploadPoll() {
  if (uploadTimer !== null) return;
  uploadTimer = window.setInterval(() => void pollUploads(), 700);
}

function stopUploadPoll() {
  if (uploadTimer === null) return;
  window.clearInterval(uploadTimer);
  uploadTimer = null;
}

// Same race as pollFetches: the timer and every action start their own poll,
// and an older snapshot landing last made the byte count step backwards.
let uploadPollSeq = 0;
let uploadPollApplied = 0;

export async function pollUploads() {
  if (!state.selectedConn) return stopUploadPoll();
  const seq = ++uploadPollSeq;
  let jobs: StagedUpload[];
  try {
    jobs = await api.uploads();
  } catch {
    return; // transient; the running timer retries
  }
  if (seq < uploadPollApplied) return;
  uploadPollApplied = seq;
  activeUploads = jobs;

  for (const j of jobs) {
    const before = uploadSeen.get(j.id);
    uploadSeen.set(j.id, j.phase);
    if (before === j.phase) continue;
    if (j.phase === "done") {
      // The daemon's copy is verified and the staged file is gone.
      toast(`${j.name} stored on the daemon`);
      if (state.selectedPool === j.pool) void transferHooks.renderPoolPage();
    } else if (j.phase === "failed") {
      toast(`${j.name}: ${j.error ?? "transfer failed"}`, "bad");
    }
  }
  if (jobs.some(isLiveUpload)) startUploadPoll();
  else stopUploadPoll();
  refreshUploadBars();
}

/// One card per transfer, reusing the pool page's metric card so no new
/// styling is introduced for this.
export function uploadsSection(pool: string): string {
  const jobs = activeUploads.filter((j) => j.pool === pool);
  if (!jobs.length) return "";
  return `
    ${jobs.map((j) => {
      const live = isLiveUpload(j);
      const failed = j.phase === "failed";
      const cancelled = failed && j.error === "Cancelled";
      const p = j.size > 0 ? Math.min(100, (j.sent / j.size) * 100) : 0;
      // A finished transfer reads as full even if the byte count ran short.
      const width = live ? p : 100;
      // `speed_bps` is bytes/s; fmtRate speaks KiB/s.
      const speed = j.speed_bps > 0 ? fmtRate(j.speed_bps / 1024) : "";
      return `<div class="xfer" data-up-id="${esc(j.id)}">
        <div class="metric-head">
          <span class="metric-ic">${ic.upload()}</span>
          <b>${esc(j.name)}</b>
          ${j.phase === "staged" ? `<button class="btn" data-up-push="${esc(j.id)}">Send to Daemon</button>` : ""}
          ${j.phase === "verifying" ? "" : `<button class="btn" data-up-${live ? "cancel" : "clear"}="${esc(j.id)}">${live ? "Cancel" : "Dismiss"}</button>`}
        </div>
        <div class="meter"><div class="meter-fill ${failed && !cancelled ? "hot" : ""}" style="width:${width.toFixed(1)}%"></div></div>
        <div class="metric-foot">
          <span>${live ? `${fmtBytes(j.sent)} of ${fmtBytes(j.size)}` : fmtBytes(j.size)}</span>
          <span>${failed ? (cancelled ? "Cancelled" : "Failed") : j.phase === "uploading" ? speed : esc(PHASE_LABEL[j.phase] ?? j.phase)}</span>
        </div>
      </div>`;
    }).join("")}`;
}

/// Redraw only the transfers section, so polling never rebuilds the volume
/// table underneath whoever is using it.
function refreshUploadBars() {
  const host = document.getElementById("pool-uploads");
  if (!host || !state.selectedPool) return;
  setHtml(host, uploadsSection(state.selectedPool));
  bindUploads();
  syncTransfersTitle();
}

export function bindUploads() {
  const host = document.getElementById("pool-uploads");
  if (!host) return;
  const drop = async (id: string, keep: boolean) => {
    try {
      await api.discardUpload(id);
    } catch (e) {
      return toast((e as Error).message, "bad");
    }
    uploadSeen.delete(id);
    if (keep) uploadSeen.set(id, "gone");
    await pollUploads();
  };
  for (const b of host.querySelectorAll<HTMLElement>("[data-up-cancel]")) {
    b.onclick = () => void drop(b.dataset.upCancel!, true);
  }
  for (const b of host.querySelectorAll<HTMLElement>("[data-up-clear]")) {
    b.onclick = () => void drop(b.dataset.upClear!, true);
  }
  // A job that came back from a previous run is already staged on disk, so the
  // bytes do not need to be sent again: one click restarts the daemon leg.
  for (const b of host.querySelectorAll<HTMLElement>("[data-up-push]")) {
    b.onclick = async () => {
      const id = b.dataset.upPush!;
      b.setAttribute("disabled", "true");
      try {
        await api.pushUpload(id);
      } catch (e) {
        b.removeAttribute("disabled");
        return toast((e as Error).message, "bad");
      }
      await pollUploads();
    };
  }
}

// ------------------------------------------------------------ URL fetches

interface FetchJob {
  id: string; pool: string; name: string; url: string;
  phase: "downloading" | "verifying" | "writing" | "done" | "failed" | "cancelled";
  received: number; total: number | null; speed_bps: number;
  sha256: string | null; error: string | null;
}
/// Last known fetches, per server. The daemon owns the jobs; this is only
/// what the pool page last saw.
const fetchJobs = new Map<string, FetchJob[]>();
const fetchSeen = new Map<string, string>();
let fetchTimer: number | null = null;
const FETCH_LABEL: Record<string, string> = {
  downloading: "Downloading", verifying: "Verifying", writing: "Writing",
  done: "Stored", failed: "Failed", cancelled: "Cancelled",
};
const liveFetch = (j: FetchJob) => j.phase === "downloading" || j.phase === "verifying" || j.phase === "writing";

/// Ask the daemon to download an ISO into `pool` itself. The bytes never pass
/// through this server or the browser, and the job survives closing the tab.
export function openFetch(pool: string) {
  if (!state.selectedConn) return;
  const conn = state.selectedConn;
  const el = openModal(
    `Download ISO into ${esc(shownPool(pool))}`,
    `<label class="field"><span>URL</span>
       <input id="fetch-url" placeholder="https://…/image.iso" spellcheck="false"></label>
     <label class="field"><span>Volume Name</span>
       <input id="fetch-name" placeholder="Leave blank to use the file name"></label>
     <label class="field"><span>SHA-256 (optional)</span>
       <input id="fetch-sha" placeholder="Checked before the image is stored" spellcheck="false"></label>`,
    `<button class="btn primary" id="fetch-go">Download</button>`,
  );
  const url = $("#fetch-url", el) as HTMLInputElement;
  const name = $("#fetch-name", el) as HTMLInputElement;
  // Suggest the URL's file name, but never overwrite one the user typed.
  let autoName = "";
  url.addEventListener("input", () => {
    let file = "";
    try { file = decodeURIComponent(new URL(url.value.trim()).pathname.split("/").pop() ?? ""); } catch { /* not a URL yet */ }
    const next = autoVolumeName(name.value, autoName, file);
    if (next === null) return;
    name.value = next;
    autoName = next;
  });
  const go = $("#fetch-go", el) as HTMLButtonElement;
  go.onclick = async () => {
    // Same rule the daemon applies, said before the round trip.
    const target = name.value.trim() || decodeURIComponent(url.value.trim().split(/[?#]/)[0].split("/").pop() ?? "");
    if (!/\.iso$/i.test(target)) return toast("Only .iso files can be downloaded", "bad");
    go.disabled = true;
    try {
      await api.post(`/proxy/${conn}/pools/${encodeURIComponent(pool)}/fetch`, {
        url: url.value.trim(),
        name: name.value.trim() || undefined,
        sha256: ($("#fetch-sha", el) as HTMLInputElement).value.trim() || undefined,
      });
    } catch (e) {
      go.disabled = false;
      return toast((e as Error).message, "bad");
    }
    el.remove();
    toast("Download started on the server");
    await pollFetches(conn);
  };
  url.focus();
}

// Polls overlap (the 1 s timer plus renders and actions each fire their own),
// and responses can land out of order; the daemon's counter only grows, so an
// older snapshot arriving last is what made the figure step backwards.
let fetchPollSeq = 0;
let fetchPollApplied = 0;

export async function pollFetches(conn: string) {
  const seq = ++fetchPollSeq;
  let jobs: FetchJob[];
  try {
    jobs = await api.get<FetchJob[]>(`/proxy/${conn}/fetches`);
  } catch {
    return; // transient, or the daemon is down: the next poll retries
  }
  if (seq < fetchPollApplied) return;
  fetchPollApplied = seq;
  fetchJobs.set(conn, jobs);
  for (const j of jobs) {
    const before = fetchSeen.get(j.id);
    fetchSeen.set(j.id, j.phase);
    if (before === undefined || before === j.phase) continue;
    if (j.phase === "done") {
      toast(`${j.name} downloaded and stored`);
      void api.post("/activity", { label: `Download ${j.name} into pool ${j.pool}`, status: "done", message: j.url }).catch(() => {});
      if (state.selectedPool === j.pool && state.selectedConn === conn) void transferHooks.renderPoolPage();
    } else if (j.phase === "failed") {
      toast(`${j.name}: ${j.error ?? "download failed"}`, "bad");
      void api.post("/activity", { label: `Download ${j.name} into pool ${j.pool}`, status: "failed", message: `${j.error ?? ""} (${j.url})` }).catch(() => {});
    }
  }
  if (jobs.some(liveFetch)) {
    if (fetchTimer === null) fetchTimer = window.setInterval(() => void pollFetches(conn), 1000);
  } else if (fetchTimer !== null) {
    window.clearInterval(fetchTimer);
    fetchTimer = null;
  }
  const host = document.getElementById("pool-fetches");
  if (host && state.selectedPool && state.selectedConn === conn) {
    setHtml(host, fetchesSection(conn, state.selectedPool));
    bindFetches(conn);
    syncTransfersTitle();
  }
}

/// The master Transfers card says so when neither list has a row.
function syncTransfersTitle() {
  const empty = document.getElementById("pool-no-transfers");
  if (!empty) return;
  empty.hidden = !!document.getElementById("pool-uploads")?.children.length
    || !!document.getElementById("pool-fetches")?.children.length;
}

export function fetchesSection(conn: string, pool: string): string {
  const jobs = (fetchJobs.get(conn) ?? []).filter((j) => j.pool === pool);
  if (!jobs.length) return "";
  return `
    ${jobs.map((j) => {
      const live = liveFetch(j);
      const pctDone = j.total ? Math.min(100, (j.received / j.total) * 100) : 0;
      const width = live ? (j.total ? pctDone : 0) : 100;
      return `<div class="xfer" data-fetch-id="${esc(j.id)}">
        <div class="metric-head">
          <span class="metric-ic">${ic.download()}</span>
          <b>${esc(j.name)}</b>
          <button class="btn" data-fetch-${live ? "cancel" : "clear"}="${esc(j.id)}">${live ? "Cancel" : "Dismiss"}</button>
        </div>
        <div class="meter"><div class="meter-fill ${j.phase === "failed" ? "hot" : ""}" style="width:${width.toFixed(1)}%"></div></div>
        <div class="metric-foot">
          <span>${live ? `${fmtBytes(j.received)}${j.total ? ` of ${fmtBytes(j.total)}` : ""}` : fmtBytes(j.total ?? j.received)}</span>
          <span>${j.phase !== "downloading" ? esc(FETCH_LABEL[j.phase] ?? j.phase) : j.speed_bps > 0 ? fmtRate(j.speed_bps / 1024) : ""}</span>
        </div>
      </div>`;
    }).join("")}`;
}

export function bindFetches(conn: string) {
  const host = document.getElementById("pool-fetches");
  if (!host) return;
  for (const b of host.querySelectorAll<HTMLElement>("[data-fetch-cancel], [data-fetch-clear]")) {
    b.onclick = async () => {
      const id = b.dataset.fetchCancel ?? b.dataset.fetchClear!;
      try {
        await api.del(`/proxy/${conn}/fetches/${id}`);
      } catch (e) {
        return toast((e as Error).message, "bad");
      }
      await pollFetches(conn);
    };
  }
}

export function openUpload(pool: string) {
  if (!state.selectedConn) return;
  const conn = state.selectedConn;
  const el = openModal(
    `Upload ISO to ${esc(shownPool(pool))}`,
    `<label class="field"><span>Volume Name</span>
       <input id="up-name" placeholder="Leave blank to use the file name"></label>
     <label class="field"><span>File</span><input id="up-file" type="file" accept=".iso,application/x-iso9660-image"></label>
     <div class="metric" id="up-bar" hidden>
       <div class="metric-head">
         <span class="metric-ic">${ic.upload()}</span>
         <b id="up-status">Uploading to server</b>
       </div>
       <div class="meter"><div class="meter-fill" id="up-fill" style="width:0%"></div></div>
       <div class="metric-foot"><span id="up-bytes" style="min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap"></span><span id="up-rate" style="white-space:nowrap"></span></div>
     </div>`,
    `<button class="btn" id="up-cancel" hidden>Cancel</button>
     <button class="btn primary" id="up-go">Upload</button>`,
  );

  const nameInput = $("#up-name", el) as HTMLInputElement;
  const fileInput = $("#up-file", el) as HTMLInputElement;
  const goBtn = $("#up-go", el) as HTMLButtonElement;
  const cancelBtn = $("#up-cancel", el) as HTMLButtonElement;
  const bar = $("#up-bar", el) as HTMLElement;
  const fill = $("#up-fill", el) as HTMLElement;
  const status = $("#up-status", el) as HTMLElement;
  const bytes = $("#up-bytes", el) as HTMLElement;
  const rateEl = $("#up-rate", el) as HTMLElement;
  // The name we filled in from a file, so we can tell an untouched field from
  // one the user typed in: re-picking a file must not clobber a custom name.
  let autoName = "";
  fileInput.addEventListener("change", () => {
    const file = fileInput.files?.[0];
    if (!file) return;
    const next = autoVolumeName(nameInput.value, autoName, file.name);
    if (next === null) return; // the user typed a name: leave it alone
    nameInput.value = next;
    autoName = next;
  });

  goBtn.onclick = async () => {
    const file = fileInput.files?.[0];
    if (!file) return toast("Pick a file to upload", "bad");
    // `accept` is only a hint: the picker's "All files" option still bypasses it.
    if (!/\.iso$/i.test(file.name)) return toast("Only .iso files can be uploaded", "bad");
    // Blank means "name it after the file", so the field is optional.
    const name = nameInput.value.trim() || volumeNameFromFile(file.name);
    if (!name) return toast("Name is required", "bad");

    // Phase 1: browser -> this server. The file has to land here first, because
    // holding the bytes is what lets the server verify the daemon afterwards.
    goBtn.disabled = true;
    nameInput.disabled = true;
    bar.hidden = false;
    cancelBtn.hidden = false;
    status.textContent = "Preparing…";
    fill.classList.remove("hot");
    bytes.textContent = rateEl.textContent = "";
    bytes.title = "";

    // Timed from the moment the body starts leaving the browser, so the rate
    // reflects this hop only (the browser -> server leg).
    const startedAt = Date.now();
    const { promise, cancel } = uploadWithProgress(
      `/uploads?conn=${encodeURIComponent(conn)}&pool=${encodeURIComponent(pool)}&name=${encodeURIComponent(name)}`,
      file,
      (sent, total) => {
        const p = total > 0 ? (sent / total) * 100 : 0;
        fill.style.width = `${p.toFixed(1)}%`;
        // Null until it is worth showing, rather than a misleading 0 KiB/s.
        const rate = avgRateKib(sent, startedAt, Date.now());
        status.textContent = "Uploading to server";
        bytes.textContent = `${fmtBytes(sent)} of ${fmtBytes(total)}`;
        rateEl.textContent = rate === null ? "" : fmtRate(rate);
      },
    );
    cancelBtn.onclick = () => cancel();

    let staged: StagedUpload;
    try {
      staged = await promise;
    } catch (e) {
      // Keep the dialog open so the user can retry without re-picking the file.
      goBtn.disabled = false;
      nameInput.disabled = false;
      cancelBtn.hidden = true;
      status.textContent = "Failed";
      fill.classList.add("hot");
      // The reason is in the toast, which stays up.
      bytes.textContent = fmtBytes(file.size);
      rateEl.textContent = "Error";
      return toast(`Upload failed: ${(e as Error).message}`, "bad");
    }

    // Phase 2 runs on the server, so the dialog can close. Progress moves to
    // the pool page, where it survives navigating away and back.
    el.remove();
    toast(`Uploaded ${name} to the server — sending to daemon`);
    startUploadPoll();
    try {
      await api.pushUpload(staged.id);
    } catch (e) {
      toast(`Could not start the daemon transfer: ${(e as Error).message}`, "bad");
    }
    await pollUploads();
  };
}
