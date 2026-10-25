// The profile card: identity, password, API keys and the user's own limits.
// It calls back into main.ts for three things (the sidebar profile button, the
// role label and the show-once secret dialog); those come in through
// `profileHooks`, set once at start-up, so this module never imports main.ts.
import { api, effectiveLimit } from "./api";
import type { User } from "./api";
import { limitSpecs } from "./admin";
import { openCropDialog } from "./crop";
import { $, setHtml } from "./dom";
import { askText, openModal, toast } from "./dialogs";
import { ic } from "./icons";
import { state } from "./state";
import { esc } from "./xml";

export const profileHooks = {
  renderProfileBtn: (): void => {},
  roleLabel: (_admin: boolean | undefined): string => "",
  showSecret: (_title: string, _secret: string): void => {},
};

/// Avatar markup shared by the sidebar button and the profile card.
export function avatarHtml(): string {
  return state.user?.avatar
    ? `<img src="/avatars/${encodeURIComponent(state.user.avatar)}" alt="">`
    : (state.user?.nickname[0] ?? "u").toUpperCase();
}

/// Repaint every place the identity is shown, after any profile change.
export function renderIdentity() {
  profileHooks.renderProfileBtn();
  const host = $(".topbar .host");
  if (host && state.user) {
  host.textContent = `web${state.authDisabled ? " · local" : ` · ${state.user.nickname}`}`;
  }
}

/// One profile card: avatar (click to crop), display name, username,
/// password and API keys. Replaces the old popover + separate modal split.
export function openProfile() {
  if (!state.user) return;
  const el = openModal(
    "Profile",
    `<div class="profile-id">
       <button class="pop-avatar" id="pf-avatar-btn" title="Change Picture">
         <div class="avatar">${avatarHtml()}</div>
         <span class="camera-badge">${ic.camera()}</span>
       </button>
       <div class="who">
         <b id="pf-who-name">${esc(state.user.nickname)}</b>
         <span id="pf-who-user">@${esc(state.user.username)} · ${profileHooks.roleLabel(state.user.admin)}</span>
       </div>
     </div>

      <div class="grid2">
       <label class="field"><span>Display Name</span>
          <input id="pf-nick" value="${esc(state.user.nickname)}"></label>
       <label class="field"><span>Username (used to sign in)</span>
         <input id="pf-user" value="${esc(state.user.username)}" autocomplete="username"></label>
     </div>

     <div class="section-title">Password</div>
     <div class="grid2">
       <label class="field"><span>Current Password</span>
         <input id="pf-pass" type="password" autocomplete="current-password"></label>
       <label class="field"><span>New Password</span>
         <input id="pf-pass2" type="password" autocomplete="new-password" placeholder="min 8 characters"></label>
     </div>
     <div class="actions"><button class="btn" id="pf-pass-save">${ic.lock()} Change Password</button></div>

     <div class="section-title">API Keys</div>
     <div class="key-list" id="pf-keys"></div>
     <div class="actions"><button class="btn" id="pf-newkey">${ic.key()} New API Key</button></div>

     <div id="pf-limits"></div>`,
    // Save sits next to Cancel and only appears once something differs, so the
    // footer never offers a no-op. Cancel/Close is the same button relabelled:
    // with pending edits, closing discards them.
    `<button class="btn" id="pf-close">Close</button>
     <button class="btn primary hidden" id="pf-save">Save Changes</button>`,
  );
  // Wider than `.wide` (900px): the limits are six cards pinned to one row, and
  // at 900 they were squeezed against the viewport cap. Its own class, since
  // `.wide` is shared with the access log and firewall tables, which do not
  // want to grow.
  el.querySelector(".modal")?.classList.add("wide", "pf-modal");
  $("#pf-close", el).onclick = () => el.remove();
  $("#pf-avatar-btn", el).onclick = () => void changeAvatar(el);
  void loadProfileLimits(el);

  // Show Save only when the form actually differs from the stored identity.
  const nickInput = $("#pf-nick", el) as HTMLInputElement;
  const userInput = $("#pf-user", el) as HTMLInputElement;
  const saveBtn = $("#pf-save", el) as HTMLButtonElement;
  const closeBtn = $("#pf-close", el) as HTMLButtonElement;
  const syncDirty = () => {
    const dirty = nickInput.value.trim() !== (state.user?.nickname ?? "")
      || userInput.value.trim() !== (state.user?.username ?? "");
    saveBtn.classList.toggle("hidden", !dirty);
    closeBtn.textContent = dirty ? "Cancel" : "Close";
  };
  nickInput.oninput = syncDirty;
  userInput.oninput = syncDirty;

  // Display name and username save together: both live on PUT /me, so one
  // button means one round trip and no half-applied state.
  saveBtn.onclick = async () => {
    const nickname = nickInput.value.trim();
    const username = userInput.value.trim();
    if (!nickname) return toast("Display name is required", "bad");
    if (!username) return toast("Username is required", "bad");
    const before = state.user?.username;
    try {
      state.user = await api.put<User>("/me", { nickname, username });
      toast(username === before ? "Saved" : "Saved — sign in with the new username next time");
      renderIdentity();
      refreshProfileCard(el);
      // Saved values are the new baseline, so the button retires itself.
      syncDirty();
    } catch (e) {
      toast((e as Error).message, "bad");
    }
  };

  $("#pf-pass-save", el).onclick = async () => {
    const current = $("#pf-pass", el).value;
    const next = $("#pf-pass2", el).value;
    if (!current || !next) return toast("Enter current and new password", "bad");
    try {
      await api.post("/me/password", { current, new: next });
      toast("Password changed");
      ($("#pf-pass", el) as HTMLInputElement).value = "";
      ($("#pf-pass2", el) as HTMLInputElement).value = "";
    } catch (e) {
      toast((e as Error).message, "bad");
    }
  };

  const refreshKeys = async () => {
    const keys = await api.get<import("./api").ApiKey[]>("/me/keys");
    const box = $("#pf-keys", el);
    setHtml(box, keys.map((k) => `<div class="task-row"><span class="task-dot done"></span><span class="task-msg">${esc(k.name)} · ${esc(k.prefix)}…</span><button class="btn danger icon" data-kid="${k.id}">${ic.trash()}</button></div>`).join("") || `<div class="empty">No API keys.</div>`);
    box.querySelectorAll("[data-kid]").forEach((b: Element) => {
      (b as HTMLElement).onclick = async () => {
        await api.del(`/me/keys/${(b as HTMLElement).dataset.kid}`);
        refreshKeys();
      };
    });
  };
  refreshKeys();
  $("#pf-newkey", el).onclick = async () => {
    const name = await askText(
      "New API Key", "Key name (blank to generate one)", "", "leave blank for a random name");
    // askText returns null when cancelled and "" when accepted while empty, so
    // only null aborts; empty means "let the server name it".
    if (name === null) return;
    try {
      const k = await api.post<import("./api").ApiKey>("/me/keys", { name: name.trim() });
      refreshKeys();
      // Shown once and never again, so it gets a dismissable dialog rather
      // than a toast on a timer.
      profileHooks.showSecret(`API key — ${k.name}`, k.key ?? "");
    } catch (e) {
      toast((e as Error).message, "bad");
    }
  };
}

/// Repaint the card's identity header from the current user.
/// The profile's Resource Limits section: what the user may use, and what
/// they do. Each limit is its own meter. Per-daemon limits are shown for each
/// daemon; pooled limits once, against the total across daemons.
async function loadProfileLimits(el: Element) {
  const box = $("#pf-limits", el);
  if (!box) return;
  type Used = { vms: number; vcpus: number; memory_mb: number; storage_bytes: number; networks: number; forwards?: number };
  let u: {
    admin: boolean;
    quota: import("./api").Quota;
    total: Used;
    daemons: (Used & { conn: string; name: string; online: boolean })[];
  };
  try {
    u = await api.get<typeof u>("/me/usage");
  } catch {
    return; // the rest of the profile is still usable
  }
  if (!box.isConnected) return;
  // An administrator has no limits at all, so the section is removed rather
  // than explained. A heading plus "Administrators have no resource limits" is
  // still a Resource Limits section: it takes the space, and it is the first
  // thing read on the profile. Nothing here can be set for them, and the server
  // no longer enforces anything either (`guard::quota_applies`), so there is
  // nothing to say.
  if (u.admin) {
    setHtml(box, "");
    return;
  }
  const title = `<div class="section-title">Resource Limits</div>`;
  const q = u.quota ?? { shared: false, default: {}, daemons: {} };
  const any = (l: import("./api").Limit) => [l.vms, l.vcpus, l.memory_mb, l.storage_gb, l.networks, l.forwards].some((x) => x != null);
  if (!any(q.default) && !Object.values(q.daemons ?? {}).some(any)) {
    setHtml(box, "");
    return;
  }
  const block = (name: string, used: Used, cap: import("./api").Limit, note = "") => `
    <div class="pf-limit">
      <div class="pf-limit-head"><b>${esc(name)}</b>${note ? `<span class="dim-note">${esc(note)}</span>` : ""}</div>
      ${limitSpecs(used, cap)}
    </div>`;
  let html = "";
  // Pooled: the limits are one figure for the whole pool, so per-server rows
  // would be the same limits restated once per server. A server is listed only
  // where it has an override of its own, which is a limit the pool does not
  // state. Per-daemon mode has no pooled figure, so every server is listed.
  const pooled = q.shared;
  if (pooled) html += block("All servers", u.total, q.default, "shared across servers");
  for (const d of u.daemons) {
    const own = q.daemons?.[d.conn];
    if (pooled && !own) continue;
    if (!d.online) {
      html += `<div class="pf-limit"><div class="pf-limit-head"><b>${esc(d.name)}</b><span class="dim-note">offline</span></div></div>`;
      continue;
    }
    // An override is that daemon's own ceiling and replaces the global one, so
    // its row shows the override alone -- not the tighter of it and the pool.
    html += block(d.name, d, effectiveLimit(q, d.conn), own ? "server-specific limit" : "");
  }
  setHtml(box, `${title}${html}`);
}

function refreshProfileCard(el: Element) {
  if (!state.user) return;
  const name = $("#pf-who-name", el);
  if (name) name.textContent = state.user.nickname;
  const who = $("#pf-who-user", el);
  if (who) who.textContent = `@${state.user.username} · ${profileHooks.roleLabel(state.user.admin)}`;
  const av = $("#pf-avatar-btn .avatar", el);
  if (av) setHtml(av, avatarHtml());
}

/// Pick a file, crop it square, and upload it as the avatar.
async function changeAvatar(card?: Element) {
  if (!state.user) return;
  const input = document.createElement("input");
  input.type = "file";
  input.accept = "image/png,image/jpeg,image/webp";
  input.onchange = async () => {
    const file = input.files?.[0];
    if (!file) return;
    const blob = await openCropDialog(file);
    if (!blob) return;
    try {
      // JPEG: the crop is exported at the photo's own resolution, and a
      // lossless PNG of a 1024px photo is ~3 MB, past the server's 2 MB cap
      // (`web/server/src/api/profile.rs`).
      state.user = await api.postRaw<User>("/me/avatar?ext=jpg", blob, "image/jpeg");
      toast("Avatar updated");
      renderIdentity();
      if (card) refreshProfileCard(card);
    } catch (e) {
      toast((e as Error).message, "bad");
    }
  };
  input.click();
}
