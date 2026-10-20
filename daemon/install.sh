#!/usr/bin/env bash
# Install virt-man-daemon: build, install binary, generate token + TLS cert,
# write config, install and enable the systemd service.
#
# Test/stage without touching the system:
#   DESTDIR=/tmp/stage SKIP_SYSTEMD=1 ./install.sh
set -euo pipefail

PREFIX="${VIRT_MAN_PREFIX:-/usr/local}"
DESTDIR="${DESTDIR:-}"
SKIP_SYSTEMD="${SKIP_SYSTEMD:-}"

ETC="/etc/virt-man-daemon"
TLS_DIR="$ETC/tls"
TOKEN_PATH="$ETC/token"
CONFIG_PATH="$ETC/daemon.toml"
BIN_NAME="virt-man-daemon"
HTTP_ADDR="${VIRT_MAN_HTTP_ADDR:-0.0.0.0:8443}"

# Extra SAN hostnames/IPs for the certificate. Defaults to localhost + this
# host's names and addresses.
HOSTNAME_OVERRIDE="${VIRT_MAN_HOSTNAME:-}"
IPS_OVERRIDE="${VIRT_MAN_IPS:-}"

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
for tool in cargo rustc openssl; do
  command -v "$tool" >/dev/null 2>&1 || die "missing required tool: $tool"
done

# --- build -----------------------------------------------------------------
cd "$(dirname "$0")"
say "building release binary"
run_build cargo build --release

# --- install binary --------------------------------------------------------
say "installing $BIN_NAME to $DESTDIR$PREFIX/bin"
install -Dm755 "target/release/$BIN_NAME" "$DESTDIR$PREFIX/bin/$BIN_NAME"

# Legacy files are migrated by Config::load, which handles symlinks, structured
# config rewrites and interrupted copies consistently. Do not duplicate it here.

# --- token -----------------------------------------------------------------
install -d -m 700 "$DESTDIR$ETC"
if [[ -n "${VIRT_MAN_TOKEN:-}" ]]; then
  say "using supplied VIRT_MAN_TOKEN"
  (umask 077; printf '%s\n' "$VIRT_MAN_TOKEN" > "$DESTDIR$TOKEN_PATH")
elif [[ -s "$DESTDIR$TOKEN_PATH" ]]; then
  say "keeping existing token at $DESTDIR$TOKEN_PATH"
else
  say "generating token at $DESTDIR$TOKEN_PATH"
  umask 077
  head -c 32 /dev/urandom | base64 > "$DESTDIR$TOKEN_PATH"
fi
chmod 600 "$DESTDIR$TOKEN_PATH"

# --- TLS certificate -------------------------------------------------------
hostname_="$(hostname 2>/dev/null || cat /etc/hostname 2>/dev/null | tr -d '\n' || echo localhost)"
if [[ -n "$HOSTNAME_OVERRIDE" ]]; then
  hostname_="$HOSTNAME_OVERRIDE"
fi
ips="${IPS_OVERRIDE:-$(hostname -I 2>/dev/null || ip -4 -o addr show scope global 2>/dev/null | awk '{print $4}' | cut -d/ -f1 | tr '\n' ' ')}"

# ::1 alongside 127.0.0.1: the listener is dual-stack, so a client may dial the
# IPv6 loopback literal, and a missing SAN makes that a cert-name failure.
san="DNS:localhost,IP:127.0.0.1,IP:::1,DNS:$hostname_"
for ip in $ips; do
  [[ "$ip" =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$ ]] && san="$san,IP:$ip"
done

install -d -m 700 "$DESTDIR$TLS_DIR"
if [[ -s "$DESTDIR$TLS_DIR/cert.pem" && -s "$DESTDIR$TLS_DIR/key.pem" ]]; then
  say "keeping existing TLS cert/key in $DESTDIR$TLS_DIR"
else
  say "generating self-signed TLS cert (SANs: $san)"
  # A proper end-entity leaf (CA:FALSE) so rustls clients can pin it as a
  # trust anchor without tripping its "CA used as end entity" check.
  (umask 077; openssl req -x509 -newkey rsa:2048 -nodes -sha256 -days 3650 \
    -keyout "$DESTDIR$TLS_DIR/key.pem" -out "$DESTDIR$TLS_DIR/cert.pem" \
    -subj "/CN=$hostname_" \
    -addext "subjectAltName=$san" \
    -addext "basicConstraints=critical,CA:FALSE" \
    -addext "keyUsage=critical,digitalSignature,keyEncipherment" \
    -addext "extendedKeyUsage=serverAuth")
  chmod 600 "$DESTDIR$TLS_DIR/key.pem"
  chmod 644 "$DESTDIR$TLS_DIR/cert.pem"
fi

# --- config ----------------------------------------------------------------
say "writing $DESTDIR$CONFIG_PATH"
if [[ ! -e "$DESTDIR$CONFIG_PATH" ]]; then
cat > "$DESTDIR$CONFIG_PATH" <<EOF
# virt-man-daemon config (managed by install.sh; edit freely and restart).
http_addr = "$HTTP_ADDR"
token_path = "$TOKEN_PATH"
cert_dir = "$TLS_DIR"

[tls]
cert = "$TLS_DIR/cert.pem"
key = "$TLS_DIR/key.pem"
EOF
else
  say "keeping existing config at $DESTDIR$CONFIG_PATH"
fi

# --- systemd service -------------------------------------------------------
if [[ -n "$SKIP_SYSTEMD" ]]; then
  say "SKIP_SYSTEMD set — not touching systemd"
else
  say "installing systemd unit"
  install -Dm644 virt-man-daemon.service "$DESTDIR/etc/systemd/system/virt-man-daemon.service"
  if [[ -n "$DESTDIR" ]]; then
    say "staged install — not enabling/starting service"
  else
    systemctl daemon-reload
    systemctl enable virt-man-daemon.service
    systemctl restart virt-man-daemon.service
    systemctl --no-pager --lines=5 status virt-man-daemon.service || true
  fi
fi

say "done"
printf 'Token: %s\n' "$TOKEN_PATH"
printf 'Connect: https://<this-host>:8443  (cert: %s/cert.pem — pin it on the web UI)\n' "$TLS_DIR"
