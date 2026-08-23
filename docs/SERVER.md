# Running the wolfvoice server

## Requirements

- **Linux** with systemd (the installer is Linux-only; the binary itself also builds
  and runs on macOS and Windows).
- **A public IP address.** Not optional, and not something the service can discover.
  It is the *only* address a viewer ever learns, because Linden Lab's voice protocol
  gives the server no channel on which to trickle ICE candidates — our candidate list
  has to be complete in the SDP answer we return. Behind 1:1 NAT, advertise the
  public address and forward the media range.
- **A DNS name** pointing at that address, for TLS.
- **Ports**: TCP 9443 from your region hosts only; UDP 40000–40999 from anywhere.
- **CPU**: about 14 ms per listener per second. See [Capacity](#capacity).
- To build from source: a Rust toolchain and `cmake` (the `opus` crate compiles
  libopus itself).

## Install

```bash
curl -fsSLO https://raw.githubusercontent.com/wolfsoftwaresystemsltd/wolfvoice/main/setup.sh
less setup.sh
sudo bash setup.sh
```

Or by hand:

```bash
# 1. binary
curl -fsSLO https://github.com/wolfsoftwaresystemsltd/wolfvoice/releases/latest/download/wolfvoice-x86_64-linux-musl
curl -fsSLO https://github.com/wolfsoftwaresystemsltd/wolfvoice/releases/latest/download/wolfvoice-x86_64-linux-musl.sha256
sha256sum -c wolfvoice-x86_64-linux-musl.sha256
sudo install -m0755 wolfvoice-x86_64-linux-musl /usr/local/bin/wolfvoice

# 2. unprivileged user
sudo useradd --system --home-dir /var/lib/wolfvoice --create-home \
             --shell /usr/sbin/nologin wolfvoice

# 3. TLS (see below), then the unit
sudo cp contrib/wolfvoice.service /etc/systemd/system/
sudo sed -i 's/203.0.113.10/YOUR.PUBLIC.IP/' /etc/systemd/system/wolfvoice.service
sudo systemctl daemon-reload && sudo systemctl enable --now wolfvoice
```

## Configuration

Everything is environment variables; there is no config file.

| Variable | Required | Meaning |
|---|---|---|
| `WOLFVOICE_PUBLIC_IP` | **yes** | Public address to advertise in ICE candidates. The service refuses to start without it rather than guess. |
| `RUST_LOG` | no | `info` (default), or `debug` for per-session detail. |

Ports and paths are compile-time constants at the top of `src/main.rs`:
`RPC_BIND` (0.0.0.0:9443), `MEDIA_PORT_LO`/`HI` (40000–40999), and the TLS paths
under `/etc/wolfvoice/tls/`. If you change the media range, change your firewall to
match — they must agree.

## TLS

The service reads `/etc/wolfvoice/tls/fullchain.pem` and `privkey.pem`, owned by the
`wolfvoice` user (key mode 0640). It does **not** read `/etc/letsencrypt` directly,
which is root-only.

`setup.sh` installs three certbot hooks from `contrib/`:

- **pre**: adds port 80 to an nftables set, opening the ACME window
- **post**: removes it again — so 80 is shut except for the seconds a renewal needs
- **deploy**: republishes the renewed cert into `/etc/wolfvoice/tls` and reloads the
  service

Verify the whole renewal path, hooks included:

```bash
sudo certbot renew --dry-run --no-random-sleep-on-renew --run-deploy-hooks
```

Two things worth knowing. `--dry-run` alone does **not** run deploy hooks, so a
broken deploy hook is invisible until the real renewal 60 days later — pass
`--run-deploy-hooks` and confirm the files in `/etc/wolfvoice/tls` actually change
(back-date them with `touch -d 2020-01-01` first if you want to be certain). And
`certbot renew` in non-interactive mode sleeps a random delay of up to 12 minutes
before doing anything, which looks exactly like a hang; `--no-random-sleep-on-renew`
is what the systemd timer itself passes.

## Firewall

See `contrib/nftables.conf.example`. Two rules matter:

```
ip saddr @voice_rpc_clients tcp dport 9443 accept   # region hosts ONLY
udp dport 40000-40999 accept                        # viewers, from anywhere
```

**The JSON-RPC endpoint has no authentication of its own.** OpenSim's connector posts
a bare JSON-RPC body with no credential, so that source restriction plus TLS is the
entire boundary. Identity is still trustworthy — the *region* tells us which agent a
request belongs to, and the viewer never gets to assert who it is — but anyone who can
reach the port can create sessions.

**Measure your region hosts' egress addresses; do not assume them.** A region in a
NATed container presents its host's address, which may be nothing like the name you
ssh to. To measure: `nc -l -p 9999` on the voice host, then connect to it on 9999
from the region machine — the listener prints the address you need.

If you are applying a default-drop ruleset over SSH, arm a rollback first:

```bash
systemd-run --on-active=240 --unit=nft-rollback /usr/sbin/nft flush ruleset
nft -f /etc/nftables.conf
# now prove a BRAND NEW ssh connection works — your existing one survives on
# `ct state established` and proves nothing
systemctl stop nft-rollback.timer
```

## Operating

```bash
systemctl status wolfvoice
journalctl -u wolfvoice -f

# health, from a permitted region host or over loopback
curl -k https://127.0.0.1:9443/
# {"rooms":1,"service":"wolfvoice","sessions":3}
```

Useful log lines:

- `session wv-… agent … region … port 40007 spatial=true (N live)` — a session was
  provisioned
- `connection state Connected` — ICE and DTLS completed
- `data channel SLData` — the channel opened; this is the point at which a viewer
  considers voice usable
- `answer SDP carries NO ICE candidates` — **fatal misconfiguration**; check
  `WOLFVOICE_PUBLIC_IP` and that the media range is bindable

## Capacity

Measure your own with `examples/load_test.rs`; ours, on a 12-core VM:

| Clients | Speaking | Service CPU | Per listener |
|---|---|---|---|
| 10 | 5 | 12 % of one core | 12.3 ms/s |
| 40 | 20 | 55 % of one core | 13.8 ms/s |

Cost is linear in listeners; the per-listener Opus encode dominates and the O(N²)
frame summing is comparatively free. Speaker count barely matters.

Two ceilings, in the order you will hit them:

1. **~70 concurrent listeners** — `mixer_loop` is a single task, so all mixing runs
   on one core. Past this the 20 ms tick slips and audio breaks up. Rooms are
   independent, so fanning them across cores lifts this by roughly the core count.
2. **1000 sessions** — one UDP socket per peer connection, bounded by the media port
   range. Widen the range and `MEDIA_PORT_HI` together.

Sessions are per viewer *per audible region*: a viewer near a region corner can hold
up to four at once, because Firestorm probes eight compass directions at twice its
50 m audio range and opens a connection for each WebRTC-enabled region it finds.

## Upgrading

```bash
sudo systemctl stop wolfvoice
sudo install -m0755 wolfvoice-new /usr/local/bin/wolfvoice
sudo systemctl start wolfvoice
```

Sessions do not survive a restart, but viewers re-provision automatically within a
few seconds, so an upgrade costs a short gap rather than manual intervention.
