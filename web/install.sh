#!/usr/bin/env bash
# Install virt-man-web: build frontend + backend, install binary + static
# assets + data dir, install and enable the systemd service.
#
# Test/stage without touching the system:
#   DESTDIR=/tmp/stage SKIP_SYSTEMD=1 ./install.sh
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"          # web/
SERVER="$HERE/server"                          # web/server/

PREFIX="${VIRT_MAN_PREFIX:-/usr/local}"
SHARE="${VIRT_MAN_SHARE:-$PREFIX/share/virt-man}"
DATA_DIR="${VIRT_MAN_WEB_DATA:-/var/lib/virt-man}"
DESTDIR="${DESTDIR:-}"
SKIP_SYSTEMD="${SKIP_SYSTEMD:-}"
BIN_NAME="virt-man-web"

say() { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

# Build steps run as the invoking user (not root), so repo artifacts stay
# user-owned and rustup resolves the user's toolchain.
run_build() {
  if [[ -n "${SUDO_USER:-}" ]]; then
    # sudo drops the environment; keep what the root installer set for the
    # build (RUSTFLAGS on musl, PATH to rustup's cargo).
    sudo -u "$SUDO_USER" -- env PATH="$PATH" ${RUSTFLAGS:+RUSTFLAGS="$RUSTFLAGS"} "$@"
  else
    "$@"
  fi
}

if [[ -z "$DESTDIR" && $EUID -ne 0 ]]; then
  die "run as root (sudo $0), or set DESTDIR=/tmp/stage SKIP_SYSTEMD=1 for a staged test"
fi

# --- dependencies ----------------------------------------------------------
for tool in cargo rustc node pnpm; do
  command -v "$tool" >/dev/null 2>&1 || die "missing required tool: $tool"
done

# --- build frontend --------------------------------------------------------
say "building frontend (web/dist)"
( cd "$HERE" && run_build pnpm install && run_build pnpm build )

# --- build backend ---------------------------------------------------------
say "building backend"
( cd "$SERVER" && run_build cargo build --release )

# --- install ---------------------------------------------------------------
say "installing $BIN_NAME to $DESTDIR$PREFIX/bin"
install -Dm755 "$SERVER/target/release/$BIN_NAME" "$DESTDIR$PREFIX/bin/$BIN_NAME"

say "installing static assets to $DESTDIR$SHARE/dist"
rm -rf "$DESTDIR$SHARE/dist"
install -d -m755 "$DESTDIR$SHARE"
cp -a "$HERE/dist" "$DESTDIR$SHARE/dist"

say "creating data dir $DESTDIR$DATA_DIR"
install -d -m700 "$DESTDIR$DATA_DIR"

# --- systemd service -------------------------------------------------------
if [[ -n "$SKIP_SYSTEMD" ]]; then
  say "SKIP_SYSTEMD set — not touching systemd"
else
  say "installing systemd unit"
  install -Dm644 "$SERVER/virt-man-web.service" "$DESTDIR/etc/systemd/system/virt-man-web.service"
  if [[ -n "$DESTDIR" ]]; then
    say "staged install — not enabling/starting service"
  else
    systemctl daemon-reload
    systemctl enable virt-man-web.service
    systemctl restart virt-man-web.service
    systemctl --no-pager --lines=5 status virt-man-web.service || true
  fi
fi

say "done"
printf 'Web UI: https://<this-host>:8006  (first visit: create the admin user)\n'
