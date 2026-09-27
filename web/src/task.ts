// Long daemon operations (create VM, ISO upload) report progress through a
// task. The task itself is transient: it advances running -> done/failed and
// hands `set` to the caller for inline status. The outcome is also written to
// the server's persistent activity log (`POST /activity`), which is what the
// admin panel shows — that log is attributed per user and survives reload.
//
// There is deliberately no in-memory list any more: a second copy that only
// the current tab could see, and that vanished on reload, added nothing.

import { api } from "./api";

export function runTask(label: string, work: (set: (msg: string) => void) => Promise<void>) {
  // Reporting is best effort: failing to write the log must never turn a
  // successful operation into a failed one, so the error is swallowed.
  const record = (status: "done" | "failed", message: string) => {
    void api.post("/activity", { label, status, message }).catch(() => undefined);
  };

  let latest = "…";
  return work((msg) => {
    latest = msg;
  })
    .then(() => record("done", latest))
    .catch((e) => {
      record("failed", e instanceof Error ? e.message : String(e));
      throw e;
    });
}
