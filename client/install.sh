#!/usr/bin/env bash
# Install the virt-man desktop app: build the web UI + backend + Tauri shell,
# install into /opt/virt-man, and (optionally) create a .desktop launcher.
#
# Test/stage without touching the system:
#   DESTDIR=/tmp/stage ./install.sh
#
# The app spawns the bundled virt-man-web as a local sidecar (loopback HTTP)
# with its own data dir, so it runs standalone with no webui/daemon services.
set -euo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"   # repo root
CLIENT="$HERE/client"

PREFIX="/opt/virt-man"
DESTDIR="${DESTDIR:-}"
SKIP_DESKTOP="${SKIP_DESKTOP:-}"

say() { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

# Build steps run as the invoking user (not root), so repo artifacts stay
# user-owned and rustup resolves the user's toolchain.
run_build() {
  if [[ -n "${SUDO_USER:-}" ]]; then
    sudo -u "$SUDO_USER" -- "$@"
  else
    "$@"
  fi
}

if [[ -z "$DESTDIR" && $EUID -ne 0 ]]; then
  die "run as root (sudo $0), or set DESTDIR=/tmp/stage for a staged test"
fi

for tool in cargo rustc node pnpm; do
  command -v "$tool" >/dev/null 2>&1 || die "missing required tool: $tool"
done

# --- build everything ------------------------------------------------------
say "building web UI (web/dist)"
( cd "$HERE/web" && run_build pnpm install && run_build pnpm build )

say "building web backend"
( cd "$HERE/web/server" && run_build cargo build --release )

say "building desktop shell"
( cd "$CLIENT/src-tauri" && run_build cargo build --release )

# --- install ---------------------------------------------------------------
say "installing to $DESTDIR$PREFIX"
install -d -m755 "$DESTDIR$PREFIX"

# Tauri binary is named after the crate ("client"); install it as virt-man.
install -Dm755 "$CLIENT/src-tauri/target/release/client" "$DESTDIR$PREFIX/virt-man"
install -Dm755 "$HERE/web/server/target/release/virt-man-web" "$DESTDIR$PREFIX/virt-man-web"

say "installing static assets to $DESTDIR$PREFIX/dist"
rm -rf "$DESTDIR$PREFIX/dist"
cp -a "$HERE/web/dist" "$DESTDIR$PREFIX/dist"

say "installing icon"
install -Dm644 "$CLIENT/src-tauri/icons/icon.png" "$DESTDIR$PREFIX/icon.png"

# --- desktop launcher ------------------------------------------------------
if [[ -n "$SKIP_DESKTOP" ]]; then
  say "SKIP_DESKTOP set — not installing .desktop launcher"
else
  say "installing .desktop launcher"
  install -Dm644 "$CLIENT/virt-man.desktop" "$DESTDIR/usr/share/applications/virt-man.desktop"
fi

say "done"
printf 'Run: %s/virt-man\n' "$PREFIX"
