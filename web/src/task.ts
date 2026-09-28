// Long daemon operations (create VM, ISO upload) report progress through a
// task. The task itself is transient: it advances running -> done/failed and
// hands `set` to the caller for inline status. The outcome is also written to
// the server's persistent activity log (`POST /activity`), which is what the
// admin panel shows — that log is attributed per user and survives reload.
//
// There is deliberately no in-memory list any more: a second copy that only
// the current tab could see, and that vanished on reload, added nothing.

import { api } from "./api";

export interface TaskOpts {
  /// Told to the user when the work actually finished.
  done?: string;
  /// Shows the outcome. Separate so this module needs no DOM.
  notify?: (msg: string, kind: "ok" | "bad") => void;
}

/// Run `work`, record its outcome, and tell the user how it ended.
///
/// Callers used to toast "created" right after starting the work and let the
/// promise go: a create the server refused (a quota, a permission) still said
/// it succeeded, and the reason only reached the activity log. The outcome is
/// reported here, once the work has settled, so no caller can get it wrong.
/// The returned promise resolves to whether it succeeded and never rejects.
export function runTask(
  label: string,
  work: (set: (msg: string) => void) => Promise<void>,
  opts: TaskOpts = {},
): Promise<boolean> {
  // Reporting is best effort: failing to write the log must never turn a
  // successful operation into a failed one, so the error is swallowed.
  const record = (status: "done" | "failed", message: string) => {
    void api.post("/activity", { label, status, message }).catch(() => undefined);
  };

  let latest = "…";
  return work((msg) => {
    latest = msg;
  })
    .then(() => {
      record("done", latest);
      if (opts.done) opts.notify?.(opts.done, "ok");
      return true;
    })
    .catch((e) => {
      const msg = e instanceof Error ? e.message : String(e);
      record("failed", msg);
      opts.notify?.(`${label} failed: ${msg}`, "bad");
      return false;
    });
}
