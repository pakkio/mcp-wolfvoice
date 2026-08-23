# Troubleshooting

Work down this list — it is ordered by how often each thing is actually the problem.
Each entry tells you how to *distinguish* it, not just what it is.

## Nothing happens at all, and the region log shows no provision request

The region log shows `setting VoiceServerType=webrtc` for your agent, and then
nothing. No `[ProvisionVoice]` line ever appears.

**Cause: voice is not allowed on the parcel or the estate.** The viewer checks both
locally and, finding voice unavailable, never asks the region for anything. There is
nothing in any log to say so.

- About Land → Sound → **Allow Voice Chat** (parcel flag, bit 29)
- Region/Estate → Estate → **Allow Voice Chat** (`EstateSettings.AllowVoice`)

This is by far the most common cause. Check it before anything else.

## Firestorm shows "unable to connect to the voice server: www.bhr.vivox.com"

**This is a Firestorm bug and unrelated to WebRTC voice.** `LLVoiceClient::tuningStart()`
calls `tuningStart()` on *both* voice modules unconditionally, and the Vivox one
launches a doomed login to Vivox's own servers, which eventually raises this alert.

The trigger is opening **Preferences → Sound & Media → Voice → Audio Device
Settings**. WebRTC voice can be working perfectly at the same time. Tick "Do not show
me this again", or avoid that panel.

Note that Vivox's `userAuthorized` does *not* start anything, so this is not a
login-time effect — it is that panel specifically.

## Sessions are created but never reach "Connected"

`journalctl -u wolfvoice` shows `session wv-… port 400xx` but never
`connection state Connected`.

**First check your own answer.** If the log shows
`answer SDP carries NO ICE candidates`, that is fatal and the fix is configuration:
`WOLFVOICE_PUBLIC_IP` must be set to a genuinely reachable public address, and the
media port range must be bindable.

**Then check UDP reachability.** The viewer must be able to reach
`WOLFVOICE_PUBLIC_IP` on the media range. Confirm the firewall opens
`udp dport 40000-40999` to the world, and that any upstream NAT forwards it.

**If it is one particular user**, it may simply be their network. There is no TURN
and no way to add one, because Firestorm hardcodes its STUN servers to
`stun:stunN.<grid>.secondlife.io` — which do not resolve outside Second Life. A user
whose network blocks outbound UDP cannot use voice at all. The reference client
reports this rather than failing silently.

> A trap worth knowing: this class of fault is **invisible when you test on
> loopback**, because the server can reach the client's own host candidates and the
> client learns the server's address peer-reflexively. It only appears across NAT.
> `examples/two_clients.rs` passes either way.

## Connected, data channel open, but no audio

Check that both participants have actually sent a position. A session that has never
reported one is deliberately **not mixed** — otherwise every silent newcomer would be
audible to anyone standing near `<0,0,0>`.

With `MessageDetails = true` on the region you will see the provision bodies; in the
service log at `RUST_LOG=debug` you will see data-channel traffic.

Then check distance. Beyond 60 m a speaker is silent by design, and past 50 m the
listener's ear is clamped back toward their avatar.

## Firestorm uses Vivox even though the region says webrtc

The region log will show a provision request whose body is just
`{"voice_server_type":"vivox"}`.

That body comes from `LLVivoxVoiceClient::provisionVoiceAccount`, which the viewer
runs from `userAuthorized` **independently of provider selection**. Seeing it does
*not* mean Vivox was chosen. Look for a *second* provision carrying a `jsep` offer —
if that is present, WebRTC is working and the Vivox request is just noise.

If there genuinely is no jsep provision, go back to the parcel/estate flags above.

## Only some participants hear each other

Check that everyone is in the same room. Spatial rooms are keyed on **region +
parcel**, so two people on different parcels of the same region are in different
rooms — which is correct behaviour when the parcel does not use the estate-wide voice
channel. Whether a parcel uses its own channel depends on its
`PF_USE_ESTATE_VOICE_CHAN` flag (bit 30).

## Participants show as UUIDs instead of names

A client-side issue: your viewer's roster is keyed on agent UUID (that is all the
protocol carries) and it is up to the client to resolve names. Resolution is usually
asynchronous, so a client must **retry** — the first lookup typically returns nothing
and the answer arrives moments later.

## Departed users stay in the voice panel

The server must send `{"<uuid>":{"l":true}}` when a session ends. Nothing else
expires a participant. wolfvoice sends it on session teardown, but only when that
agent has no *other* session left in the room — an agent legitimately holds one
session per audible region, and losing a neighbour connection does not mean they left.

## Audio breaks up under load

Look for this in the service log:

```
mixer overloaded: N tick(s) skipped in the last 10s
```

That is the mixer failing to finish a 20 ms tick before the next one is due, so it
skipped a frame rather than queueing work. The mixer already spreads across all
cores (one task per room, one per listener), so if you are seeing this the host
genuinely needs more CPU, or there are more concurrent listeners than it can carry.

Budget roughly **17 ms of CPU per listener per second** and confirm with
`examples/load_test.rs` on your own hardware.

If you see breakup with *no* overload warnings, it is not the mixer — look at network
loss between the viewer and the media ports.

## Certificate expired even though certbot ran

The renewal succeeded but the **deploy hook** did not run or failed, so
`/etc/wolfvoice/tls` still holds the old files. `certbot renew --dry-run` does *not*
exercise deploy hooks by default, which is exactly why this goes unnoticed:

```bash
sudo touch -d 2020-01-01 /etc/wolfvoice/tls/fullchain.pem
sudo certbot renew --dry-run --no-random-sleep-on-renew --run-deploy-hooks
ls -l /etc/wolfvoice/tls/     # the timestamps MUST have changed
```

## certbot appears to hang

In non-interactive mode `certbot renew` sleeps a random delay of up to 12 minutes
before doing anything. Pass `--no-random-sleep-on-renew`, which is what the systemd
timer itself uses.

## Region will not start after adding the config

A malformed ini stops OpenSim booting. Remove `bin/config/wolfvoice.ini`, confirm the
region starts, then re-add it — and always validate on **one** region before rolling
out to a fleet.

## Useful one-liners

```bash
# is the service alive and how busy?
curl -k https://127.0.0.1:9443/

# full end-to-end check without a viewer
cargo run --release --example two_clients -- https://127.0.0.1:9443

# did this region load the voice modules?
grep -E "REGION WEBRTC VOICE\]: enabled|WebRtcVoiceServiceConnector enabled" OpenSim.log

# is Vivox still active on this region? (should be 0 for the current boot)
grep -c VivoxVoice OpenSim.log

# what did the viewer actually ask for? (needs MessageDetails = true)
grep "ProvisionVoice\]: Request" OpenSim.log | tail -3
```
