#!/usr/bin/env bash
# Install virt-man on Debian/Ubuntu (apt) or Alpine (apk).
#
#   sudo ./install.sh daemon      # on each hypervisor host
#   sudo ./install.sh web         # on the machine that serves the UI
#   sudo ./install.sh all         # both, on one machine
#
# Installs the system packages each part needs, a new enough Rust toolchain
# (the daemon uses let-chains, Rust >= 1.88; Debian 13 ships 1.85 and Alpine
# 3.22 ships 1.87, so rustup is used when the distro's is older), and pnpm,
# then hands over to daemon/install.sh or web/install.sh, which build and
# install the service. On Alpine the services are OpenRC, not systemd.
#
# Package names were checked against Debian 13 (trixie) and Alpine 3.22.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
RUST_MIN="1.88"
say() { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

what="${1:-}"
case "$what" in
  daemon | web | all) ;;
  *) die "usage: sudo $0 daemon|web|all" ;;
esac
[[ $EUID -eq 0 ]] || die "run as root: sudo $0 $what"

# The user builds run as (repo artifacts stay theirs, rustup is theirs).
BUILD_USER="${SUDO_USER:-root}"
# SUDO_USER can name someone with no account here (entering via
# nsenter inherits the host's): build as root then, rather than fail.
if ! getent passwd "$BUILD_USER" >/dev/null 2>&1; then
  BUILD_USER=root
  unset SUDO_USER # the part installers build as SUDO_USER too
fi
BUILD_HOME="$(getent passwd "$BUILD_USER" | cut -d: -f6)"
[[ -n "$BUILD_HOME" ]] || BUILD_HOME=/root
as_user() { if [[ "$BUILD_USER" == root ]]; then "$@"; else sudo -u "$BUILD_USER" -H -- "$@"; fi; }

if command -v apt-get >/dev/null 2>&1; then
  PM=apt
elif command -v apk >/dev/null 2>&1; then
  PM=apk
else
  die "no apt-get or apk here: install the packages listed in README.md by hand, then run daemon/install.sh or web/install.sh"
fi
say "package manager: $PM"

# ------------------------------------------------------------------ packages
# Build: a C toolchain and pkg-config for the Rust crates. Runtime, daemon:
# QEMU with the x86_64 system emulator and qemu-img, OVMF firmware, nftables
# (firewall), iproute2 (bridge addressing and taps), dnsmasq-base (the DHCP and
# DNS server; the -base package has the binary without the distro's service,
# which would bind :53 on every interface), swtpm (TPM guests need it),
# util-linux, and openssl for the TLS certificate.
apt_daemon=(qemu-system-x86 qemu-utils ovmf nftables iproute2 dnsmasq-base swtpm util-linux openssl)
apt_web=(nodejs npm)
apt_build=(build-essential pkg-config curl ca-certificates)
# Alpine has no swtpm-tools package; `swtpm` carries the emulator binary.
apk_daemon=(qemu-system-x86_64 qemu-img ovmf nftables iproute2 dnsmasq swtpm util-linux-misc openssl)
apk_web=(nodejs npm)
apk_build=(build-base pkgconf curl ca-certificates bash sudo)

pkgs=()
case "$PM" in
  apt)
    pkgs+=("${apt_build[@]}")
    [[ $what != web ]] && pkgs+=("${apt_daemon[@]}")
    [[ $what != daemon ]] && pkgs+=("${apt_web[@]}")
    say "installing: ${pkgs[*]}"
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -q
    apt-get install -y -q --no-install-recommends "${pkgs[@]}"
    ;;
  apk)
    # nodejs and ovmf are in community.
    if ! grep -qE '^[^#].*/community' /etc/apk/repositories; then
      main="$(grep -m1 -E '^[^#].*/main' /etc/apk/repositories || true)"
      [[ -n "$main" ]] && echo "${main%/main}/community" >> /etc/apk/repositories
    fi
    pkgs+=("${apk_build[@]}")
    [[ $what != web ]] && pkgs+=("${apk_daemon[@]}")
    [[ $what != daemon ]] && pkgs+=("${apk_web[@]}")
    say "installing: ${pkgs[*]}"
    apk update -q
    apk add -q "${pkgs[@]}"
    ;;
esac

# ------------------------------------------------------------------ rust
rust_ok() {
  local v
  v="$(as_user bash -lc 'rustc --version 2>/dev/null' | awk '{print $2}')" || return 1
  [[ -n "$v" ]] && [[ "$(printf '%s\n%s\n' "$RUST_MIN" "$v" | sort -V | head -1)" == "$RUST_MIN" ]]
}
if ! rust_ok; then
  say "installing Rust >= $RUST_MIN with rustup for $BUILD_USER"
  as_user bash -c 'curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable'
fi
# The child installers find cargo on PATH, as the build user and as root.
export PATH="$BUILD_HOME/.cargo/bin:$PATH"
# rustup's musl toolchain links statically by default (`-static-pie`), which
# does not link against the system's shared libraries on Alpine. Link against
# them instead, as Alpine's own rust package does.
if [[ $PM == apk ]]; then
  export RUSTFLAGS="${RUSTFLAGS:-} -C target-feature=-crt-static"
fi
rust_ok || die "Rust >= $RUST_MIN is still not available; install it and re-run"

# ------------------------------------------------------------------ pnpm
if [[ $what != daemon ]] && ! command -v pnpm >/dev/null 2>&1; then
  say "installing pnpm"
  npm install -g pnpm >/dev/null
fi

# ------------------------------------------------------------------ install
run_part() {
  local part="$1"
  if [[ $PM == apk ]]; then
    # OpenRC: the part installs its binary and config; the service is ours.
    SKIP_SYSTEMD=1 "$HERE/$part/install.sh"
    install -Dm755 "$HERE/$part/openrc/virt-man-$part" "/etc/init.d/virt-man-$part"
    rc-update add "virt-man-$part" default
    rc-service "virt-man-$part" restart
  else
    "$HERE/$part/install.sh"
  fi
}
[[ $what != web ]] && run_part daemon
[[ $what != daemon ]] && run_part web

say "done"
