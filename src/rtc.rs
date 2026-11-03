//! Native WebRTC transport. Video uses SRTP/UDP; ordered SCTP carries the same
//! input protocol as `/ws`. Signalling stays behind the server's normal login.

use crate::hub::{Client, Hub, Out};
use crate::server::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch, OwnedSemaphorePermit, Semaphore};
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_H264};
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::media::Sample;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_local::TrackLocal;

const MAX_PEERS: usize = 4;
const IDLE_SECS: u64 = 35;
const GATHER_TIMEOUT: Duration = Duration::from_secs(12);

pub struct Peers {
    peers: Mutex<HashMap<String, Arc<Peer>>>,
    slots: Arc<Semaphore>,
}

impl Peers {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            peers: Mutex::new(HashMap::new()),
            slots: Arc::new(Semaphore::new(MAX_PEERS)),
        })
    }

    async fn close(&self, id: &str) {
        let peer = self.peers.lock().remove(id);
        if let Some(peer) = peer {
            peer.stop().await;
        }
    }

    pub(crate) async fn close_all(&self) {
        let peers = std::mem::take(&mut *self.peers.lock());
        for (_, peer) in peers {
            peer.stop().await;
        }
    }
}

struct Peer {
    id: String,
    pc: Arc<RTCPeerConnection>,
    client: Arc<Client>,
    hub: Arc<Hub>,
    registry: Weak<Peers>,
    channel: Mutex<Option<Arc<RTCDataChannel>>>,
    state: AppState,
    rtts: Mutex<VecDeque<u32>>,
    quit: watch::Sender<bool>,
    closed: AtomicBool,
    last_seen: AtomicU64,
    epoch: Instant,
    _slot: OwnedSemaphorePermit,
}

impl Peer {
    async fn stop(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.quit.send_replace(true);
        self.client.video.store(false, Ordering::Relaxed);
        self.hub.remove_client(self.client.id);
        // A read-only viewer leaving must not release a controller's held keys.
        if !self.state.read_only {
            if let Some(tx) = self.hub.input.lock().as_ref() {
                let _ = tx.send(crate::input::InputEvent::ReleaseAll);
            }
        }
        let _ = self.pc.close().await;
        log::info!("RTC peer {} disconnected", self.id);
    }

    fn seen(&self) {
        self.last_seen
            .store(self.epoch.elapsed().as_secs(), Ordering::Relaxed);
    }

    fn hello(&self) -> String {
        let mut hello: serde_json::Value =
            serde_json::from_str(&self.hub.hello()).unwrap_or_default();
        hello["readOnly"] = self.state.read_only.into();
        hello["transport"] = "webrtc".into();
        hello.to_string()
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Offer {
    sdp: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    ice_servers: Vec<IceServer>,
    #[serde(default)]
    read_only: bool,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Urls {
    One(String),
    Many(Vec<String>),
}

#[derive(Deserialize)]
pub struct IceServer {
    urls: Urls,
    #[serde(default)]
    username: String,
    #[serde(default)]
    credential: String,
}

#[derive(Serialize)]
struct Answer {
    id: String,
    #[serde(rename = "type")]
    kind: &'static str,
    sdp: String,
}

fn error(status: StatusCode, message: impl AsRef<str>) -> Response {
    (
        status,
        Json(serde_json::json!({"message":message.as_ref()})),
    )
        .into_response()
}

fn ice_servers(servers: Vec<IceServer>) -> Result<Vec<RTCIceServer>, &'static str> {
    if servers.len() > 8 {
        return Err("Too many ICE servers.");
    }
    servers
        .into_iter()
        .map(|s| {
            let urls = match s.urls {
                Urls::One(u) => vec![u],
                Urls::Many(u) => u,
            };
            if urls.is_empty()
                || urls.len() > 8
                || s.username.len() > 512
                || s.credential.len() > 512
            {
                return Err("Invalid ICE server.");
            }
            for url in &urls {
                if url.len() > 512
                    || !["stun:", "stuns:", "turn:", "turns:"]
                        .iter()
                        .any(|p| url.starts_with(p))
                    || url.contains(['\r', '\n', '@'])
                {
                    return Err("Invalid ICE server URL.");
                }
                if url.starts_with("turn") && (s.username.is_empty() || s.credential.is_empty()) {
                    return Err("TURN credentials are required.");
                }
            }
            Ok(RTCIceServer {
                urls,
                username: s.username,
                credential: s.credential,
            })
        })
        .collect()
}

pub async fn offer(State(mut state): State<AppState>, Json(offer): Json<Offer>) -> Response {
    if state.stopping.load(Ordering::Acquire) {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "The desktop host is shutting down.",
        );
    }
    if offer.kind != "offer" || offer.sdp.len() > 512 * 1024 || offer.sdp.is_empty() {
        return error(
            StatusCode::BAD_REQUEST,
            "A bounded WebRTC SDP offer is required.",
        );
    }
    if state.hub.stripes.lock().len() > 1 {
        return error(StatusCode::CONFLICT, "Native WebRTC needs --stripes=1.");
    }
    let servers = match ice_servers(offer.ice_servers) {
        Ok(s) => s,
        Err(e) => return error(StatusCode::BAD_REQUEST, e),
    };
    let slot = match state.rtc.slots.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            return error(
                StatusCode::TOO_MANY_REQUESTS,
                "All desktop viewer slots are in use.",
            )
        }
    };
    state.read_only |= offer.read_only;
    let result = make_peer(state.clone(), servers, slot).await;
    let peer = match result {
        Ok(p) => p,
        Err(e) => {
            log::warn!("RTC setup failed: {e}");
            return error(StatusCode::BAD_GATEWAY, "Could not prepare WebRTC.");
        }
    };
    if state.stopping.load(Ordering::Acquire) {
        peer.stop().await;
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "The desktop host is shutting down.",
        );
    }
    state.rtc.peers.lock().insert(peer.id.clone(), peer.clone());
    supervise(Arc::downgrade(&peer));
    let negotiation = async {
        peer.pc
            .set_remote_description(RTCSessionDescription::offer(offer.sdp)?)
            .await?;
        let answer = peer.pc.create_answer(None).await?;
        let mut gathered = peer.pc.gathering_complete_promise().await;
        peer.pc.set_local_description(answer).await?;
        let _ = gathered.recv().await;
        peer.pc
            .local_description()
            .await
            .ok_or(webrtc::Error::ErrSessionDescriptionNoFingerprint)
    };
    match tokio::time::timeout(GATHER_TIMEOUT, negotiation).await {
        Ok(Ok(answer)) => Json(Answer {
            id: peer.id.clone(),
            kind: "answer",
            sdp: answer.sdp,
        })
        .into_response(),
        result => {
            log::warn!("RTC negotiation failed: {result:?}");
            state.rtc.close(&peer.id).await;
            error(
                StatusCode::BAD_GATEWAY,
                "WebRTC negotiation failed; try the TCP connection.",
            )
        }
    }
}

pub async fn remove(State(s): State<AppState>, Path(id): Path<String>) -> StatusCode {
    s.rtc.close(&id).await;
    StatusCode::NO_CONTENT
}

pub async fn list(State(s): State<AppState>) -> Json<serde_json::Value> {
    let peers=s.rtc.peers.lock().values().map(|p|serde_json::json!({"id":p.id,"state":p.pc.connection_state().to_string(),"readOnly":p.state.read_only})).collect::<Vec<_>>();
    Json(serde_json::json!({"peers":peers}))
}

async fn make_peer(
    state: AppState,
    servers: Vec<RTCIceServer>,
    slot: OwnedSemaphorePermit,
) -> webrtc::error::Result<Arc<Peer>> {
    let mut engine = MediaEngine::default();
    engine.register_default_codecs()?;
    let registry = register_default_interceptors(Registry::new(), &mut engine)?;
    let api = APIBuilder::new()
        .with_media_engine(engine)
        .with_interceptor_registry(registry)
        .build();
    let pc = Arc::new(
        api.new_peer_connection(RTCConfiguration {
            ice_servers: servers,
            ..Default::default()
        })
        .await?,
    );
    let video = Arc::new(TrackLocalStaticSample::new(
        RTCRtpCodecCapability {
            mime_type: MIME_TYPE_H264.into(),
            clock_rate: 90_000,
            sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                .into(),
            ..Default::default()
        },
        "desktop".into(),
        "bro-rmm".into(),
    ));
    let sender = pc
        .add_track(video.clone() as Arc<dyn TrackLocal + Send + Sync>)
        .await?;
    let (tx, rx) = mpsc::channel(8);
    let client = state.hub.add_bounded_client(tx);
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(|e| webrtc::Error::new(e.to_string()))?;
    let id: String = random.iter().map(|b| format!("{b:02x}")).collect();
    let (quit, _) = watch::channel(false);
    let peer = Arc::new(Peer {
        id,
        pc: pc.clone(),
        client: client.clone(),
        hub: state.hub.clone(),
        registry: Arc::downgrade(&state.rtc),
        channel: Mutex::new(None),
        state,
        rtts: Mutex::new(VecDeque::new()),
        quit,
        closed: AtomicBool::new(false),
        last_seen: AtomicU64::new(0),
        epoch: Instant::now(),
        _slot: slot,
    });
    let weak = Arc::downgrade(&peer);
    pc.on_peer_connection_state_change(Box::new(move |status| {
        let weak = weak.clone();
        Box::pin(async move {
            if matches!(
                status,
                RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed
            ) {
                close_weak(weak);
            }
        })
    }));
    let weak = Arc::downgrade(&peer);
    pc.on_data_channel(Box::new(move |channel| {
        let weak = weak.clone();
        Box::pin(async move {
            attach_channel(weak, channel).await;
        })
    }));
    let weak = Arc::downgrade(&peer);
    tokio::spawn(async move {
        while let Ok((packets, _)) = sender.read_rtcp().await {
            let Some(p) = weak.upgrade() else {
                break;
            };
            for packet in packets {
                if packet.as_any().is::<webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication>()
                    || packet.as_any().is::<webrtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest>() {
                    p.client.needs_keyframe.store(true,Ordering::Relaxed);p.hub.wake();
                }
                if let Some(rr) = packet
                    .as_any()
                    .downcast_ref::<webrtc::rtcp::receiver_report::ReceiverReport>()
                {
                    let loss = rr
                        .reports
                        .iter()
                        .map(|r| r.fraction_lost as u32)
                        .max()
                        .unwrap_or(0);
                    p.client.loss_fraction.store(loss, Ordering::Relaxed);
                }
            }
        }
    });
    tokio::spawn(write_media(Arc::downgrade(&peer), rx, video));
    Ok(peer)
}

fn close_weak(weak: Weak<Peer>) {
    tokio::spawn(async move {
        if let Some(p) = weak.upgrade() {
            if let Some(registry) = p.registry.upgrade() {
                registry.close(&p.id).await;
            }
        }
    });
}

async fn attach_channel(weak: Weak<Peer>, channel: Arc<RTCDataChannel>) {
    let Some(peer) = weak.upgrade() else {
        return;
    };
    if channel.label() != "control" || !channel.ordered() || peer.channel.lock().is_some() {
        let _ = channel.close().await;
        return;
    }
    *peer.channel.lock() = Some(channel.clone());
    let on_open = weak.clone();
    channel.on_open(Box::new(move || {
        Box::pin(async move {
            if let Some(p) = on_open.upgrade() {
                p.seen();
                p.client.send(Out::Text(p.hello()));
                if let Some(position) = p.hub.pointer.lock().clone() {
                    p.client.send(Out::Text(position));
                }
                if let Some(c) = p.hub.cursor.lock().clone() {
                    p.client.send(Out::Bin(c));
                }
                if let Some(c) = p.hub.last_clipboard.lock().clone() {
                    p.client
                        .send(Out::Text(serde_json::json!({"t":"clip","d":c}).to_string()));
                }
            }
        })
    }));
    let on_message = weak.clone();
    channel.on_message(Box::new(move |m: DataChannelMessage| {
        let weak = on_message.clone();
        Box::pin(async move {
            let Some(p) = weak.upgrade() else {
                return;
            };
            if p.closed.load(Ordering::Acquire) || !m.is_string || m.data.len() > 64 * 1024 {
                return;
            }
            let Ok(text) = std::str::from_utf8(&m.data) else {
                return;
            };
            p.seen();
            crate::server::handle_text(&p.state, &p.client, text, &mut p.rtts.lock());
        })
    }));
    channel.on_close(Box::new(move || {
        let weak = weak.clone();
        Box::pin(async move {
            close_weak(weak);
        })
    }));
}

async fn write_media(
    weak: Weak<Peer>,
    mut rx: mpsc::Receiver<Out>,
    video: Arc<TrackLocalStaticSample>,
) {
    let Some(p) = weak.upgrade() else {
        return;
    };
    let mut quit = p.quit.subscribe();
    drop(p);
    let mut last_frame = Instant::now();
    loop {
        let message = tokio::select! { m=rx.recv()=>m, _=quit.changed()=>None };
        let (Some(message), Some(peer)) = (message, weak.upgrade()) else {
            break;
        };
        let channel = peer.channel.lock().clone();
        let Some(channel) = channel else {
            continue;
        };
        if channel.ready_state()
            != webrtc::data_channel::data_channel_state::RTCDataChannelState::Open
        {
            continue;
        }
        if channel.buffered_amount().await > 1024 * 1024 {
            break;
        }
        let send = async {
            match message {
                Out::Bin(b) if b.len() > 16 && b[0] == 1 => {
                    if b[10] != 0 || b[12] != 1 {
                        return Err(webrtc::Error::new("RTC needs one video stripe".into()));
                    }
                    let frame = u32::from_be_bytes(b[2..6].try_into().unwrap());
                    let duration = last_frame
                        .elapsed()
                        .clamp(Duration::from_millis(1), Duration::from_secs(1));
                    last_frame = Instant::now();
                    video
                        .write_sample(&Sample {
                            data: b.slice(16..),
                            duration,
                            ..Default::default()
                        })
                        .await?;
                    // RTP transport has consumed this frame; RTCP receiver reports
                    // separately feed packet loss into the shared bitrate controller.
                    peer.client.on_ack(frame);
                }
                Out::Bin(b) if b.first() == Some(&3) && b.len() <= 60 * 1024 => {
                    channel.send(&b).await?;
                }
                Out::Text(text) => {
                    let text = if text.contains("\"t\":\"hello\"") {
                        peer.hello()
                    } else {
                        text
                    };
                    if text.len() <= 60 * 1024 {
                        channel.send_text(text).await?;
                    }
                }
                _ => {}
            }
            Ok::<_, webrtc::Error>(())
        };
        if !matches!(
            tokio::time::timeout(Duration::from_secs(3), send).await,
            Ok(Ok(()))
        ) {
            break;
        }
    }
    close_weak(weak);
}

fn supervise(weak: Weak<Peer>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tick.tick().await;
            let Some(peer) = weak.upgrade() else {
                return;
            };
            if peer.closed.load(Ordering::Acquire) {
                return;
            }
            if peer
                .epoch
                .elapsed()
                .as_secs()
                .saturating_sub(peer.last_seen.load(Ordering::Relaxed))
                > IDLE_SECS
            {
                close_weak(weak);
                return;
            }
            let sent = peer.state.epoch.elapsed().as_micros() as u64;
            peer.client
                .send(Out::Text(format!("{{\"t\":\"ping\",\"s\":{sent}}}")));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> (
        AppState,
        std::sync::mpsc::Receiver<crate::input::InputEvent>,
    ) {
        let hub = Hub::new();
        hub.width.store(1280, Ordering::Relaxed);
        hub.height.store(800, Ordering::Relaxed);
        *hub.stripes.lock() = vec![(0, 800)];
        let (tx, rx) = std::sync::mpsc::channel();
        *hub.input.lock() = Some(tx);
        (
            AppState {
                stopping: Arc::new(AtomicBool::new(false)),
                rtc: Peers::new(),
                hub: hub.clone(),
                audio: false,
                epoch: Instant::now(),
                files: Arc::new(crate::files::Files {
                    root: std::env::temp_dir(),
                    upload: false,
                    download: false,
                    default_dir: String::new(),
                    max_upload_bytes: 0,
                }),
                mic: None,
                resize: false,
                read_only: false,
                recorder: crate::record::Recorder::new(hub, Duration::from_secs(1)),
                index: bytes::Bytes::new(),
                favicon: bytes::Bytes::new(),
                favicon_type: "image/x-icon",
            },
            rx,
        )
    }

    // Exercise the permission boundary across real ICE/DTLS/SCTP, including
    // release-all and clipboard writes, rather than trusting a viewer flag.
    #[tokio::test]
    async fn readonly_sctp_refuses_input_and_tears_down() {
        let result = tokio::time::timeout(Duration::from_secs(20), async {
            let (state, input) = state();
            let mut engine = MediaEngine::default();
            engine.register_default_codecs().unwrap();
            let api = APIBuilder::new().with_media_engine(engine).build();
            let client = Arc::new(api.new_peer_connection(Default::default()).await.unwrap());
            use webrtc::rtp_transceiver::{
                rtp_codec::RTPCodecType, rtp_transceiver_direction::RTCRtpTransceiverDirection,
                RTCRtpTransceiverInit,
            };
            client
                .add_transceiver_from_kind(
                    RTPCodecType::Video,
                    Some(RTCRtpTransceiverInit {
                        direction: RTCRtpTransceiverDirection::Recvonly,
                        send_encodings: vec![],
                    }),
                )
                .await
                .unwrap();
            let control = client.create_data_channel("control", None).await.unwrap();
            let opened = control.clone();
            control.on_open(Box::new(move || {
                Box::pin(async move {
                    for command in [
                        "m,100,120",
                        "b,1,1",
                        "b,1,0",
                        "k,65,1",
                        "k,65,0",
                        "r",
                        "c,private",
                        "v,1",
                    ] {
                        opened.send_text(command).await.unwrap();
                    }
                })
            }));
            let local = client.create_offer(None).await.unwrap();
            let mut gathered = client.gathering_complete_promise().await;
            client.set_local_description(local).await.unwrap();
            let _ = gathered.recv().await;
            let response = offer(
                State(state.clone()),
                Json(Offer {
                    sdp: client.local_description().await.unwrap().sdp,
                    kind: "offer".into(),
                    ice_servers: vec![],
                    read_only: true,
                }),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
                .await
                .unwrap();
            let answer: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let id = answer["id"].as_str().unwrap();
            client
                .set_remote_description(
                    RTCSessionDescription::answer(answer["sdp"].as_str().unwrap().to_owned())
                        .unwrap(),
                )
                .await
                .unwrap();
            loop {
                if state.hub.video_clients().len() == 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert_eq!(
                input.try_iter().count(),
                0,
                "read-only peer injected desktop input"
            );
            assert_eq!(state.rtc.peers.lock().len(), 1);
            state.rtc.close(id).await;
            client.close().await.unwrap();
            assert!(state.hub.clients.lock().is_empty());
            assert!(state.rtc.peers.lock().is_empty());
            assert_eq!(
                input.try_iter().count(),
                0,
                "spectator disconnect released controller keys"
            );
        })
        .await;
        assert!(result.is_ok(), "loopback SCTP connection timed out");
    }

    #[test]
    fn only_stun_and_authenticated_turn_are_valid() {
        let parse = |s: &str| serde_json::from_str::<Vec<IceServer>>(s).unwrap();
        assert!(ice_servers(parse(r#"[{"urls":"stun:example.com:3478"}]"#)).is_ok());
        assert!(ice_servers(parse(r#"[{"urls":["turn:example.com:3478?transport=udp","turn:example.com:3478?transport=tcp"],"username":"u","credential":"p"}]"#)).is_ok());
        assert!(ice_servers(parse(r#"[{"urls":"https://example.com"}]"#)).is_err());
        assert!(ice_servers(parse(r#"[{"urls":"turn:example.com"}]"#)).is_err());
    }
}
