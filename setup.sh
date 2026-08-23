#!/usr/bin/env bash
#
# wolfvoice installer.
#
#   curl -fsSL https://raw.githubusercontent.com/wolfsoftwaresystemsltd/wolfvoice/main/setup.sh | sudo bash
#
# or, to see what it will do first (recommended):
#
#   curl -fsSLO https://raw.githubusercontent.com/wolfsoftwaresystemsltd/wolfvoice/main/setup.sh
#   less setup.sh && sudo bash setup.sh
#
# What it does:
#   1. works out your platform and downloads the matching release binary
#   2. verifies its published SHA-256
#   3. creates an unprivileged `wolfvoice` system user
#   4. obtains a TLS certificate with certbot, and installs renewal hooks that
#      open port 80 only for the seconds a renewal needs it
#   5. installs and starts a hardened systemd unit
#   6. prints the firewall rules and the OpenSim region config you still need
#
# It does NOT touch your firewall — that is yours to review. It prints exactly
# what to add.
#
# Copyright 2026 Wolf Software Systems Ltd. Licensed under the Apache License 2.0.

set -euo pipefail

REPO="wolfsoftwaresystemsltd/wolfvoice"
PREFIX="${PREFIX:-/usr/local/bin}"
CONFDIR="/etc/wolfvoice"
TLSDIR="$CONFDIR/tls"
SERVICE_USER="wolfvoice"
RPC_PORT="${RPC_PORT:-9443}"
MEDIA_LO="${MEDIA_LO:-40000}"
MEDIA_HI="${MEDIA_HI:-40999}"

die()  { printf '\nERROR: %s\n' "$*" >&2; exit 1; }
info() { printf '  %s\n' "$*"; }
step() { printf '\n==> %s\n' "$*"; }

[ "$(id -u)" -eq 0 ] || die "run this as root (sudo bash setup.sh)"

# ── platform detection ────────────────────────────────────────────────────────
step "Detecting platform"
os=$(uname -s)
arch=$(uname -m)
# Two candidates per platform, tried in order. The static musl build is
# preferred because it does not care about your glibc version, which matters on
# older distributions; the gnu build is the fallback.
case "$os:$arch" in
    Linux:x86_64)              assets="wolfvoice-x86_64-linux-musl wolfvoice-x86_64-linux-gnu" ;;
    Linux:aarch64|Linux:arm64) assets="wolfvoice-aarch64-linux-musl wolfvoice-aarch64-linux-gnu" ;;
    *) die "no prebuilt binary for $os/$arch — build from source:
    git clone https://github.com/$REPO && cd wolfvoice && cargo build --release
  (needs a Rust toolchain and cmake; macOS and Windows are build-from-source
   only, see .github/workflows/release.yml for why)" ;;
esac
info "$os/$arch  ->  ${assets%% *}"

command -v systemctl >/dev/null || die "systemd is required by this installer"

# ── gather configuration ──────────────────────────────────────────────────────
step "Configuration"

# The public address is the single most important setting and cannot be guessed:
# it is the ONLY address a viewer ever learns, because the Linden Lab voice
# protocol gives the server no way to trickle ICE candidates. Offer a best guess
# but make the operator confirm it.
guess=$(ip -4 -o addr show scope global 2>/dev/null | awk '{print $4}' | cut -d/ -f1 | head -1 || true)
if [ -n "${WOLFVOICE_PUBLIC_IP:-}" ]; then
    public_ip="$WOLFVOICE_PUBLIC_IP"
else
    printf '  Public IP viewers will send voice media to [%s]: ' "${guess:-none found}"
    read -r public_ip < /dev/tty || true
    public_ip="${public_ip:-$guess}"
fi
[ -n "$public_ip" ] || die "a public IP is required"
info "public media address : $public_ip"

if [ -n "${WOLFVOICE_HOSTNAME:-}" ]; then
    hostname_fqdn="$WOLFVOICE_HOSTNAME"
else
    printf '  DNS name for this service (must already point here, for TLS): '
    read -r hostname_fqdn < /dev/tty || true
fi
[ -n "$hostname_fqdn" ] || die "a DNS name is required — TLS is not optional here"
info "service hostname     : $hostname_fqdn"

resolved=$(getent hosts "$hostname_fqdn" | awk '{print $1}' | head -1 || true)
if [ -n "$resolved" ] && [ "$resolved" != "$public_ip" ]; then
    info "NOTE: $hostname_fqdn resolves to $resolved, not $public_ip."
    info "      That is fine behind NAT, but certbot must be able to reach port 80."
fi

# ── download ──────────────────────────────────────────────────────────────────
step "Downloading $asset"
command -v curl >/dev/null || die "curl is required"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

base="https://github.com/$REPO/releases/latest/download"
asset=""
for candidate in $assets; do
    if curl -fsSL "$base/$candidate" -o "$tmp/wolfvoice" 2>/dev/null \
    && curl -fsSL "$base/$candidate.sha256" -o "$tmp/sum" 2>/dev/null; then
        asset="$candidate"
        break
    fi
    info "not in this release: $candidate"
done
[ -n "$asset" ] || die "no usable binary for $os/$arch in the latest release"
info "using $asset"

step "Verifying checksum"
want=$(awk '{print $1}' "$tmp/sum")
got=$(sha256sum "$tmp/wolfvoice" | awk '{print $1}')
[ "$want" = "$got" ] || die "checksum mismatch: expected $want, got $got"
info "sha256 ok"

# ── install ───────────────────────────────────────────────────────────────────
step "Installing"
id "$SERVICE_USER" >/dev/null 2>&1 || \
    useradd --system --home-dir /var/lib/wolfvoice --create-home \
            --shell /usr/sbin/nologin "$SERVICE_USER"
info "service user: $SERVICE_USER"

install -o root -g root -m 0755 "$tmp/wolfvoice" "$PREFIX/wolfvoice"
info "binary: $PREFIX/wolfvoice"

install -d -o root -g "$SERVICE_USER" -m 0750 "$TLSDIR"

# ── TLS ───────────────────────────────────────────────────────────────────────
step "TLS certificate"
if [ -s "$TLSDIR/fullchain.pem" ] && [ -s "$TLSDIR/privkey.pem" ]; then
    info "certificate already present, leaving it alone"
else
    if ! command -v certbot >/dev/null; then
        info "installing certbot"
        if command -v apt-get >/dev/null; then
            DEBIAN_FRONTEND=noninteractive apt-get -qq update
            DEBIAN_FRONTEND=noninteractive apt-get -qq -y install certbot
        elif command -v dnf >/dev/null; then dnf -q -y install certbot
        else die "install certbot manually, then re-run"
        fi
    fi

    # Renewal hooks: open port 80 only while a challenge is in flight, and publish
    # the renewed cert where the unprivileged service can read it.
    for d in pre post deploy; do mkdir -p "/etc/letsencrypt/renewal-hooks/$d"; done
    if [ -f contrib/10-open-acme-port.sh ]; then
        install -m 0750 contrib/10-open-acme-port.sh    /etc/letsencrypt/renewal-hooks/pre/
        install -m 0750 contrib/10-close-acme-port.sh   /etc/letsencrypt/renewal-hooks/post/
        install -m 0750 contrib/10-install-wolfvoice-cert.sh /etc/letsencrypt/renewal-hooks/deploy/
        info "renewal hooks installed"
    else
        info "contrib/ not found (running from a piped script) — install the"
        info "renewal hooks by hand from the repo, or the service will keep"
        info "serving the OLD certificate after each renewal."
    fi

    info "requesting a certificate for $hostname_fqdn (port 80 must be reachable)"
    certbot certonly --standalone --non-interactive --agree-tos \
        --register-unsafely-without-email --key-type ecdsa -d "$hostname_fqdn" \
        || die "certbot failed — check that port 80 is open and DNS points here"

    RENEWED_LINEAGE="/etc/letsencrypt/live/$hostname_fqdn" \
        bash /etc/letsencrypt/renewal-hooks/deploy/10-install-wolfvoice-cert.sh 2>/dev/null || {
            install -o "$SERVICE_USER" -g "$SERVICE_USER" -m 0644 \
                "/etc/letsencrypt/live/$hostname_fqdn/fullchain.pem" "$TLSDIR/fullchain.pem"
            install -o "$SERVICE_USER" -g "$SERVICE_USER" -m 0640 \
                "/etc/letsencrypt/live/$hostname_fqdn/privkey.pem" "$TLSDIR/privkey.pem"
        }
    info "certificate published to $TLSDIR"
fi

# ── systemd ───────────────────────────────────────────────────────────────────
step "systemd unit"
unit=/etc/systemd/system/wolfvoice.service
if [ -f contrib/wolfvoice.service ]; then
    sed "s|^Environment=WOLFVOICE_PUBLIC_IP=.*|Environment=WOLFVOICE_PUBLIC_IP=$public_ip|" \
        contrib/wolfvoice.service > "$unit"
else
    cat > "$unit" <<UNIT
[Unit]
Description=wolfvoice — shared WebRTC voice service for OpenSimulator
After=network-online.target
Wants=network-online.target
StartLimitIntervalSec=0

[Service]
Type=simple
User=$SERVICE_USER
Group=$SERVICE_USER
ExecStart=$PREFIX/wolfvoice
Environment=RUST_LOG=info
Environment=WOLFVOICE_PUBLIC_IP=$public_ip
Restart=always
RestartSec=2
NoNewPrivileges=yes
PrivateTmp=yes
PrivateDevices=yes
ProtectSystem=strict
ProtectHome=yes
ReadOnlyPaths=$CONFDIR
CapabilityBoundingSet=
AmbientCapabilities=
RestrictAddressFamilies=AF_INET AF_INET6
MemoryMax=2G

[Install]
WantedBy=multi-user.target
UNIT
fi
systemctl daemon-reload
systemctl enable --now wolfvoice
sleep 2

if [ "$(systemctl is-active wolfvoice)" != "active" ]; then
    printf '\n'
    journalctl -u wolfvoice -n 20 --no-pager || true
    die "wolfvoice did not start — see the log above"
fi
info "wolfvoice is running"

# ── what is left for the operator ─────────────────────────────────────────────
cat <<EOF

────────────────────────────────────────────────────────────────────────────
 wolfvoice is installed and running. TWO things are still up to you.
────────────────────────────────────────────────────────────────────────────

1) FIREWALL. Open these, and no more:

     ${RPC_PORT}/tcp              from YOUR REGION HOSTS ONLY
     ${MEDIA_LO}-${MEDIA_HI}/udp      from anywhere (viewers dial in)

   The ${RPC_PORT}/tcp endpoint has NO AUTHENTICATION OF ITS OWN. OpenSim posts a
   bare JSON-RPC body with no credential, so that source restriction plus TLS is
   the entire security boundary. Do not expose it to the internet.

   MEASURE your region hosts' egress addresses — do not assume. A region in a
   NATed container presents its HOST's address. To measure: run  nc -l -p 9999
   here, then connect to this host on 9999 from the region machine.

   nftables example: contrib/nftables.conf.example

2) EACH OPENSIM REGION. Two separate things, and both are required:

   a) Copy contrib/wolfvoice.ini to  <region>/bin/config/wolfvoice.ini
      and set:
          WebRtcVoiceServerURI = https://$hostname_fqdn:${RPC_PORT}
      Your OpenSim MUST have the os-webrtc-janus addon compiled in — it provides
      WebRtcVoice.dll and the region-side capabilities. You do not need to run a
      Janus gateway alongside it. Then restart the region.

   b) ALLOW VOICE ON THE ESTATE AND THE PARCEL.
          About Land -> Sound -> Allow Voice Chat
      This is the most common reason for "it does nothing": with voice disallowed
      on the parcel the viewer never even asks the region for voice, so nothing
      appears in any log. See docs/TROUBLESHOOTING.md.

Check it is alive (from a permitted region host):

     curl https://$hostname_fqdn:${RPC_PORT}/
     -> {"rooms":0,"service":"wolfvoice","sessions":0}

Logs:   journalctl -u wolfvoice -f
Docs:   https://github.com/$REPO

EOF
