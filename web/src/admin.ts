// The administrative area: a page of its own with its own sidebar (Users,
// Groups, Roles, Permissions, Daemons, Resources, Quotas, IPs, API keys,
// Activity). main.ts owns the shell, routing and the poll; this module paints
// the sidebar and one section, through helpers main.ts hands it (`AdminCtx`),
// so it needs no import back into main.ts.
//
// Every write here is checked by the server (`Permissions.Modify` at `/`).
// Nothing is hidden from a non-admin by this file alone; they never get here
// because the server refuses every call it makes.

import { api } from "./api";
import type {
  AclEntry, AdminResource, ApiKey, ActivityEntry, Connection, GroupDef, IpRow,
  Limit, Quota, RoleDef, User,
} from "./api";
import { ic } from "./icons";
import { skeleton } from "./skeleton";
import { fmtBytes } from "./stats";
import { esc } from "./xml";
import type { AdminSection } from "./route";

export interface AdminCtx {
  setHtml(el: Element, html: string): void;
  $(sel: string, root?: ParentNode): any;
  openModal(title: string, body: string, foot: string): HTMLElement;
  askConfirm(title: string, message: string, okLabel?: string, danger?: boolean): Promise<boolean>;
  toast(msg: string, kind?: "ok" | "bad"): void;
  showSecret(title: string, secret: string): void;
  relTime(ms: number): string;
  me(): User | null;
  connections(): Connection[];
  /// Re-read the daemon list from the server (another admin, or another tab,
  /// may have changed it since this page loaded).
  reloadConnections(): Promise<void>;
  openConnectionDialog(id?: string): void;
  deleteConnection(id: string): Promise<void>;
  /// Open a guest or network in the main view.
  openResource(kind: "vm" | "network" | "pool", conn: string, name: string): void;
  /// Go to another admin section (through the router, so the URL follows).
  go(section: AdminSection): void;
  /// The identity changed (your own groups, roles): refresh it and re-render.
  refreshMe(): Promise<void>;
  /// Whether the pane still shows the render that started at `gen`.
  stillCurrent(gen: number): boolean;
}

const LABEL: Record<AdminSection, string> = {
  dashboard: "Dashboard",
  users: "Users",
  groups: "Groups",
  roles: "Roles",
  permissions: "Permissions",
  daemons: "Daemons",
  resources: "Resources",
  quotas: "Quotas",
  ips: "IP Addresses",
  keys: "API Keys",
  activity: "Activity",
};

const ICON: Record<AdminSection, (s?: number) => string> = {
  dashboard: ic.datacenter,
  users: ic.user,
  groups: ic.groups,
  roles: ic.roles,
  permissions: ic.perms,
  daemons: ic.server,
  resources: ic.vms,
  quotas: ic.quota,
  ips: ic.ip,
  keys: ic.key,
  activity: ic.autostart,
};

/// The sidebar's groups. Every section is in exactly one (asserted by the
/// live suite against ADMIN_SECTIONS), in the order they are listed.
export const ADMIN_GROUPS: { label: string; sections: AdminSection[] }[] = [
  { label: "Access", sections: ["users", "groups", "roles", "permissions", "keys"] },
  { label: "Infrastructure", sections: ["daemons", "resources", "ips"] },
  { label: "Limits", sections: ["quotas"] },
  { label: "Audit", sections: ["activity"] },
];

/// The admin sidebar, laid out like the resource tree: the Dashboard is the
/// root (as Datacenter is there), and the sections nest under group labels
/// the way guests nest under a server. Rows keep the tree's classes, so the
/// drawer, arrow keys and long-press behave the same.
export function adminTree(section: AdminSection): string {
  const row = (s: AdminSection, cls: string) => `
      <button class="tree-row ${cls} ${s === section ? "sel" : ""}" data-admin-section="${s}">
        <span class="tree-ic">${ICON[s]()}</span>
        <span class="tree-label">${LABEL[s]}</span>
      </button>`;
  return `
    <button class="tree-row tree-root ${section === "dashboard" ? "sel" : ""}" data-admin-section="dashboard">
      <span class="tree-ic">${ic.admin()}</span>
      <span class="tree-label">Administration</span>
    </button>
    ${ADMIN_GROUPS.map((g) => `
      <div class="admin-group" role="presentation">${esc(g.label)}</div>
      ${g.sections.map((s) => row(s, "tree-vm admin-item")).join("")}`).join("")}`;
}

export function adminTitle(section: AdminSection): string {
  return LABEL[section];
}

// ------------------------------------------------------------- shared state

let roles: RoleDef[] = [];
let privileges: string[] = [];
let users: User[] = [];
let groups: GroupDef[] = [];

async function loadMeta(ctx: AdminCtx) {
  await ctx.reloadConnections();
  const [r, u, g] = await Promise.all([
    api.get<{ roles: RoleDef[]; privileges: string[] }>("/roles"),
    api.get<User[]>("/users"),
    api.get<GroupDef[]>("/groups"),
  ]);
  roles = r.roles;
  privileges = r.privileges;
  users = u;
  groups = g;
}

const userName = (id: string) => users.find((u) => u.id === id)?.username ?? id;

/// "user:<id>" / "group:<name>" as a person reads it.
function ugidLabel(ugid: string): string {
  const [kind, id] = ugid.split(":", 2);
  return kind === "user" ? userName(id) : `@${id}`;
}

function option(value: string, label: string, selected: boolean): string {
  return `<option value="${esc(value)}" ${selected ? "selected" : ""}>${esc(label)}</option>`;
}

function fmtLimit(n: number | null | undefined, unit = ""): string {
  return n == null ? "∞" : `${n}${unit}`;
}

/// Paint one admin section into `area`. `gen` guards against a slow load
/// painting over a section the user has since left.
export async function renderAdminSection(ctx: AdminCtx, area: HTMLElement, section: AdminSection, gen: number) {
  ctx.setHtml(area, skeleton("table"));
  try {
    await loadMeta(ctx);
    if (!ctx.stillCurrent(gen)) return;
    await SECTIONS[section](ctx, area, gen);
  } catch (e) {
    if (ctx.stillCurrent(gen)) ctx.setHtml(area, `<div class="empty">${esc((e as Error).message)}</div>`);
  }
}

type Painter = (ctx: AdminCtx, area: HTMLElement, gen: number) => Promise<void>;

const reload = (ctx: AdminCtx, s: AdminSection) => () => ctx.go(s);

function bind(area: Element, attr: string, run: (value: string, el: HTMLElement) => void) {
  area.querySelectorAll(`[${attr}]`).forEach((b) => {
    const el = b as HTMLElement;
    const key = attr.replace(/^data-/, "").replace(/-([a-z])/g, (_, c: string) => c.toUpperCase());
    el.onclick = (e) => {
      e.stopPropagation();
      run(el.dataset[key] ?? "", el);
    };
  });
}

// ------------------------------------------------------------------- users

const users_: Painter = async (ctx, area) => {
  const me = ctx.me();
  ctx.setHtml(area, `
    <div class="actions"><button class="btn primary" id="add-user">${ic.plus(14)} Add user</button></div>
    <table class="grid">
      <tr><th>Username</th><th>Display Name</th><th>Groups</th><th>Limits</th><th class="col-actions"></th></tr>
      ${users.map((u) => `
        <tr>
          <td class="tname">${esc(u.username)}${u.id === me?.id ? ` <span class="badge">you</span>` : ""}${u.admin ? ` <span class="badge on">admin</span>` : ""}</td>
          <td>${esc(u.nickname)}</td>
          <td>${(u.groups ?? []).map((g) => `<span class="role-tag">@${esc(g)}</span>`).join("") || `<span class="dim-note">—</span>`}</td>
          <td>${quotaSummary(u.quota)}</td>
          <td class="col-actions"><div class="row-actions">
            <button class="btn icon" data-edit-user="${u.id}" title="Edit">${ic.edit()}</button>
            <button class="btn icon" data-user-perms="${u.id}" title="Permissions">${ic.perms()}</button>
            <button class="btn icon" data-user-quota="${u.id}" title="Limits">${ic.quota()}</button>
            <button class="btn danger icon" data-del-user="${u.id}" title="Delete">${ic.trash()}</button>
          </div></td>
        </tr>`).join("")}
    </table>`);
  ctx.$("#add-user", area).onclick = () => openUser(ctx, null);
  bind(area, "data-edit-user", (id) => openUser(ctx, users.find((u) => u.id === id) ?? null));
  bind(area, "data-user-perms", (id) => openGrant(ctx, { ugid: `user:${id}` }));
  bind(area, "data-user-quota", (id) => {
    const u = users.find((x) => x.id === id);
    if (u) openQuota(ctx, u);
  });
  bind(area, "data-del-user", async (id) => {
    const u = users.find((x) => x.id === id);
    if (!await ctx.askConfirm("Delete user",
      `Delete ${u?.username ?? "this user"}? Their API keys, sessions and grants go too; what they own passes to an administrator.`,
      "Delete", true)) return;
    try {
      await api.del(`/users/${id}`);
      ctx.toast("User removed");
      reload(ctx, "users")();
    } catch (e) { ctx.toast((e as Error).message, "bad"); }
  });
};

function quotaSummary(q?: Quota): string {
  const d = q?.default ?? {};
  const set = [d.vms, d.vcpus, d.memory_mb, d.storage_gb, d.networks].some((x) => x != null);
  const over = Object.keys(q?.daemons ?? {}).length;
  if (!set && !over) return `<span class="dim-note">Unlimited</span>`;
  const parts = [
    d.vms != null ? `${d.vms} VM` : "",
    d.vcpus != null ? `${d.vcpus} vCPU` : "",
    d.memory_mb != null ? fmtBytes(d.memory_mb * 1048576) : "",
    d.storage_gb != null ? `${d.storage_gb} GiB disk` : "",
  ].filter(Boolean);
  return `${esc(parts.join(" · ") || "—")}${q?.shared ? ` <span class="badge">pooled</span>` : ""}${over ? ` <span class="badge">+${over} daemon</span>` : ""}`;
}

function openUser(ctx: AdminCtx, u: User | null) {
  const el = ctx.openModal(
    u ? `Edit — ${esc(u.username)}` : "Add user",
    `<div class="grid2">
       <label class="field"><span>Username</span><input id="u-name" value="${esc(u?.username ?? "")}" autocomplete="off"></label>
       <label class="field"><span>Display Name</span><input id="u-nick" value="${esc(u?.nickname ?? "")}"></label>
       <label class="field"><span>${u ? "Reset Password (blank = unchanged)" : "Password (min 8)"}</span>
         <input id="u-pass" type="password" autocomplete="new-password"></label>
       ${u ? "" : `<label class="field"><span>Role at /</span><select id="u-role">
         ${option("", "None (grant later)", false)}
         ${roles.map((r) => option(r.name, r.name, r.name === "User")).join("")}
       </select></label>`}
     </div>
     <div class="section-title">Groups</div>
     <div class="check-list">
       ${groups.map((g) => `<label class="check"><input type="checkbox" data-group="${esc(g.name)}"
          ${u?.groups?.includes(g.name) ? "checked" : ""}> ${esc(g.name)}</label>`).join("") || `<span class="dim-note">No groups yet.</span>`}
     </div>
     ${u ? "" : `<p class="hint">"User" can see daemons and pools, and create guests and networks &mdash; so it sees only what it creates. Set limits after creating.</p>`}`,
    `<button class="btn" id="u-cancel">Cancel</button><button class="btn primary" id="u-save">${u ? "Save" : "Create"}</button>`,
  );
  ctx.$("#u-cancel", el).onclick = () => el.remove();
  ctx.$("#u-save", el).onclick = async () => {
    const picked = [...el.querySelectorAll<HTMLInputElement>("[data-group]")]
      .filter((c) => c.checked).map((c) => c.dataset.group!);
    const body: Record<string, unknown> = {
      username: ctx.$("#u-name", el).value,
      nickname: ctx.$("#u-nick", el).value,
      groups: picked,
    };
    const pass = ctx.$("#u-pass", el).value;
    if (pass || !u) body.password = pass;
    try {
      if (u) {
        await api.put(`/users/${u.id}`, body);
      } else {
        body.role = ctx.$("#u-role", el).value;
        await api.post("/users", body);
      }
      el.remove();
      ctx.toast(u ? "User updated" : "User created");
      if (u?.id === ctx.me()?.id) await ctx.refreshMe();
      else reload(ctx, "users")();
    } catch (e) { ctx.toast((e as Error).message, "bad"); }
  };
}

// ------------------------------------------------------------------ groups

const groups_: Painter = async (ctx, area) => {
  ctx.setHtml(area, `
    <div class="actions"><button class="btn primary" id="add-group">${ic.plus(14)} Add group</button></div>
    <table class="grid">
      <tr><th>Group</th><th>Comment</th><th>Members</th><th class="col-actions"></th></tr>
      ${groups.map((g) => `
        <tr>
          <td class="tname">@${esc(g.name)}</td>
          <td>${esc(g.comment || "—")}</td>
          <td>${g.members.map((m) => `<span class="role-tag">${esc(userName(m))}</span>`).join("") || `<span class="dim-note">—</span>`}</td>
          <td class="col-actions"><div class="row-actions">
            <button class="btn icon" data-edit-group="${esc(g.name)}" title="Edit">${ic.edit()}</button>
            <button class="btn icon" data-group-perms="${esc(g.name)}" title="Permissions">${ic.perms()}</button>
            <button class="btn danger icon" data-del-group="${esc(g.name)}" title="Delete">${ic.trash()}</button>
          </div></td>
        </tr>`).join("") || `<tr><td colspan="4" class="empty">No groups. A group gets permissions like a user does, and every member has them.</td></tr>`}
    </table>`);
  ctx.$("#add-group", area).onclick = () => openGroup(ctx, null);
  bind(area, "data-edit-group", (n) => openGroup(ctx, groups.find((g) => g.name === n) ?? null));
  bind(area, "data-group-perms", (n) => openGrant(ctx, { ugid: `group:${n}` }));
  bind(area, "data-del-group", async (n) => {
    if (!await ctx.askConfirm("Delete group", `Delete @${n} and every permission granted to it?`, "Delete", true)) return;
    try {
      await api.del(`/groups/${encodeURIComponent(n)}`);
      ctx.toast("Group deleted");
      reload(ctx, "groups")();
    } catch (e) { ctx.toast((e as Error).message, "bad"); }
  });
};

function openGroup(ctx: AdminCtx, g: GroupDef | null) {
  const el = ctx.openModal(
    g ? `Group — @${esc(g.name)}` : "Add group",
    `<div class="grid2">
       <label class="field"><span>Name</span><input id="g-name" value="${esc(g?.name ?? "")}" placeholder="ops"></label>
       <label class="field"><span>Comment</span><input id="g-comment" value="${esc(g?.comment ?? "")}"></label>
     </div>
     <div class="section-title">Members</div>
     <div class="check-list">
       ${users.map((u) => `<label class="check"><input type="checkbox" data-member="${u.id}"
          ${g?.members.includes(u.id) ? "checked" : ""}> ${esc(u.username)}</label>`).join("")}
     </div>`,
    `<button class="btn" id="g-cancel">Cancel</button><button class="btn primary" id="g-save">Save</button>`,
  );
  ctx.$("#g-cancel", el).onclick = () => el.remove();
  ctx.$("#g-save", el).onclick = async () => {
    const body = {
      name: ctx.$("#g-name", el).value,
      comment: ctx.$("#g-comment", el).value,
      members: [...el.querySelectorAll<HTMLInputElement>("[data-member]")].filter((c) => c.checked).map((c) => c.dataset.member!),
    };
    try {
      if (g) await api.put(`/groups/${encodeURIComponent(g.name)}`, body);
      else await api.post("/groups", body);
      el.remove();
      ctx.toast(g ? "Group updated" : "Group created");
      if (g?.members.includes(ctx.me()?.id ?? "") || body.members.includes(ctx.me()?.id ?? "")) await ctx.refreshMe();
      else reload(ctx, "groups")();
    } catch (e) { ctx.toast((e as Error).message, "bad"); }
  };
}

// ------------------------------------------------------------------- roles

/// Privileges grouped by their prefix, for a readable editor.
function privGroups(): [string, string[]][] {
  const by = new Map<string, string[]>();
  for (const p of privileges) {
    const k = p.split(".")[0];
    by.set(k, [...(by.get(k) ?? []), p]);
  }
  return [...by];
}

const roles_: Painter = async (ctx, area) => {
  ctx.setHtml(area, `
    <div class="actions"><button class="btn primary" id="add-role">${ic.plus(14)} New role</button></div>
    <table class="grid">
      <tr><th>Role</th><th>Privileges</th><th class="col-actions"></th></tr>
      ${roles.map((r) => `
        <tr>
          <td class="tname">${esc(r.name)}${r.builtin ? ` <span class="badge">built-in</span>` : ""}</td>
          <td class="priv-cell">${r.privs.length === privileges.length ? `<span class="perm w">ALL</span>`
            : r.privs.map((p) => `<span class="priv">${esc(p)}</span>`).join("") || `<span class="dim-note">none</span>`}</td>
          <td class="col-actions"><div class="row-actions">
            ${r.builtin ? `<button class="btn icon" data-view-role="${esc(r.name)}" title="View">${ic.logs()}</button>`
              : `<button class="btn icon" data-edit-role="${esc(r.name)}" title="Edit">${ic.edit()}</button>
                 <button class="btn danger icon" data-del-role="${esc(r.name)}" title="Delete">${ic.trash()}</button>`}
          </div></td>
        </tr>`).join("")}
    </table>`);
  ctx.$("#add-role", area).onclick = () => openRole(ctx, null);
  const find = (n: string) => roles.find((r) => r.name === n) ?? null;
  bind(area, "data-edit-role", (n) => openRole(ctx, find(n)));
  bind(area, "data-view-role", (n) => openRole(ctx, find(n)));
  bind(area, "data-del-role", async (n) => {
    if (!await ctx.askConfirm("Delete role", `Delete the role "${n}"?`, "Delete", true)) return;
    try {
      await api.del(`/roles/${encodeURIComponent(n)}`);
      ctx.toast("Role deleted");
      reload(ctx, "roles")();
    } catch (e) { ctx.toast((e as Error).message, "bad"); }
  });
};

function openRole(ctx: AdminCtx, r: RoleDef | null) {
  const ro = r?.builtin ?? false;
  const el = ctx.openModal(
    r ? `Role — ${esc(r.name)}` : "New role",
    `<label class="field"><span>Name</span><input id="r-name" value="${esc(r?.name ?? "")}" ${ro ? "disabled" : ""} placeholder="BackupOperator"></label>
     ${ro ? `<p class="hint">Built-in roles are fixed. Make a new role for custom access.</p>` : ""}
     ${privGroups().map(([grp, ps]) => `
       <div class="section-title">${esc(grp)}</div>
       <div class="check-list">${ps.map((p) => `<label class="check"><input type="checkbox" data-priv="${esc(p)}"
         ${r?.privs.includes(p) ? "checked" : ""} ${ro ? "disabled" : ""}> ${esc(p.split(".")[1])}</label>`).join("")}</div>`).join("")}`,
    ro ? `<button class="btn primary" id="r-cancel">Close</button>`
      : `<button class="btn" id="r-cancel">Cancel</button><button class="btn primary" id="r-save">Save</button>`,
  );
  ctx.$("#r-cancel", el).onclick = () => el.remove();
  const save = ctx.$("#r-save", el);
  if (save) save.onclick = async () => {
    const body = {
      name: ctx.$("#r-name", el).value.trim(),
      privs: [...el.querySelectorAll<HTMLInputElement>("[data-priv]")].filter((c) => c.checked).map((c) => c.dataset.priv!),
    };
    try {
      if (r) await api.put(`/roles/${encodeURIComponent(r.name)}`, body);
      else await api.post("/roles", body);
      el.remove();
      ctx.toast(r ? "Role updated" : "Role created");
      await ctx.refreshMe();
    } catch (e) { ctx.toast((e as Error).message, "bad"); }
  };
}

// ------------------------------------------------------------- permissions

/// The ACL paths an admin can pick, built from what the daemons have, so a
/// grant is made by choosing rather than typing a path.
async function pathChoices(ctx: AdminCtx): Promise<{ path: string; label: string }[]> {
  const out = [
    { path: "/", label: "/ — everything" },
    { path: "/nodes", label: "/nodes — every daemon" },
  ];
  for (const c of ctx.connections()) {
    const node = `/nodes/${c.id}`;
    out.push({ path: node, label: `${c.name} — whole daemon` });
    for (const k of ["vms", "networks", "storage", "bridges"]) {
      out.push({ path: `${node}/${k}`, label: `${c.name} › all ${k}` });
    }
    try {
      const [vms, nets, pools, bridges] = await Promise.all([
        api.get<{ name: string }[]>(`/proxy/${c.id}/domains`),
        api.get<{ name: string }[]>(`/proxy/${c.id}/networks`),
        api.get<{ name: string }[]>(`/proxy/${c.id}/pools`),
        api.get<string[]>(`/proxy/${c.id}/networks/bridges`),
      ]);
      for (const v of vms) out.push({ path: `${node}/vms/${v.name}`, label: `${c.name} › VM ${v.name}` });
      for (const n of nets) out.push({ path: `${node}/networks/${n.name}`, label: `${c.name} › network ${n.name}` });
      for (const p of pools) out.push({ path: `${node}/storage/${p.name}`, label: `${c.name} › pool ${p.name}` });
      for (const b of bridges) out.push({ path: `${node}/bridges/${b}`, label: `${c.name} › bridge ${b}` });
    } catch {
      // Offline daemon: its whole-daemon paths are still offered.
    }
  }
  return out;
}

/// A path as a person reads it: daemon ids become names.
function pathLabel(ctx: AdminCtx, path: string): string {
  const m = path.match(/^\/nodes\/([^/]+)(.*)$/);
  if (!m) return path;
  const c = ctx.connections().find((x) => x.id === m[1]);
  return c ? `/nodes/${c.name}${m[2]}` : path;
}

const permissions_: Painter = async (ctx, area) => {
  const acl = await api.get<AclEntry[]>("/acl");
  const sorted = [...acl].sort((a, b) => a.path.localeCompare(b.path) || a.ugid.localeCompare(b.ugid));
  ctx.setHtml(area, `
    <div class="actions"><button class="btn primary" id="add-acl">${ic.plus(14)} Add permission</button></div>
    <p class="hint">A role granted at a path applies there and below (unless "propagate" is off). The deepest grant for a user wins over one inherited from above, and a user's own grant wins over their groups'. Owners always hold full access to what they create.</p>
    <table class="grid">
      <tr><th>Path</th><th>User / Group</th><th>Role</th><th>Propagate</th><th class="col-actions"></th></tr>
      ${sorted.map((e, i) => `
        <tr>
          <td class="mono">${esc(pathLabel(ctx, e.path))}</td>
          <td>${esc(e.label ?? ugidLabel(e.ugid))}</td>
          <td><span class="role-tag">${esc(e.role)}</span></td>
          <td>${e.propagate ? "Yes" : "No"}</td>
          <td class="col-actions"><button class="btn danger icon" data-del-acl="${i}" title="Remove">${ic.trash()}</button></td>
        </tr>`).join("") || `<tr><td colspan="5" class="empty">No permissions.</td></tr>`}
    </table>`);
  ctx.$("#add-acl", area).onclick = () => openGrant(ctx, {});
  bind(area, "data-del-acl", async (i) => {
    const e = sorted[Number(i)];
    if (!await ctx.askConfirm("Remove permission",
      `Remove ${e.role} for ${e.label ?? ugidLabel(e.ugid)} on ${pathLabel(ctx, e.path)}?`, "Remove", true)) return;
    try {
      await api.del("/acl", e);
      ctx.toast("Permission removed");
      await ctx.refreshMe();
    } catch (err) { ctx.toast((err as Error).message, "bad"); }
  });
};

/// Grant a role. Prefilled from wherever it was opened: a user's or group's
/// row, or a resource's.
export async function openGrant(ctx: AdminCtx, pre: { ugid?: string; path?: string }) {
  if (!roles.length) await loadMeta(ctx);
  const paths = await pathChoices(ctx);
  if (pre.path && !paths.some((p) => p.path === pre.path)) paths.push({ path: pre.path, label: pathLabel(ctx, pre.path) });
  const el = ctx.openModal(
    "Add permission",
    `<label class="field"><span>Path</span><select id="a-path">
        ${paths.map((p) => option(p.path, p.label, p.path === (pre.path ?? "/"))).join("")}
      </select></label>
     <div class="grid2">
       <label class="field"><span>User / Group</span><select id="a-ugid">
         ${users.map((u) => option(`user:${u.id}`, u.username, pre.ugid === `user:${u.id}`)).join("")}
         ${groups.map((g) => option(`group:${g.name}`, `@${g.name}`, pre.ugid === `group:${g.name}`)).join("")}
       </select></label>
       <label class="field"><span>Role</span><select id="a-role">
         ${roles.map((r) => option(r.name, r.name, r.name === "VMUser")).join("")}
       </select></label>
     </div>
     <label class="check"><input type="checkbox" id="a-prop" checked> Propagate to everything below</label>`,
    `<button class="btn" id="a-cancel">Cancel</button><button class="btn primary" id="a-save">Add</button>`,
  );
  ctx.$("#a-cancel", el).onclick = () => el.remove();
  ctx.$("#a-save", el).onclick = async () => {
    try {
      await api.put("/acl", {
        path: ctx.$("#a-path", el).value,
        ugid: ctx.$("#a-ugid", el).value,
        role: ctx.$("#a-role", el).value,
        propagate: ctx.$("#a-prop", el).checked,
      });
      el.remove();
      ctx.toast("Permission added");
      await ctx.refreshMe();
    } catch (e) { ctx.toast((e as Error).message, "bad"); }
  };
}

// ----------------------------------------------------------------- daemons

const daemons_: Painter = async (ctx, area) => {
  const conns = ctx.connections();
  ctx.setHtml(area, `
    <div class="actions"><button class="btn primary" id="add-daemon">${ic.plus(14)} Add daemon</button></div>
    <p class="hint">Users see a daemon when they hold Sys.Audit on it (the "User" role grants it at /). Restrict one with a NoAccess grant on its path.</p>
    <table class="grid">
      <tr><th>Name</th><th>URL</th><th>Status</th><th>Cert</th><th class="col-actions"></th></tr>
      ${conns.map((c) => `
        <tr>
          <td class="tname">${esc(c.name)}</td>
          <td class="mono">${esc(c.url)}</td>
          <td><span class="state ${c.online !== false ? "on" : ""}">${c.online !== false ? "Online" : "Offline"}</span></td>
          <td class="mono">${c.fingerprint ? `${esc(c.fingerprint.slice(0, 17))}…` : "—"}</td>
          <td class="col-actions"><div class="row-actions">
            <button class="btn icon" data-edit-conn="${c.id}" title="Edit">${ic.edit()}</button>
            <button class="btn icon" data-conn-perms="${c.id}" title="Permissions">${ic.perms()}</button>
            <button class="btn danger icon" data-del-conn="${c.id}" title="Remove">${ic.trash()}</button>
          </div></td>
        </tr>`).join("") || `<tr><td colspan="5" class="empty">No daemons yet.</td></tr>`}
    </table>`);
  ctx.$("#add-daemon", area).onclick = () => ctx.openConnectionDialog();
  bind(area, "data-edit-conn", (id) => ctx.openConnectionDialog(id));
  bind(area, "data-conn-perms", (id) => void openGrant(ctx, { path: `/nodes/${id}` }));
  bind(area, "data-del-conn", (id) => void ctx.deleteConnection(id));
};

// --------------------------------------------------------------- resources

let resFilter = { kind: "", owner: "", conn: "" };

const resources_: Painter = async (ctx, area, gen) => {
  const rows = await api.get<AdminResource[]>("/resources");
  if (!ctx.stillCurrent(gen)) return;
  const paint = () => {
    const shown = rows.filter((r) =>
      (!resFilter.kind || r.kind === resFilter.kind)
      && (!resFilter.owner || r.owner === resFilter.owner)
      && (!resFilter.conn || r.conn === resFilter.conn));
    const ownable = (r: AdminResource) => r.kind !== "pool";
    const size = (r: AdminResource) =>
      r.kind === "vm" ? `${r.vcpus ?? "—"} vCPU · ${r.memory_mb ? fmtBytes(r.memory_mb * 1048576) : "—"}`
        : r.kind === "network" ? esc(`${r.forward ?? ""} ${r.ip ?? ""}`)
          : r.capacity != null ? fmtBytes(r.capacity) : "—";
    ctx.setHtml(area, `
      <div class="actions admin-filters">
        <label class="field"><span>Kind</span><select id="rf-kind">
          ${option("", "All", !resFilter.kind)}
          ${["vm", "network", "pool", "volume"].map((k) => option(k, k === "vm" ? "VMs" : `${k[0].toUpperCase()}${k.slice(1)}s`, resFilter.kind === k)).join("")}
        </select></label>
        <label class="field"><span>Owner</span><select id="rf-owner">
          ${option("", "Anyone", !resFilter.owner)}
          ${users.map((u) => option(u.id, u.username, resFilter.owner === u.id)).join("")}
        </select></label>
        <label class="field"><span>Daemon</span><select id="rf-conn">
          ${option("", "All", !resFilter.conn)}
          ${ctx.connections().map((c) => option(c.id, c.name, resFilter.conn === c.id)).join("")}
        </select></label>
        <div class="tabbar-actions">
          <button class="btn" id="res-new-vm">${ic.plus(14)} VM for user</button>
          <button class="btn" id="res-new-net">${ic.plus(14)} Network for user</button>
        </div>
      </div>
      <table class="grid">
        <tr><th>Kind</th><th>Name</th><th>Daemon</th><th>Owner</th><th>State</th><th>Size</th><th class="col-actions"></th></tr>
        ${shown.map((r, i) => `
          <tr class="${r.suspended ? "suspended" : ""}">
            <td>${r.kind === "vm" ? ic.vm() : r.kind === "network" ? ic.network() : r.kind === "pool" ? ic.storage() : ic.disk()}</td>
            <td class="tname ${r.kind === "volume" ? "" : "row-link"}" ${r.kind === "volume" ? "" : `data-open-res="${i}"`}>${esc(r.pool ? `${r.pool}/${r.name}` : r.name)}</td>
            <td>${esc(r.conn_name)}</td>
            <td>${ownable(r) ? esc(r.owner_name ?? "—") : `<span class="dim-note">shared</span>`}</td>
            <td>${r.suspended ? `<span class="state bad">Suspended</span>` : esc(r.state ?? "")}</td>
            <td>${size(r)}</td>
            <td class="col-actions"><div class="row-actions">
              ${ownable(r) ? `<button class="btn icon" data-owner-res="${i}" title="Change owner">${ic.user()}</button>` : ""}
              ${r.kind === "vm" || r.kind === "network" ? `
                <button class="btn icon" data-share-res="${i}" title="Share">${ic.perms()}</button>
                <button class="btn icon ${r.suspended ? "on" : ""}" data-suspend-res="${i}" title="${r.suspended ? "Restore access" : "Suspend access"}">${r.suspended ? ic.start() : ic.suspend()}</button>` : ""}
            </div></td>
          </tr>`).join("") || `<tr><td colspan="7" class="empty">Nothing matches.</td></tr>`}
      </table>`);
    for (const [id, key] of [["#rf-kind", "kind"], ["#rf-owner", "owner"], ["#rf-conn", "conn"]] as const) {
      ctx.$(id, area).onchange = (e: Event) => {
        resFilter = { ...resFilter, [key]: (e.target as HTMLSelectElement).value };
        paint();
      };
    }
    ctx.$("#res-new-vm", area).onclick = () => createFor(ctx, "vm");
    ctx.$("#res-new-net", area).onclick = () => createFor(ctx, "network");
    bind(area, "data-open-res", (i) => {
      const r = shown[Number(i)];
      ctx.openResource(r.kind as "vm" | "network" | "pool", r.conn, r.name);
    });
    bind(area, "data-owner-res", (i) => void changeOwner(ctx, shown[Number(i)]));
    bind(area, "data-share-res", (i) => {
      const r = shown[Number(i)];
      void openGrant(ctx, { path: `/nodes/${r.conn}/${r.kind === "vm" ? "vms" : "networks"}/${r.name}` });
    });
    bind(area, "data-suspend-res", async (i) => {
      const r = shown[Number(i)];
      const to = !r.suspended;
      if (to && !await ctx.askConfirm("Suspend access",
        `Suspend ${r.owner_name ?? "the owner"}'s access to ${r.name}? They keep read-only access; administrators are unaffected.`, "Suspend", true)) return;
      try {
        await api.put("/resources", { kind: r.kind, conn: r.conn, pool: r.pool ?? "", name: r.name, suspended: to });
        r.suspended = to;
        ctx.toast(to ? "Access suspended" : "Access restored");
        paint();
      } catch (e) { ctx.toast((e as Error).message, "bad"); }
    });
  };
  paint();
};

async function changeOwner(ctx: AdminCtx, r: AdminResource) {
  const el = ctx.openModal(
    `Owner — ${esc(r.name)}`,
    `<label class="field"><span>Owner</span><select id="o-user">
       ${users.map((u) => option(u.id, u.username, u.id === r.owner)).join("")}
     </select></label>
     <p class="hint">The owner has full access to it, and it counts against their limits.</p>`,
    `<button class="btn" id="o-cancel">Cancel</button><button class="btn primary" id="o-save">Save</button>`,
  );
  ctx.$("#o-cancel", el).onclick = () => el.remove();
  ctx.$("#o-save", el).onclick = async () => {
    try {
      await api.put("/resources", { kind: r.kind, conn: r.conn, pool: r.pool ?? "", name: r.name, owner: ctx.$("#o-user", el).value });
      el.remove();
      ctx.toast("Owner changed");
      reload(ctx, "resources")();
    } catch (e) { ctx.toast((e as Error).message, "bad"); }
  };
}

/// Create a VM or network *for* someone: pick the owner, then use the normal
/// create flow; the new resource is handed over once it exists. Admins are
/// not held to the owner's limits.
let pendingOwner: { kind: "vm" | "network"; owner: string } | null = null;

export function takePendingOwner(kind: "vm" | "network"): string | null {
  if (pendingOwner?.kind !== kind) return null;
  const o = pendingOwner.owner;
  pendingOwner = null;
  return o;
}

export async function assignOwner(conn: string, kind: "vm" | "network", name: string, owner: string) {
  await api.put("/resources", { kind, conn, pool: "", name, owner });
}

let onCreateFor: ((kind: "vm" | "network") => void) | null = null;
export function setCreateHandler(run: (kind: "vm" | "network") => void) {
  onCreateFor = run;
}

function createFor(ctx: AdminCtx, kind: "vm" | "network") {
  const el = ctx.openModal(
    kind === "vm" ? "Create VM for a user" : "Create network for a user",
    `<label class="field"><span>Owner</span><select id="cf-user">
       ${users.map((u) => option(u.id, u.username, false)).join("")}
     </select></label>
     <p class="hint">The ${kind === "vm" ? "VM" : "network"} is created with the usual form and then handed to this user.</p>`,
    `<button class="btn" id="cf-cancel">Cancel</button><button class="btn primary" id="cf-next">Continue</button>`,
  );
  ctx.$("#cf-cancel", el).onclick = () => el.remove();
  ctx.$("#cf-next", el).onclick = () => {
    pendingOwner = { kind, owner: ctx.$("#cf-user", el).value };
    el.remove();
    onCreateFor?.(kind);
  };
}

// ------------------------------------------------------------------ quotas

const quotas_: Painter = async (ctx, area) => {
  ctx.setHtml(area, `
    <p class="hint">Limits apply to what a user owns. "Per daemon" applies the limits on each daemon separately; "Pooled" shares them across all daemons. A daemon override replaces the per-daemon limits there, or further caps the pool. Administrators are not limited.</p>
    <table class="grid">
      <tr><th>User</th><th>Mode</th><th>VMs</th><th>vCPUs</th><th>Memory</th><th>Storage</th><th>Networks</th><th>Overrides</th><th class="col-actions"></th></tr>
      ${users.map((u) => {
        const d = u.quota?.default ?? {};
        const n = Object.keys(u.quota?.daemons ?? {}).length;
        return `<tr>
          <td class="tname">${esc(u.username)}${u.admin ? ` <span class="badge on">admin</span>` : ""}</td>
          <td>${u.quota?.shared ? "Pooled" : "Per daemon"}</td>
          <td>${fmtLimit(d.vms)}</td><td>${fmtLimit(d.vcpus)}</td>
          <td>${d.memory_mb == null ? "∞" : fmtBytes(d.memory_mb * 1048576)}</td>
          <td>${fmtLimit(d.storage_gb, " GiB")}</td><td>${fmtLimit(d.networks)}</td>
          <td>${n || "—"}</td>
          <td class="col-actions"><button class="btn icon" data-quota="${u.id}" title="Edit limits">${ic.edit()}</button></td>
        </tr>`;
      }).join("")}
    </table>`);
  bind(area, "data-quota", (id) => {
    const u = users.find((x) => x.id === id);
    if (u) openQuota(ctx, u);
  });
};

const LIMIT_FIELDS: [keyof Limit, string][] = [
  ["vms", "VMs"], ["vcpus", "vCPUs"], ["memory_mb", "Memory (MiB)"],
  ["storage_gb", "Storage (GiB)"], ["networks", "Networks"],
];

function limitFields(prefix: string, l: Limit): string {
  return `<div class="limit-grid">${LIMIT_FIELDS.map(([k, label]) => `
    <label class="field"><span>${label}</span>
      <input type="number" min="0" inputmode="numeric" data-${prefix}="${k}" value="${l[k] ?? ""}" placeholder="∞"></label>`).join("")}</div>`;
}

function readLimit(root: Element, prefix: string): Limit {
  const out: Limit = {};
  root.querySelectorAll<HTMLInputElement>(`[data-${prefix}]`).forEach((i) => {
    const v = i.value.trim();
    out[i.getAttribute(`data-${prefix}`) as keyof Limit] = v === "" ? null : Math.max(0, Math.floor(Number(v)));
  });
  return out;
}

export function openQuota(ctx: AdminCtx, u: User) {
  const q: Quota = u.quota ?? { shared: false, default: {}, daemons: {} };
  const conns = ctx.connections();
  const el = ctx.openModal(
    `Limits — ${esc(u.username)}`,
    `<label class="field"><span>Mode</span><select id="q-mode">
       ${option("per", "Per daemon (each daemon separately)", !q.shared)}
       ${option("shared", "Pooled (shared across all daemons)", q.shared)}
     </select></label>
     <div class="section-title">Limits (blank = unlimited)</div>
     ${limitFields("ql", q.default)}
     <div class="section-title">Per-daemon overrides</div>
     ${conns.map((c) => {
       const has = !!q.daemons[c.id];
       return `<div class="quota-daemon">
         <label class="check"><input type="checkbox" data-qd-on="${c.id}" ${has ? "checked" : ""}> ${esc(c.name)}</label>
         <div data-qd-box="${c.id}" ${has ? "" : "hidden"}>${limitFields(`qd-${c.id}`, q.daemons[c.id] ?? {})}</div>
       </div>`;
     }).join("") || `<span class="dim-note">No daemons.</span>`}
     <p class="hint">Example: one VM with 2 vCPUs, 2048 MiB and 20 GiB — or 3 VMs pooled over 6 vCPUs, 8192 MiB and 60 GiB.</p>`,
    `<button class="btn" id="q-cancel">Cancel</button><button class="btn primary" id="q-save">Save</button>`,
  );
  el.querySelector(".modal")?.classList.add("wide");
  el.querySelectorAll<HTMLInputElement>("[data-qd-on]").forEach((c) => {
    c.onchange = () => {
      const box = el.querySelector(`[data-qd-box="${c.dataset.qdOn}"]`) as HTMLElement | null;
      if (box) box.hidden = !c.checked;
    };
  });
  ctx.$("#q-cancel", el).onclick = () => el.remove();
  ctx.$("#q-save", el).onclick = async () => {
    const daemons: Record<string, Limit> = {};
    for (const c of conns) {
      const on = el.querySelector<HTMLInputElement>(`[data-qd-on="${c.id}"]`);
      if (on?.checked) daemons[c.id] = readLimit(el, `qd-${c.id}`);
    }
    const quota: Quota = { shared: ctx.$("#q-mode", el).value === "shared", default: readLimit(el, "ql"), daemons };
    try {
      await api.put(`/users/${u.id}`, { quota });
      el.remove();
      ctx.toast("Limits saved");
      reload(ctx, "quotas")();
    } catch (e) { ctx.toast((e as Error).message, "bad"); }
  };
}

// --------------------------------------------------------------------- IPs

let ipOwner = "";

const ips_: Painter = async (ctx, area, gen) => {
  const rows = await api.get<IpRow[]>(`/ips${ipOwner ? `?user=${encodeURIComponent(ipOwner)}` : ""}`);
  if (!ctx.stillCurrent(gen)) return;
  ctx.setHtml(area, `
    <div class="actions admin-filters">
      <label class="field"><span>Owner</span><select id="ip-owner">
        ${option("", "Anyone", !ipOwner)}
        ${users.map((u) => option(u.id, u.username, ipOwner === u.id)).join("")}
      </select></label>
    </div>
    <p class="hint">Every address is held by one guest permanently (see IPAM). Changing it takes effect at the guest's next DHCP renewal; removing it lets the guest take a new one.</p>
    <table class="grid">
      <tr><th>Address</th><th>Guest</th><th>Owner</th><th>Network</th><th>Daemon</th><th>MAC</th><th>Kind</th><th class="col-actions"></th></tr>
      ${rows.map((r, i) => `
        <tr>
          <td class="tname mono">${esc(r.ip)}</td>
          <td>${esc(r.vm_name ?? r.name ?? "—")}</td>
          <td>${esc(r.owner_name ?? "—")}</td>
          <td>${esc(r.network)}</td>
          <td>${esc(r.conn_name)}</td>
          <td class="mono">${esc(r.mac)}</td>
          <td>${r.dynamic ? "Assigned" : "Reserved"}</td>
          <td class="col-actions"><div class="row-actions">
            <button class="btn icon" data-edit-ip="${i}" title="Change address">${ic.edit()}</button>
            <button class="btn danger icon" data-del-ip="${i}" title="Release">${ic.trash()}</button>
          </div></td>
        </tr>`).join("") || `<tr><td colspan="8" class="empty">No addresses allocated.</td></tr>`}
    </table>`);
  ctx.$("#ip-owner", area).onchange = (e: Event) => {
    ipOwner = (e.target as HTMLSelectElement).value;
    reload(ctx, "ips")();
  };
  const base = (r: IpRow) => `/proxy/${r.conn}/networks/${encodeURIComponent(r.network)}/hosts`;
  bind(area, "data-edit-ip", (i) => {
    const r = rows[Number(i)];
    const el = ctx.openModal(
      `Address — ${esc(r.vm_name ?? r.mac)}`,
      `<div class="grid2">
         <label class="field"><span>IPv4 address</span><input id="ip-addr" value="${esc(r.ip)}"></label>
         <label class="field"><span>Host name</span><input id="ip-name" value="${esc(r.name ?? "")}"></label>
       </div>`,
      `<button class="btn" id="ip-cancel">Cancel</button><button class="btn primary" id="ip-save">Save</button>`,
    );
    ctx.$("#ip-cancel", el).onclick = () => el.remove();
    ctx.$("#ip-save", el).onclick = async () => {
      try {
        await api.put(base(r), { mac: r.mac, ip: ctx.$("#ip-addr", el).value.trim(), name: ctx.$("#ip-name", el).value.trim() || null });
        el.remove();
        ctx.toast("Address changed");
        reload(ctx, "ips")();
      } catch (e) { ctx.toast((e as Error).message, "bad"); }
    };
  });
  bind(area, "data-del-ip", async (i) => {
    const r = rows[Number(i)];
    if (!await ctx.askConfirm("Release address", `Release ${r.ip} from ${r.vm_name ?? r.mac}? It gets a new one at its next DHCP.`, "Release", true)) return;
    try {
      await api.del(`${base(r)}/${encodeURIComponent(r.mac)}`);
      ctx.toast("Address released");
      reload(ctx, "ips")();
    } catch (e) { ctx.toast((e as Error).message, "bad"); }
  });
};

// ---------------------------------------------------------------- API keys

const keys_: Painter = async (ctx, area) => {
  const keys = await api.get<ApiKey[]>("/keys");
  ctx.setHtml(area, `
    <div class="actions"><button class="btn primary" id="add-key">${ic.plus(14)} New API key</button></div>
    <table class="grid">
      <tr><th>Name</th><th>Owner</th><th>Prefix</th><th>Limited to</th><th class="col-actions"></th></tr>
      ${keys.map((k) => `
        <tr>
          <td class="tname">${esc(k.name)}</td>
          <td>${esc(userName(k.user_id ?? ""))}</td>
          <td class="mono">${esc(k.prefix)}…</td>
          <td class="priv-cell">${k.privs?.length ? k.privs.map((p) => `<span class="priv">${esc(p)}</span>`).join("") : `<span class="dim-note">Owner's access</span>`}</td>
          <td class="col-actions"><button class="btn danger icon" data-del-key="${k.id}" title="Revoke">${ic.trash()}</button></td>
        </tr>`).join("") || `<tr><td colspan="5" class="empty">No keys.</td></tr>`}
    </table>`);
  ctx.$("#add-key", area).onclick = () => {
    const el = ctx.openModal(
      "New API key",
      `<div class="grid2">
         <label class="field"><span>Name</span><input id="k-name" placeholder="blank for a random name"></label>
         <label class="field"><span>Owner</span><select id="k-owner">
           ${users.map((u) => option(u.id, u.username, u.id === ctx.me()?.id)).join("")}
         </select></label>
       </div>
       <p class="hint">A key can only narrow its owner's access. Tick nothing to give it everything the owner has.</p>
       ${privGroups().map(([grp, ps]) => `
         <div class="section-title">${esc(grp)}</div>
         <div class="check-list">${ps.map((p) => `<label class="check"><input type="checkbox" data-kpriv="${esc(p)}"> ${esc(p.split(".")[1])}</label>`).join("")}</div>`).join("")}`,
      `<button class="btn" id="k-cancel">Cancel</button><button class="btn primary" id="k-save">Create</button>`,
    );
    ctx.$("#k-cancel", el).onclick = () => el.remove();
    ctx.$("#k-save", el).onclick = async () => {
      try {
        const k = await api.post<ApiKey>("/keys", {
          name: ctx.$("#k-name", el).value.trim(),
          user_id: ctx.$("#k-owner", el).value,
          privs: [...el.querySelectorAll<HTMLInputElement>("[data-kpriv]")].filter((c) => c.checked).map((c) => c.dataset.kpriv!),
        });
        el.remove();
        ctx.showSecret(`API key — ${k.name}`, k.key ?? "");
        reload(ctx, "keys")();
      } catch (e) { ctx.toast((e as Error).message, "bad"); }
    };
  };
  bind(area, "data-del-key", async (id) => {
    try {
      await api.del(`/keys/${id}`);
      ctx.toast("Key revoked");
      reload(ctx, "keys")();
    } catch (e) { ctx.toast((e as Error).message, "bad"); }
  });
};

// ---------------------------------------------------------------- activity

const activity_: Painter = async (ctx, area, gen) => {
  const res = await api.get<{ entries: ActivityEntry[]; all_users: boolean }>("/activity");
  if (!ctx.stillCurrent(gen)) return;
  ctx.setHtml(area, `
    <div class="actions"><button class="btn danger" id="act-clear">${ic.trash(14)} Clear all activity</button></div>
    <table class="grid">
      <tr><th>When</th><th>User</th><th>Action</th><th>Result</th><th>Detail</th></tr>
      ${res.entries.map((e) => `
        <tr>
          <td class="host">${esc(ctx.relTime(e.at_ms))}</td>
          <td>${esc(e.username)}</td>
          <td class="tname">${esc(e.label)}</td>
          <td><span class="state ${e.status === "done" ? "on" : ""}">${esc(e.status)}</span></td>
          <td class="task-msg">${esc(e.message || "—")}</td>
        </tr>`).join("") || `<tr><td colspan="5" class="empty">No activity recorded yet.</td></tr>`}
    </table>`);
  ctx.$("#act-clear", area).onclick = async () => {
    if (!await ctx.askConfirm("Clear activity log", "Delete every user's activity history?", "Clear", true)) return;
    try {
      await api.del("/activity");
      ctx.toast("Activity log cleared");
      reload(ctx, "activity")();
    } catch (e) { ctx.toast((e as Error).message, "bad"); }
  };
};

// --------------------------------------------------------------- dashboard

interface UsageRow {
  id: string;
  username: string;
  quota: Quota;
  vms: number;
  vcpus: number;
  memory_mb: number;
  storage_bytes: number;
  networks: number;
}

type DashTab = "overview" | "usage" | "ownership" | "activity";
let dashTab: DashTab = "overview";

const DASH_TABS: [DashTab, string, (s?: number) => string][] = [
  ["overview", "Overview", ic.datacenter],
  ["usage", "Usage", ic.quota],
  ["ownership", "Ownership", ic.user],
  ["activity", "Activity", ic.autostart],
];

/// One limit as a metric line: used / cap, a meter, and what is left. Each
/// resource is its own limit, so each gets its own meter; there is no single
/// "how full is this user" figure, because a user at 100% of their vCPUs and
/// 5% of their storage is not 100% of anything as a whole.
function limitMeter(label: string, used: number, cap: number | null | undefined, fmt: (n: number) => string): string {
  if (cap == null) {
    return `<div class="spec"><b>${esc(label)}</b><span>${fmt(used)} <span class="dim-note">· no limit</span></span></div>`;
  }
  const pct = cap === 0 ? (used > 0 ? 100 : 0) : Math.min(100, (used / cap) * 100);
  return `<div class="spec limit-spec">
      <b>${esc(label)}</b>
      <span>${fmt(used)} / ${fmt(cap)}</span>
      <div class="meter"><div class="meter-fill ${pct >= 100 ? "hot" : ""}" style="width:${pct.toFixed(1)}%"></div></div>
    </div>`;
}

/// The admin page's landing view, built from the datacenter dashboard's own
/// parts -- the card strip, metric panels with meters, spec grids and grid
/// tables -- so it reads as the same product, split into tabs like every
/// other multi-part page.
const dashboard_: Painter = async (ctx, area, gen) => {
  const [res, usage, acl, act, keys] = await Promise.all([
    api.get<AdminResource[]>("/resources"),
    api.get<UsageRow[]>("/usage"),
    api.get<AclEntry[]>("/acl"),
    api.get<{ entries: ActivityEntry[] }>("/activity"),
    api.get<ApiKey[]>("/keys"),
  ]);
  if (!ctx.stillCurrent(gen)) return;

  const conns = ctx.connections();
  const online = conns.filter((c) => c.online !== false).length;
  const admins = users.filter((u) => u.admin).length;
  const vms = res.filter((r) => r.kind === "vm");
  const nets = res.filter((r) => r.kind === "network");
  const vols = res.filter((r) => r.kind === "volume");
  const suspended = res.filter((r) => r.suspended);
  const dayAgo = Date.now() - 86_400_000;
  const today = act.entries.filter((e) => e.at_ms >= dayAgo);
  const failedToday = today.filter((e) => e.status === "failed").length;
  const limited = usage.filter((u) => {
    const d = u.quota?.default ?? {};
    return [d.vms, d.vcpus, d.memory_mb, d.storage_gb, d.networks].some((x) => x != null)
      || Object.keys(u.quota?.daemons ?? {}).length > 0;
  });
  const mem = (m: number) => fmtBytes(m * 1048576);
  const count = (n: number) => String(n);

  const card = (icon: string, n: number | string, label: string, tone: string, go: AdminSection) =>
    `<div class="card ${tone} admin-card-link" data-go="${go}" tabindex="0" role="link">
      <span class="card-ic">${icon}</span><div><b>${n}</b><i>${label}</i></div></div>`;

  // A metric panel in the datacenter dashboard's shape: head, big figure, a
  // meter, and a two-sided foot.
  const panel = (icon: string, title: string, value: string, pct: number, left: string, right: string, go: AdminSection) =>
    `<div class="metric ok admin-card-link" data-go="${go}" tabindex="0" role="link">
      <div class="metric-head"><span class="metric-ic">${icon}</span><b>${esc(title)}</b>
        <span class="metric-value">${esc(value)}</span></div>
      <div class="meter"><div class="meter-fill ${pct > 85 ? "hot" : ""}" style="width:${Math.min(100, pct).toFixed(1)}%"></div></div>
      <div class="metric-foot"><span>${left}</span><span>${right}</span></div>
    </div>`;

  let body = "";
  if (dashTab === "overview") {
    const running = vms.filter((v) => v.state === "running").length;
    const activeNets = nets.filter((n) => n.state === "active").length;
    const ownedByUsers = vms.filter((v) => v.owner && !users.find((u) => u.id === v.owner)?.admin).length;
    body = `
      <div class="cards">
        ${card(ic.user(20), users.length, "USERS", "", "users")}
        ${card(ic.admin(20), admins, "ADMINISTRATORS", "", "users")}
        ${card(ic.groups(20), groups.length, "GROUPS", "", "groups")}
        ${card(ic.perms(20), acl.length, "PERMISSIONS", "", "permissions")}
        ${card(ic.key(20), keys.length, "API KEYS", "", "keys")}
      </div>
      <div class="sumgrid">
        ${panel(ic.server(), "Daemons", `${online}/${conns.length}`, conns.length ? (online / conns.length) * 100 : 0,
          `${online} online`, `${conns.length - online} offline`, "daemons")}
        ${panel(ic.vms(), "VMs", String(vms.length), vms.length ? (running / vms.length) * 100 : 0,
          `${running} running`, `${ownedByUsers} owned by users`, "resources")}
        ${panel(ic.network(), "Networks", String(nets.length), nets.length ? (activeNets / nets.length) * 100 : 0,
          `${activeNets} active`, `${vols.length} volumes`, "resources")}
        ${panel(ic.autostart(), "Activity today", String(today.length),
          today.length ? (failedToday / today.length) * 100 : 0,
          `${today.length - failedToday} done`, `${failedToday} failed`, "activity")}
      </div>
      <div class="dash-section">
        <div class="section-title">Needs attention</div>
        <table class="grid">
          <tr><th>What</th><th>Where</th><th>Detail</th></tr>
          ${[
            ...conns.filter((c) => c.online === false).map((c) =>
              `<tr class="row-open" tabindex="0" data-go="daemons"><td class="tname">Daemon offline</td><td>${esc(c.name)}</td>
                <td class="task-msg">${esc(c.offline_error ?? "not answering")}</td></tr>`),
            ...suspended.map((r) =>
              `<tr class="row-open" tabindex="0" data-go="resources"><td class="tname">Suspended ${r.kind === "vm" ? "VM" : r.kind}</td>
                <td>${esc(r.name)} · ${esc(r.conn_name)}</td><td>owned by ${esc(r.owner_name ?? "—")}</td></tr>`),
            ...act.entries.filter((e) => e.status === "failed").slice(0, 5).map((e) =>
              `<tr class="row-open" tabindex="0" data-go="activity"><td class="tname">Failed: ${esc(e.label)}</td>
                <td>${esc(e.username)} · ${esc(ctx.relTime(e.at_ms))}</td><td class="task-msg">${esc(e.message || "—")}</td></tr>`),
          ].join("") || `<tr><td colspan="3" class="empty">Nothing needs attention.</td></tr>`}
        </table>
      </div>`;
  } else if (dashTab === "usage") {
    // One card per limited user, each limit its own meter.
    body = limited.length ? `<div class="card-grid">${limited.map((u) => {
      const d = u.quota?.default ?? {};
      const over = Object.keys(u.quota?.daemons ?? {}).length;
      return `<div class="storage-card" data-open tabindex="0" data-quota-user="${esc(u.id)}">
        <div class="storage-card-head">
          <span class="storage-card-icon">${ic.user()}</span>
          <span class="storage-card-title">${esc(u.username)}</span>
          <span class="badge">${u.quota?.shared ? "Pooled" : "Per daemon"}</span>
          ${over ? `<span class="badge">+${over} override${over > 1 ? "s" : ""}</span>` : ""}
        </div>
        <div class="specs">
          ${limitMeter("VMs", u.vms, d.vms, count)}
          ${limitMeter("vCPUs", u.vcpus, d.vcpus, count)}
          ${limitMeter("Memory", u.memory_mb, d.memory_mb, mem)}
          ${limitMeter("Storage", u.storage_bytes / 1073741824, d.storage_gb, (g) => `${Number.isInteger(g) ? g : g.toFixed(1)} GiB`)}
          ${limitMeter("Networks", u.networks, d.networks, count)}
        </div>
      </div>`;
    }).join("")}</div>
    ${usage.length > limited.length ? `<p class="hint">${usage.length - limited.length} user(s) have no limits and are not shown. Administrators are never limited.</p>` : ""}`
      : `<div class="empty">No user has limits. Set them under Limits › Quotas.</div>`;
  } else if (dashTab === "ownership") {
    const byOwner = new Map<string, { name: string; vms: number; nets: number; vols: number; vcpus: number; mem: number }>();
    for (const r of res) {
      if (r.kind === "pool") continue;
      const k = r.owner ?? "";
      const o = byOwner.get(k) ?? { name: r.owner_name ?? "—", vms: 0, nets: 0, vols: 0, vcpus: 0, mem: 0 };
      if (r.kind === "vm") { o.vms++; o.vcpus += r.vcpus ?? 0; o.mem += r.memory_mb ?? 0; }
      else if (r.kind === "network") o.nets++;
      else o.vols++;
      byOwner.set(k, o);
    }
    body = `<table class="grid">
      <tr><th>Owner</th><th>VMs</th><th>vCPUs</th><th>Memory</th><th>Networks</th><th>Volumes</th></tr>
      ${[...byOwner.entries()].sort((a, b) => b[1].vms - a[1].vms).map(([id, o]) => `
        <tr class="row-open" tabindex="0" data-owner="${esc(id)}">
          <td class="tname">${esc(o.name)}</td><td>${o.vms}</td><td>${o.vcpus}</td><td>${mem(o.mem)}</td>
          <td>${o.nets}</td><td>${o.vols}</td>
        </tr>`).join("") || `<tr><td colspan="6" class="empty">Nothing on any daemon yet.</td></tr>`}
    </table>`;
  } else {
    body = `<table class="grid">
      <tr><th>When</th><th>User</th><th>Action</th><th>Result</th><th>Detail</th></tr>
      ${act.entries.slice(0, 50).map((e) => `
        <tr>
          <td class="host">${esc(ctx.relTime(e.at_ms))}</td>
          <td>${esc(e.username)}</td>
          <td class="tname">${esc(e.label)}</td>
          <td><span class="state ${e.status === "done" ? "on" : "bad"}">${esc(e.status === "done" ? "Done" : "Failed")}</span></td>
          <td class="task-msg">${esc(e.message || "—")}</td>
        </tr>`).join("") || `<tr><td colspan="5" class="empty">No activity recorded yet.</td></tr>`}
    </table>
    ${act.entries.length > 50 ? `<div class="actions"><button class="btn" data-go="activity">${ic.logs(14)} Full activity log</button></div>` : ""}`;
  }

  ctx.setHtml(area, `
    <div class="tabbar">
      ${DASH_TABS.map(([t, label, icon]) =>
        `<button class="tab ${t === dashTab ? "active" : ""}" data-dash-tab="${t}">${icon(14)} ${label}</button>`).join("")}
    </div>
    ${body}`);

  area.querySelectorAll<HTMLElement>("[data-dash-tab]").forEach((b) => {
    b.onclick = () => {
      dashTab = b.dataset.dashTab as DashTab;
      ctx.go("dashboard");
    };
  });
  area.querySelectorAll<HTMLElement>("[data-go]").forEach((el) => {
    el.onclick = () => ctx.go(el.dataset.go as AdminSection);
  });
  area.querySelectorAll<HTMLElement>("[data-owner]").forEach((el) => {
    el.onclick = () => {
      resFilter = { kind: "", owner: el.dataset.owner ?? "", conn: "" };
      ctx.go("resources");
    };
  });
  area.querySelectorAll<HTMLElement>("[data-quota-user]").forEach((el) => {
    el.onclick = () => {
      const u = users.find((x) => x.id === el.dataset.quotaUser);
      if (u) openQuota(ctx, u);
    };
  });
};

const SECTIONS: Record<AdminSection, Painter> = {
  dashboard: dashboard_,
  users: users_,
  groups: groups_,
  roles: roles_,
  permissions: permissions_,
  daemons: daemons_,
  resources: resources_,
  quotas: quotas_,
  ips: ips_,
  keys: keys_,
  activity: activity_,
};
