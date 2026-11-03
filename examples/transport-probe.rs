//! Live transport acceptance: run a separately started authenticated Windows host.
use futures_util::{SinkExt, StreamExt};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::policy::ice_transport_policy::RTCIceTransportPolicy;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_codec::RTPCodecType;
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::rtp_transceiver::RTCRtpTransceiverInit;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[tokio::main]
async fn main() -> Result<()> {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
        .try_init();
    let base = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "http://127.0.0.1:6901".into());
    let password = std::env::var("VOXL_PASSWORD")?;
    let http = reqwest::Client::new();
    let unauthorized = http.get(format!("{base}/stats")).send().await?;
    assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);
    let ws_base = base.replacen("http", "ws", 1);
    let mut request =
        tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(format!(
            "{ws_base}/ws?readOnly=true"
        ))?;
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {password}").parse()?);
    let (mut ws, _) = tokio_tungstenite::connect_async(request).await?;
    ws.send(tokio_tungstenite::tungstenite::Message::Text("v,1".into()))
        .await?;
    let tcp_video = tokio::time::timeout(Duration::from_secs(20), async {
        while let Some(message) = ws.next().await {
            match message? {
                tokio_tungstenite::tungstenite::Message::Binary(b) if b.len() > 16 && b[0] == 1 => {
                    assert_eq!(b[10], 0);
                    assert_eq!(b[12], 1);
                    assert!(
                        (b[1] & 1) == 1,
                        "first picture must be independently decodable"
                    );
                    assert!(b[16..].windows(3).any(|w| w == [0, 0, 1]));
                    return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(b.len());
                }
                _ => {}
            }
        }
        Err("TCP closed before video".into())
    })
    .await??;
    ws.close(None).await?;
    println!("TCP: authenticated H264 keyframe received ({tcp_video} bytes)");

    let mut engine = MediaEngine::default();
    engine.register_default_codecs()?;
    let api = APIBuilder::new().with_media_engine(engine).build();
    let servers: Vec<RTCIceServer> = std::env::var("BRO_RMM_ICE_SERVERS")
        .ok()
        .map(|s| serde_json::from_str(&s))
        .transpose()?
        .unwrap_or_default();
    let relay = std::env::var_os("BRO_RMM_FORCE_RELAY").is_some();
    let pc = Arc::new(
        api.new_peer_connection(RTCConfiguration {
            ice_servers: servers.clone(),
            ice_transport_policy: if relay {
                RTCIceTransportPolicy::Relay
            } else {
                RTCIceTransportPolicy::All
            },
            ..Default::default()
        })
        .await?,
    );
    pc.add_transceiver_from_kind(
        RTPCodecType::Video,
        Some(RTCRtpTransceiverInit {
            direction: RTCRtpTransceiverDirection::Recvonly,
            send_encodings: vec![],
        }),
    )
    .await?;
    let got_hello = Arc::new(AtomicBool::new(false));
    let rtp_packets = Arc::new(AtomicUsize::new(0));
    let got_keyframe = Arc::new(AtomicBool::new(false));
    let channel = pc.create_data_channel("control", None).await?;
    let opened = channel.clone();
    channel.on_open(Box::new(move || {
        let opened = opened.clone();
        Box::pin(async move {
            let _ = opened.send_text("v,1").await;
        })
    }));
    let hello = got_hello.clone();
    let replies = channel.clone();
    channel.on_message(Box::new(move |m: DataChannelMessage| {
        let hello = hello.clone();
        let replies = replies.clone();
        Box::pin(async move {
            if m.is_string {
                if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&m.data) {
                    if value["t"] == "hello" {
                        assert!(value["w"].as_u64().unwrap() > 0);
                        assert_eq!(value["readOnly"], true);
                        hello.store(true, Ordering::Release);
                    }
                    if value["t"] == "ping" {
                        let _ = replies.send_text(format!("po,{}", value["s"])).await;
                    }
                }
            }
        })
    }));
    let packets = rtp_packets.clone();
    let keyframe = got_keyframe.clone();
    pc.on_track(Box::new(move |track, _, _| {
        let packets = packets.clone();
        let keyframe = keyframe.clone();
        Box::pin(async move {
            while let Ok((packet, _)) = track.read_rtp().await {
                packets.fetch_add(1, Ordering::Relaxed);
                let p = &packet.payload;
                if !p.is_empty()
                    && ((p[0] & 31) == 5 || ((p[0] & 31) == 28 && p.len() > 1 && (p[1] & 31) == 5))
                {
                    keyframe.store(true, Ordering::Release);
                }
            }
        })
    }));
    let offer = pc.create_offer(None).await?;
    let mut gathered = pc.gathering_complete_promise().await;
    pc.set_local_description(offer).await?;
    tokio::time::timeout(Duration::from_secs(15), gathered.recv()).await?;
    let offer = pc.local_description().await.ok_or("No local offer")?;
    let candidates = |sdp: &str| {
        let mut counts = std::collections::BTreeMap::new();
        for line in sdp.lines().filter(|line| line.starts_with("a=candidate:")) {
            let words: Vec<_> = line.split_whitespace().collect();
            if let Some(i) = words.iter().position(|w| *w == "typ") {
                if let Some(kind) = words.get(i + 1) {
                    *counts.entry((*kind).to_owned()).or_insert(0usize) += 1;
                }
            }
        }
        counts
    };
    println!("Local ICE candidates: {:?}", candidates(&offer.sdp));
    let response=http.post(format!("{base}/rtc/offer")).bearer_auth(&password).json(&serde_json::json!({"type":"offer","sdp":offer.sdp,"iceServers":servers,"readOnly":true})).send().await?;
    let status = response.status();
    let answer: serde_json::Value = response.json().await?;
    if !status.is_success() {
        return Err(format!("RTC offer failed: {answer}").into());
    }
    let id = answer["id"].as_str().ok_or("Missing peer id")?;
    println!(
        "Remote ICE candidates: {:?}",
        candidates(answer["sdp"].as_str().ok_or("Missing answer")?)
    );
    pc.set_remote_description(RTCSessionDescription::answer(
        answer["sdp"].as_str().ok_or("Missing answer")?.to_owned(),
    )?)
    .await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !(got_hello.load(Ordering::Acquire) && got_keyframe.load(Ordering::Acquire)) {
        if tokio::time::Instant::now() > deadline {
            return Err(format!(
                "RTC timed out: state={} ice={} hello={} rtp={} keyframe={}",
                pc.connection_state(),
                pc.ice_connection_state(),
                got_hello.load(Ordering::Acquire),
                rtp_packets.load(Ordering::Relaxed),
                got_keyframe.load(Ordering::Acquire)
            )
            .into());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    println!(
        "WebRTC: authenticated read-only hello + H264 IDR received ({} RTP packets, relay={relay})",
        rtp_packets.load(Ordering::Relaxed)
    );
    let removed = http
        .delete(format!("{base}/rtc/{id}"))
        .bearer_auth(&password)
        .send()
        .await?;
    assert_eq!(removed.status(), reqwest::StatusCode::NO_CONTENT);
    pc.close().await?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let stats: serde_json::Value = http
        .get(format!("{base}/stats"))
        .bearer_auth(&password)
        .send()
        .await?
        .json()
        .await?;
    assert!(
        stats["clients"].as_array().unwrap().is_empty(),
        "viewer leaked after teardown: {stats}"
    );
    println!("Teardown: no TCP or RTC clients remain");
    let encoded = stats["encoded"].as_u64().unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    let after: serde_json::Value = http
        .get(format!("{base}/stats"))
        .bearer_auth(&password)
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(
        after["encoded"].as_u64().unwrap(),
        encoded,
        "encoder active with no viewers"
    );
    println!("Idle: encoder sleeps without viewers");
    Ok(())
}
