//! wolfvoice — the shared WebRTC voice service for WolfStorm and Firestorm.
//!
//! The region never talks WebRTC itself. OpenSim's os-webrtc-janus addon is used
//! purely as a relay: `WebRtcVoice.dll:WebRtcVoiceServiceConnector` forwards the
//! viewer's ProvisionVoiceAccountRequest / VoiceSignalingRequest capabilities to
//! this process as JSON-RPC, and we are the actual WebRTC peer for every viewer.
//!
//! Because both Firestorm and WolfStorm reach us through the same region
//! capability, they land in the same rooms with the same spatialisation.

mod mixer;
mod proto;
mod room;
mod session;

use bytes::Bytes;
use hyper::body::HttpBody as _;
use hyper::service::service_fn;
use hyper::{Body, Method, Request, Response, StatusCode};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;

use room::{RoomKey, Session};
use session::{Endpoint, PortPool};

/// Where the JSON-RPC listener binds. TLS only, deliberately: the SDP carries this
/// server's DTLS fingerprint, so an attacker able to rewrite signalling in flight
/// could substitute their own and become the media endpoint. Restrict this port to
/// your region hosts at the firewall as well — it has no authentication of its own.
const RPC_BIND: &str = "0.0.0.0:9443";

/// Public address our ICE host candidates must advertise.
///
/// REQUIRED — there is deliberately no default. This must be an address that
/// viewers on the public internet can reach, because it is the only address they
/// ever learn: the Linden Lab voice protocol gives the server no way to trickle
/// candidates, so our candidate list has to be complete in the SDP answer.
///
/// If the machine has the address directly attached (the common case on a VPS),
/// use it as-is. Behind 1:1 NAT you must still advertise the PUBLIC address here,
/// and forward the media port range to this host.
const PUBLIC_IP_ENV: &str = "WOLFVOICE_PUBLIC_IP";

/// Media port range. Must match the UDP range opened in your firewall, and is also
/// the ceiling on concurrent sessions (one socket per peer connection).
const MEDIA_PORT_LO: u16 = 40000;
const MEDIA_PORT_HI: u16 = 40999;

const TLS_CERT: &str = "/etc/wolfvoice/tls/fullchain.pem";
const TLS_KEY: &str = "/etc/wolfvoice/tls/privkey.pem";

/// Mixer cadence. One Opus frame per tick per listener.
const TICK: Duration = Duration::from_millis(20);

/// Largest JSON-RPC body we will read. The biggest legitimate one is an SDP offer
/// plus a trickled ICE candidate list; a few hundred KiB is ample.
const MAX_RPC_BODY: usize = 256 * 1024;

/// Ceiling on concurrent sessions, checked before we allocate a media port.
/// Without it, anything able to reach the JSON-RPC port can exhaust the UDP port
/// range and the mixer's CPU budget just by asking for sessions. Kept below the
/// port range (MEDIA_PORT_HI - MEDIA_PORT_LO) so port allocation cannot wrap onto
/// a port still in use.
const MAX_SESSIONS: usize = 900;

struct App {
    sessions: room::Registry,
    endpoints: RwLock<HashMap<String, Arc<Endpoint>>>,
    ports: PortPool,
    public_ip: String,
    runtime: Arc<dyn webrtc::runtime::Runtime>,
}

impl App {
    fn endpoint(&self, id: &str) -> Option<Arc<Endpoint>> {
        self.endpoints.read().get(id).cloned()
    }

    /// Tear a session down completely: transport, registry and room membership,
    /// then tell everyone still in the room that this agent has gone.
    async fn drop_session(&self, id: &str) {
        let ep = self.endpoints.write().remove(id);
        let gone = self.sessions.remove(id);

        if let Some(ep) = ep {
            ep.close().await;
        }

        // Nothing else in the protocol expires a participant, so without this the
        // viewer keeps a departed avatar in its voice panel forever
        // (llvoicewebrtc.cpp:3212-3219 is the only removal path).
        if let Some(gone) = gone {
            // Only announce a departure if this agent has no OTHER session still in
            // the room — an agent legitimately holds one session per region it can
            // hear, and losing a neighbour connection does not mean they left.
            let remaining = self.sessions.members(&gone.room);
            let still_present = remaining.iter().any(|m| m.agent_id == gone.agent_id);
            if !still_present {
                let notice = proto::leave_json(&gone.agent_id);
                for m in remaining {
                    if let Some(ep) = self.endpoint(&m.id) {
                        if ep.data_channel_open() {
                            ep.send_roster(&notice).await;
                        }
                    }
                }
            }
        }

        log::info!("session {id} removed ({} live)", self.sessions.session_count());
    }

    /// ProvisionVoiceAccountRequest.
    ///
    /// Two shapes arrive here: a teardown (`logout`) and a connection request
    /// carrying an SDP offer. Source: llvoicewebrtc.cpp:2731-2734 and :2794-2802.
    async fn provision(&self, params: &proto::RpcParams) -> Result<Value, String> {
        let req = proto::ProvisionRequest::parse(&params.request)?;

        if req.logout {
            let Some(id) = req.viewer_session.clone() else {
                return Err("logout without viewer_session".into());
            };
            self.drop_session(&id).await;
            // The viewer ignores this body (breakVoiceConnectionCoro discards the
            // result at llvoicewebrtc.cpp:2748), but the region-side connector
            // still expects a map.
            return Ok(json!({ "viewer_session": id }));
        }

        let offer = req
            .offer_sdp
            .as_deref()
            .ok_or_else(|| "provision without a jsep offer".to_string())?;

        // A renegotiation for an existing session: drop the old transport first so
        // we never leave two peer connections fighting over one agent.
        if let Some(existing) = req.viewer_session.clone() {
            if self.endpoint(&existing).is_some() {
                log::info!("session {existing} re-provisioning; dropping old transport");
                self.drop_session(&existing).await;
            }
        }

        // Refuse politely rather than exhausting ports or CPU. The viewer treats a
        // failed provision as retryable, so this degrades to "voice unavailable"
        // instead of taking the service down for everyone already connected.
        if self.sessions.session_count() >= MAX_SESSIONS {
            log::warn!(
                "refusing provision: at the {MAX_SESSIONS}-session ceiling (agent {})",
                params.user_id
            );
            return Err("voice service is at capacity".into());
        }

        // Identity comes from the REGION (params.user_id), never from the viewer's
        // own request body. That is the whole security value of routing voice
        // through the capability: the region issued the cap per-agent and told us
        // who it belongs to.
        if params.user_id.is_empty() {
            return Err("region did not supply userID".into());
        }

        let channel_type = req.channel_type.unwrap_or(proto::ChannelType::Local);
        let spatial = channel_type == proto::ChannelType::Local;
        let room = match channel_type {
            proto::ChannelType::Local => RoomKey::Spatial {
                region: params.scene.clone(),
                parcel: req.parcel_local_id,
            },
            proto::ChannelType::MultiAgent => RoomKey::MultiAgent {
                channel: req
                    .channel
                    .clone()
                    .ok_or_else(|| "multiagent request without a channel".to_string())?,
            },
        };

        let id = format!("wv-{}", uuid::Uuid::new_v4());
        let sess = Arc::new(Session::new(
            id.clone(),
            params.user_id.clone(),
            room,
            spatial,
        ));

        let port = self.ports.take();
        let ep = session::establish(
            sess.clone(),
            offer,
            &self.public_ip,
            port,
            self.runtime.clone(),
        )
        .await?;

        let sdp = session::answer_sdp(&ep)
            .await
            .ok_or_else(|| "no answer sdp".to_string())?;

        self.endpoints.write().insert(id.clone(), Arc::new(ep));
        self.sessions.insert(sess);

        log::info!(
            "session {id} agent {} region {} port {port} spatial={spatial} ({} live)",
            params.user_id,
            params.scene,
            self.sessions.session_count()
        );

        Ok(proto::provision_answer(&id, &sdp))
    }

    /// VoiceSignalingRequest — trickled ICE candidates.
    /// Source: llvoicewebrtc.cpp:2496-2519.
    async fn signaling(&self, params: &proto::RpcParams) -> Result<Value, String> {
        let req = proto::SignalingRequest::parse(&params.request);
        let Some(id) = req.viewer_session.clone() else {
            return Err("signaling without viewer_session".into());
        };
        let Some(ep) = self.endpoint(&id) else {
            // Not fatal: the viewer trickles candidates concurrently with the
            // provision call and may reference a session we already tore down.
            return Err(format!("unknown viewer_session {id}"));
        };

        for c in &req.candidates {
            if let Err(e) = ep.add_ice_candidate(c).await {
                log::debug!("session {id} candidate rejected: {e}");
            }
        }
        if req.completed {
            log::debug!("session {id} end-of-candidates");
        }
        Ok(json!({ "viewer_session": id }))
    }
}

/// Mixer loop: one pass per room per 20 ms, spread across all available cores.
///
/// The work parallelises on two independent axes, and we use both:
///
///   * **rooms** never interact, so each room's tick is its own task;
///   * **listeners within a room** each need their own Opus encode, which is the
///     dominant cost, and those are independent too — so each listener is its own
///     task as well. That second axis matters because the interesting case is one
///     busy region, where per-room parallelism alone would still leave everything
///     on a single core.
///
/// Task churn is real but small against the work: an encode is hundreds of
/// microseconds, a tokio spawn is a couple.
///
/// Overload behaviour is deliberate. A tick is skipped entirely if the previous
/// tick's tasks have not all finished. That gives two things at once: outstanding
/// work for one endpoint can never overlap itself (which would let two frames race
/// and emit RTP timestamps out of order), and a server that cannot keep up degrades
/// by dropping whole frames — audible, but bounded — rather than growing a task
/// queue until it dies.
async fn mixer_loop(app: Arc<App>) {
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let inflight = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut skipped: u64 = 0;
    let mut last_warned = std::time::Instant::now();

    loop {
        ticker.tick().await;

        if inflight.load(Ordering::Acquire) != 0 {
            skipped += 1;
            // Rate-limit the complaint; at 50 ticks a second an unthrottled log
            // would itself become the bottleneck.
            if last_warned.elapsed() >= Duration::from_secs(10) {
                log::warn!(
                    "mixer overloaded: {skipped} tick(s) skipped in the last 10s \
                     ({} sessions). Audio will break up; the host needs more CPU \
                     or fewer concurrent listeners.",
                    app.sessions.session_count()
                );
                skipped = 0;
                last_warned = std::time::Instant::now();
            }
            continue;
        }

        // Reap anything the transport marked dead before mixing.
        let dead: Vec<String> = app
            .endpoints
            .read()
            .values()
            .filter(|ep| ep.session.closed.load(Ordering::Relaxed))
            .map(|ep| ep.session.id.clone())
            .collect();
        for id in dead {
            app.drop_session(&id).await;
        }

        for key in app.sessions.rooms() {
            let members = app.sessions.members(&key);
            if members.len() < 2 {
                // Nobody to mix for. Still drain the jitter buffers so a lone
                // participant's audio does not pile up until someone joins.
                for m in &members {
                    while m.take_frame().is_some() {}
                    m.level.store(0, Ordering::Relaxed);
                }
                continue;
            }

            let app = app.clone();
            let inflight = inflight.clone();
            inflight.fetch_add(1, Ordering::AcqRel);
            tokio::spawn(async move {
                // Summing is cheap next to the encodes and needs the whole room's
                // frames at once, so it stays inline in the room's own task.
                let outputs = room::mix_room(&members);

                let mut handles = Vec::with_capacity(outputs.len());
                for out in outputs {
                    let app = app.clone();
                    handles.push(tokio::spawn(async move {
                        let Some(ep) = app.endpoint(&out.session.id) else {
                            return;
                        };
                        // Nothing can be sent before the data channel opens, and
                        // the viewer is not in VOICE_STATE_SESSION_UP until it does.
                        if !ep.data_channel_open() {
                            return;
                        }
                        if let Some(roster) = out.roster {
                            ep.send_roster(&roster).await;
                        }
                        if let Err(e) = ep.send_mix(&out.stereo).await {
                            log::debug!("send_mix {}: {e}", out.session.id);
                        }
                    }));
                }
                for h in handles {
                    let _ = h.await;
                }
                inflight.fetch_sub(1, Ordering::AcqRel);
            });
        }
    }
}

// ─────────────────────────── HTTP / JSON-RPC ───────────────────────────

async fn handle(app: Arc<App>, req: Request<Body>) -> Result<Response<Body>, hyper::Error> {
    // A tiny health endpoint, reachable only from the allow-listed region hosts.
    if req.method() == Method::GET {
        let body = json!({
            "service": "wolfvoice",
            "sessions": app.sessions.session_count(),
            "rooms": app.sessions.rooms().len(),
        });
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap());
    }

    if req.method() != Method::POST {
        return Ok(Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .body(Body::empty())
            .unwrap());
    }

    // Bounded read. `to_bytes` would buffer the entire body with no limit, so a
    // single large POST could drive the process into its MemoryMax and get it
    // OOM-killed — a trivial denial of service for anyone who can reach the port.
    // The largest legitimate body is an SDP offer plus a trickled candidate list.
    let mut body = req.into_body();
    let mut whole = bytes::BytesMut::new();
    while let Some(chunk) = body.data().await {
        let chunk = chunk?;
        if whole.len() + chunk.len() > MAX_RPC_BODY {
            log::warn!(
                "rejecting oversize JSON-RPC body (>{} bytes) — possible abuse",
                MAX_RPC_BODY
            );
            return Ok(json_200(&proto::rpc_err(&Value::Null, "request body too large")));
        }
        whole.extend_from_slice(&chunk);
    }
    Ok(rpc_response(app, whole.freeze()).await)
}

/// Always answers 200 with a JSON object.
///
/// This is not sloppiness, it is required. OpenSim's connector calls
/// EnsureSuccessStatusCode (WebUtil.cs:428) which throws on any non-2xx, and then
/// WebRtcVoiceServiceConnector.cs:149-160 sets the task result inside the catch and
/// STILL falls through to dereference the now-null response — a NullReferenceException
/// on a detached task, with the viewer left waiting forever. A 200 carrying
/// {"error": ...} is the only way to report a failure that the region can see.
async fn rpc_response(app: Arc<App>, body: Bytes) -> Response<Body> {
    let parsed: Result<proto::RpcRequest, _> = serde_json::from_slice(&body);
    let (id, method, params) = match parsed {
        Ok(r) => (r.id, r.method, r.params),
        Err(e) => {
            return json_200(&proto::rpc_err(&Value::Null, &format!("bad json-rpc: {e}")));
        }
    };

    let result = match method.as_str() {
        proto::METHOD_PROVISION => app.provision(&params).await,
        proto::METHOD_SIGNALING => app.signaling(&params).await,
        other => Err(format!("unknown method {other:?}")),
    };

    match result {
        Ok(v) => json_200(&proto::rpc_ok(&id, v)),
        Err(e) => {
            log::warn!("{method} failed: {e}");
            json_200(&proto::rpc_err(&id, &e))
        }
    }
}

fn json_200(v: &Value) -> Response<Body> {
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(Body::from(v.to_string()))
        .unwrap()
}

fn load_tls() -> Result<rustls::ServerConfig, String> {
    let certs = {
        let f = std::fs::File::open(TLS_CERT).map_err(|e| format!("{TLS_CERT}: {e}"))?;
        let mut r = std::io::BufReader::new(f);
        rustls_pemfile::certs(&mut r)
            .map_err(|e| format!("{TLS_CERT}: {e}"))?
            .into_iter()
            .map(rustls::Certificate)
            .collect::<Vec<_>>()
    };
    if certs.is_empty() {
        return Err(format!("{TLS_CERT} contained no certificates"));
    }

    let key = {
        let f = std::fs::File::open(TLS_KEY).map_err(|e| format!("{TLS_KEY}: {e}"))?;
        let mut r = std::io::BufReader::new(f);
        // certbot issues ECDSA keys by default in v4, which land in the PKCS#8
        // section, but accept an RSA key too so a key-type change cannot brick us.
        let mut keys = rustls_pemfile::pkcs8_private_keys(&mut r)
            .map_err(|e| format!("{TLS_KEY}: {e}"))?;
        if keys.is_empty() {
            let f = std::fs::File::open(TLS_KEY).map_err(|e| format!("{TLS_KEY}: {e}"))?;
            let mut r = std::io::BufReader::new(f);
            keys = rustls_pemfile::rsa_private_keys(&mut r)
                .map_err(|e| format!("{TLS_KEY}: {e}"))?;
        }
        if keys.is_empty() {
            let f = std::fs::File::open(TLS_KEY).map_err(|e| format!("{TLS_KEY}: {e}"))?;
            let mut r = std::io::BufReader::new(f);
            keys = rustls_pemfile::ec_private_keys(&mut r)
                .map_err(|e| format!("{TLS_KEY}: {e}"))?;
        }
        rustls::PrivateKey(
            keys.into_iter()
                .next()
                .ok_or_else(|| format!("{TLS_KEY} contained no usable private key"))?,
        )
    };

    rustls::ServerConfig::builder()
        .with_safe_defaults()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("tls config: {e}"))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // Fail loudly rather than guessing: a wrong public address produces a service
    // that answers every request and then never connects, which is a miserable
    // thing to debug.
    let public_ip = match std::env::var(PUBLIC_IP_ENV) {
        Ok(v) if !v.trim().is_empty() => v,
        _ => {
            eprintln!(
                "{PUBLIC_IP_ENV} is not set.\n\n\
                 Set it to the PUBLIC IP address viewers should send voice media to,\n\
                 e.g. {PUBLIC_IP_ENV}=203.0.113.10\n\n\
                 It cannot be guessed: it is the only address the viewer ever learns."
            );
            std::process::exit(2);
        }
    };

    let app = Arc::new(App {
        sessions: room::Registry::default(),
        endpoints: RwLock::new(HashMap::new()),
        ports: PortPool::new(MEDIA_PORT_LO, MEDIA_PORT_HI),
        public_ip: public_ip.clone(),
        runtime: Arc::new(webrtc::runtime::TokioRuntime),
    });

    tokio::spawn(mixer_loop(app.clone()));

    let tls = Arc::new(load_tls()?);
    let acceptor = tokio_rustls::TlsAcceptor::from(tls);
    let addr: SocketAddr = RPC_BIND.parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;

    log::info!(
        "wolfvoice listening on {addr} (TLS), media {MEDIA_PORT_LO}-{MEDIA_PORT_HI}/udp on {public_ip}"
    );

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                log::warn!("accept: {e}");
                continue;
            }
        };
        let acceptor = acceptor.clone();
        let app = app.clone();
        tokio::spawn(async move {
            let tls_stream = match acceptor.accept(stream).await {
                Ok(s) => s,
                Err(e) => {
                    log::debug!("tls handshake from {peer}: {e}");
                    return;
                }
            };
            let svc = service_fn(move |req| handle(app.clone(), req));
            if let Err(e) = hyper::server::conn::Http::new()
                .http1_only(true)
                .serve_connection(tls_stream, svc)
                .await
            {
                log::debug!("connection from {peer}: {e}");
            }
        });
    }
}
