// Callbacks that break import cycles between the views. A module that needs a
// view it cannot import (because that view imports it, directly or through the
// navigation core) calls it through `hooks`. main.ts assigns the real
// functions once at start-up, as wrappers, so declaration order cannot matter.
// The defaults are inert: nothing runs before main.ts has wired them.
// eslint-disable-next-line @typescript-eslint/no-explicit-any
type AnyFn = (...args: any[]) => any;

export const hooks: {
  renderTree: AnyFn;
  renderOffline: AnyFn;
  isOffline: (conn: string) => boolean;
  canAdmin: () => boolean;
  openAdmin: AnyFn;
  closeAdmin: AnyFn;
  renderDc: AnyFn;
  loadDcDashboard: AnyFn;
  renderConn: AnyFn;
  loadDashboard: AnyFn;
  renderVm: AnyFn;
  renderVmBody: AnyFn;
  renderNetPage: AnyFn;
  renderPoolPage: AnyFn;
  refreshTreeVms: AnyFn;
  loadNetworks: AnyFn;
  loadPools: AnyFn;
  loadVolumes: AnyFn;
  deleteCtOn: AnyFn;
  dcCreate: AnyFn;
} = {
  renderTree: () => {},
  renderOffline: () => {},
  isOffline: () => false,
  canAdmin: () => false,
  openAdmin: () => {},
  closeAdmin: () => {},
  renderDc: () => {},
  loadDcDashboard: () => {},
  renderConn: () => {},
  loadDashboard: () => {},
  renderVm: () => {},
  renderVmBody: () => {},
  renderNetPage: () => {},
  renderPoolPage: () => {},
  refreshTreeVms: () => {},
  loadNetworks: () => {},
  loadPools: () => {},
  loadVolumes: () => {},
  deleteCtOn: () => {},
  dcCreate: () => {},
};
