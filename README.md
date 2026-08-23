# wolfvoice

Spatial WebRTC voice for [OpenSimulator](http://opensimulator.org) — one service that
Firestorm and browser-based viewers both reach through the region's own capabilities,
so users of different viewers hear each other, positionally.

```
Firestorm 7.1.10+  /  browser viewer
        │  ProvisionVoiceAccountRequest + VoiceSignalingRequest   (LLSD caps)
        ▼
OpenSim region  ──  os-webrtc-janus used ONLY as a JSON-RPC relay
        │           (no Janus server, no OpenSim source changes)
        ▼
   wolfvoice  ── answers the SDP, terminates DTLS/SRTP/SCTP,
        │         mixes a SEPARATE stream per listener
        ▼
   viewers   (UDP media)
```

## What makes this different

Linden Lab's WebRTC voice puts spatialisation, per-user gain and mute on the
**server**: the viewer sends its position, gain and mute requests *up* a data channel
and receives one already-mixed audio stream plus a participant roster *down* it.

wolfvoice therefore computes **a distinct mix for every listener**. If A is east of B,
A hears B on their right while B simultaneously hears A on their left — which a single
shared room mix cannot express, no matter how it is panned.

That is also why the usual `os-webrtc-janus` + Janus AudioBridge setup lists "no
spatial audio, no white dots, no muting, no individual volume" as known issues. All
four have one root cause: **AudioBridge rejects the data channel.** It has no
`incoming_data` callback, and Janus's SDP helper rejects an unclaimed
`m=application` line with `port = 0` — so position, gain, mute and the roster, which
all travel on that channel, never arrive. Worse, the viewer will not leave
`VOICE_STATE_WAIT_FOR_DATA_CHANNEL` until the channel opens.

wolfvoice accepts that data channel, and does the mixing itself.

## Quick start

```bash
curl -fsSLO https://raw.githubusercontent.com/wolfsoftwaresystemsltd/wolfvoice/main/setup.sh
less setup.sh          # it runs as root; read it first
sudo bash setup.sh
```

The installer detects your platform, downloads and checksums the matching release
binary, creates an unprivileged service user, obtains a TLS certificate with
certbot (including renewal hooks), and installs a hardened systemd unit. It then
prints the two things it will not do for you: your firewall rules, and the
per-region OpenSim config.

## What you need

**For the voice server**

- A Linux host with a **public IP address** and a **DNS name** pointing at it.
  TLS is not optional: the SDP carries the server's DTLS fingerprint, so anyone able
  to rewrite plaintext signalling in flight could become the media endpoint.
- **UDP 40000–40999** open to the world. This range is also the ceiling on
  concurrent sessions — one socket per peer connection.
- **TCP 9443** reachable *from your region hosts only*. This endpoint has no
  authentication of its own.
- Modest CPU. Measured cost is **~14 ms of CPU per listener per second**, and it
  scales with listeners rather than listeners². See [Capacity](#capacity).

**For OpenSimulator**

- The [os-webrtc-janus](https://github.com/Misterblue/os-webrtc-janus) addon compiled
  into your OpenSim — it supplies `WebRtcVoice.dll`. **You do not need Janus itself**;
  wolfvoice uses only that addon's JSON-RPC connector, which will post to any URL.
- Voice allowed on the **estate** *and* on each **parcel**. This is the most common
  reason for "nothing happens" — see [docs/TROUBLESHOOTING.md](docs/TROUBLESHOOTING.md).

**For viewers**

- **Firestorm 7.1.10 or newer** works with no configuration at all — the region tells
  it which voice system to use. Nothing to install, nothing to tick.
- Browser viewers need a client that speaks the same contract.
  [`client/voice_llwebrtc.js`](client/voice_llwebrtc.js) is a working reference
  implementation, extracted from WolfStorm.

## Documentation

| | |
|---|---|
| [docs/SERVER.md](docs/SERVER.md) | Installing, running and operating the service; TLS; firewall; capacity |
| [docs/OPENSIM.md](docs/OPENSIM.md) | Region configuration, the addon, estate/parcel flags, rolling out to many regions |
| [docs/CLIENT.md](docs/CLIENT.md) | Viewer support, and writing your own client |
| [docs/PROTOCOL.md](docs/PROTOCOL.md) | The complete wire contract, with citations into the Firestorm and OpenSim sources |
| [docs/TROUBLESHOOTING.md](docs/TROUBLESHOOTING.md) | Every failure we hit, and how to tell them apart |

## Capacity

Measured with `cargo run --release --example load_test`:

| Clients | Speaking | Service CPU | Per listener |
|---|---|---|---|
| 10 | 5 | 12 % of one core | 12.3 ms/s |
| 40 | 20 | 55 % of one core | 13.8 ms/s |

Cost is linear in listeners — the per-listener Opus encode dominates, and the
O(N²) frame summing is comparatively free. **The mix loop is currently a single
task, so it is confined to one core**, which puts the practical ceiling around
**70 simultaneous voice users**. Rooms are independent, so spreading them across
cores is the obvious way to lift it when someone needs to.

## Building from source

Needs a Rust toolchain and `cmake` (the `opus` crate compiles libopus from source).

```bash
cargo build --release
cargo test              # 31 tests, no network required
```

Release binaries for Linux (x86_64/aarch64, glibc and static musl), macOS
(Intel/Apple Silicon) and Windows are built by GitHub Actions — see
[.github/workflows/release.yml](.github/workflows/release.yml).

## Testing it without a viewer

```bash
# Correctness: two synthetic viewers, real SDP, real Opus, real mixing.
cargo run --release --example two_clients -- https://127.0.0.1:9443
# -> AUDIO_MIXED=true ROSTER_DELIVERED=true LEAVE_NOTIFIED=true / PASS

# Capacity: N clients, M speaking, reports service CPU.
cargo run --release --example load_test -- https://127.0.0.1:9443 40 20
```

Run these on the voice host (the JSON-RPC port is firewalled to region hosts, but
loopback is permitted). Note that both harnesses share the server's own WebRTC
crate, so they cannot catch a shared misreading of the wire format — a real viewer
remains the final test.

## Known limitations

- **No TURN, and no way to add one.** Firestorm hardcodes its STUN servers to
  `stun:stunN.<grid>.secondlife.io`, which do not resolve outside Second Life. Media
  still connects, because the server advertises a routable host candidate and learns
  the viewer's address from the connectivity check — but a user whose network blocks
  outbound UDP cannot use voice at all. The client is told, rather than failing
  silently.
- **Single instance.** If wolfvoice is down, voice is down everywhere it is
  configured. `Restart=always` covers a crash, not a host failure.
- **Group and IM voice** (`channel_type: multiagent`) is implemented and rooms are
  keyed correctly, but has had far less exercise than spatial voice.

## Credits

Built by **Wolf Software Systems Ltd** for the
[Wolf Territories Grid](https://wolfterritories.org), and released for the wider
OpenSimulator community.

Interoperates with [os-webrtc-janus](https://github.com/Misterblue/os-webrtc-janus)
by Robert Adams, which supplies the region-side capability handlers this service
talks to. The wire protocol is Linden Lab's, as implemented in
[Firestorm](https://www.firestormviewer.org/).

## Licence

Apache License 2.0 — see [LICENSE](LICENSE).
