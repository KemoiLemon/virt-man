// Shared app state. ES modules cannot reassign another module's `let`, so the
// variables that views read and write live on one exported object; a view
// module imports `state` and reads `state.selectedConn` like it did the bare
// variable. State private to one section (the sidebar, uploads, fetches) stays
// in that section's own module.
import type { Connection, User } from "./api";
import type { ConnTab, DcTab } from "./route";

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
};
