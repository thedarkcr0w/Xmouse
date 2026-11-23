//! The remote play connection: WebRTC to the console, data channel protocol, input loop,
//! video frame acknowledgements and (in PC Play) video decoding.

use std::collections::VecDeque;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use serde_json::{json, Value};
use tokio::sync::{watch, Notify};
use webrtc::api::interceptor_registry::{configure_nack, configure_rtcp_reports};
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_H264, MIME_TYPE_OPUS};
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice::network_type::NetworkType;
use webrtc::ice::udp_mux::{UDPMux, UDPMuxDefault, UDPMuxParams};
use webrtc::ice::udp_network::UDPNetwork;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTCRtpHeaderExtensionCapability, RTPCodecType,
};
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::rtp_transceiver::{RTCPFeedback, RTCRtpTransceiverInit};
use webrtc::track::track_remote::TrackRemote;

use crate::auth::Auth;
use crate::mapper::{Mapper, PadFrame};
use crate::packet::{self, FrameMeta};
use crate::storage::PlayMode;
use crate::xhome::{describe_failure, IceCandidate, XHome};

#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    Starting,
    WaitingForConsole,
    Connecting,
    Connected,
    Ended,
}

#[derive(Clone, Debug)]
pub struct Status {
    pub phase: Phase,
    pub detail: String,
    pub error: Option<String>,
}

/// A decoded PC Play video frame.
pub enum VideoFrame {
    /// Already in egui's pixel format (built on the decoder thread, so the UI only uploads it).
    Rgba(egui::ColorImage),
    /// Raw NV12 (Y plane, then interleaved UV) for the GPU path (RTX Video Super Resolution).
    Nv12 {
        width: usize,
        height: usize,
        data: Vec<u8>,
    },
}

/// A decoded frame and when it came out of the decoder.
pub struct TimedFrame {
    pub at: Instant,
    pub frame: VideoFrame,
}

/// How many decoded frames may wait for the UI. The UI normally takes each one within a
/// millisecond or two; this only fills when it stalls, and then the oldest are dropped.
pub const VIDEO_QUEUE: usize = 3;

/// Decoded frames on their way to the UI, oldest first.
///
/// This used to be a one-slot mailbox, which silently lost a frame whenever the UI was a
/// little late. For telling repeated frames from new ones (and for in-between frames) every
/// frame has to be seen, with its arrival time, so a few are kept. Latency is unchanged: the
/// UI still shows the newest.
#[derive(Default)]
pub struct VideoQueue(Mutex<VecDeque<TimedFrame>>);

impl VideoQueue {
    pub fn push(&self, frame: VideoFrame) {
        let mut q = self.0.lock().unwrap();
        while q.len() >= VIDEO_QUEUE {
            q.pop_front();
        }
        q.push_back(TimedFrame {
            at: Instant::now(),
            frame,
        });
    }

    /// Everything that is waiting, oldest first.
    pub fn drain(&self) -> Vec<TimedFrame> {
        self.0.lock().unwrap().drain(..).collect()
    }

    /// Look at the newest waiting frame without taking it.
    pub fn with_newest<R>(&self, look: impl FnOnce(&VideoFrame) -> R) -> Option<R> {
        self.0.lock().unwrap().back().map(|f| look(&f.frame))
    }

    pub fn is_empty(&self) -> bool {
        self.0.lock().unwrap().is_empty()
    }

    /// Size of the newest waiting frame.
    pub fn newest_size(&self) -> Option<(usize, usize)> {
        self.with_newest(|f| match f {
            VideoFrame::Rgba(img) => (img.size[0], img.size[1]),
            VideoFrame::Nv12 { width, height, .. } => (*width, *height),
        })
    }
}

/// State shared between the UI thread and the connection tasks.
pub struct Shared {
    pub mapper: Mutex<Mapper>,
    /// Deadzone finder: when set, a constant right-stick push of this size is sent instead.
    pub finder: Mutex<Option<f32>>,
    pub status: Mutex<Status>,
    pub frames_sent: AtomicU64,
    pub video_frames: AtomicU64,
    /// Video RTP packets received (before decoding).
    pub video_packets: AtomicU64,
    /// The UI's GPU upscaler is running: hand frames over as NV12 instead of RGBA.
    pub want_nv12: AtomicBool,
    /// Decode the video on the graphics card (set before the stream starts).
    pub gpu_decode: AtomicBool,
    pub video: VideoQueue,
    acks: Mutex<VecDeque<FrameMeta>>,
    keyframe_wanted: AtomicBool,
    stop_tx: watch::Sender<bool>,
    /// The one clock for the mapper: key presses (UI thread) and ticks (input loop) must use
    /// the same time base, or tap / wheel presses are held for as long as the clocks differ.
    epoch: Instant,
}

impl Shared {
    pub fn new(mapper: Mapper) -> Arc<Self> {
        let (stop_tx, _) = watch::channel(false);
        Arc::new(Self {
            epoch: Instant::now(),
            mapper: Mutex::new(mapper),
            finder: Mutex::new(None),
            status: Mutex::new(Status {
                phase: Phase::Starting,
                detail: "Starting remote play…".into(),
                error: None,
            }),
            frames_sent: AtomicU64::new(0),
            video_frames: AtomicU64::new(0),
            video_packets: AtomicU64::new(0),
            want_nv12: AtomicBool::new(false),
            gpu_decode: AtomicBool::new(false),
            video: VideoQueue::default(),
            acks: Mutex::new(VecDeque::new()),
            keyframe_wanted: AtomicBool::new(false),
            stop_tx,
        })
    }

    pub fn stop(&self) {
        let _ = self.stop_tx.send(true);
    }

    pub fn stopped(&self) -> bool {
        *self.stop_tx.borrow()
    }

    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }

    pub(crate) fn set(&self, phase: Phase, detail: &str) {
        let mut s = self.status.lock().unwrap();
        s.phase = phase;
        s.detail = detail.to_string();
    }

    pub(crate) fn fail(&self, err: String) {
        let mut s = self.status.lock().unwrap();
        s.phase = Phase::Ended;
        if s.error.is_none() {
            s.error = Some(err);
        }
    }

    /// True once after the decoder asked for a full picture (see `request_keyframe`): the
    /// session loop passes the request on to whatever is sending the video.
    pub(crate) fn take_keyframe_request(&self) -> bool {
        self.keyframe_wanted.swap(false, Ordering::SeqCst)
    }

    /// Changes when `stop()` is called, for session loops that wait on it.
    pub(crate) fn stop_signal(&self) -> watch::Receiver<bool> {
        self.stop_tx.subscribe()
    }

    /// Milliseconds on the mapper's clock (see `epoch`).
    pub fn now_ms(&self) -> f64 {
        self.epoch.elapsed().as_secs_f64() * 1000.0
    }

    /// One controller state for the input loop: ticks the mapper (or the deadzone finder).
    pub(crate) fn frame(&self) -> PadFrame {
        let finder = *self.finder.lock().unwrap();
        let mut mapper = self.mapper.lock().unwrap();
        let f = mapper.tick(self.now_ms());
        if let Some(v) = finder {
            let f = PadFrame {
                rx: v,
                ..Default::default()
            };
            mapper.last_frame = f;
            return f;
        }
        f
    }
}

#[derive(Clone, Debug)]
pub struct StreamConfig {
    pub mode: PlayMode,
    /// Video bitrate in kbps (0 = let the console decide).
    pub video_bitrate: u32,
    pub high_quality: bool,
    pub input_hz: u32,
    pub ice_servers: Vec<String>,
    pub connect_timeout: Duration,
    /// The console's name in the xHome list (to recognise it in LAN discovery).
    pub console_name: String,
    /// The console's LAN IPv4 address typed in by the user; empty = find it automatically.
    pub console_ip: String,
    /// The console's LAN address, once known: offered as an extra ICE candidate so the
    /// connection can go straight across the home network.
    pub lan_ip: Option<std::net::Ipv4Addr>,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            mode: PlayMode::Tv,
            video_bitrate: 1000,
            high_quality: false,
            input_hz: 60,
            ice_servers: vec![
                "stun:stun.l.google.com:19302".into(),
                "stun:stun1.l.google.com:19302".into(),
            ],
            connect_timeout: Duration::from_secs(45),
            console_name: String::new(),
            console_ip: String::new(),
            lan_ip: None,
        }
    }
}

/// How SDP/ICE get to the console (the xHome service, or a fake console in tests).
pub trait Signaling: Send + Sync + 'static {
    fn exchange_sdp(&self, offer: String) -> impl Future<Output = Result<String>> + Send;
    fn exchange_ice(
        &self,
        local: Vec<IceCandidate>,
    ) -> impl Future<Output = Result<Vec<IceCandidate>>> + Send;
    fn keepalive(&self) -> impl Future<Output = Result<()>> + Send;
}

pub struct XHomeSignaling {
    pub api: XHome,
    pub session_id: String,
}

impl Signaling for XHomeSignaling {
    async fn exchange_sdp(&self, offer: String) -> Result<String> {
        self.api.exchange_sdp(&self.session_id, &offer).await
    }
    async fn exchange_ice(&self, local: Vec<IceCandidate>) -> Result<Vec<IceCandidate>> {
        self.api.exchange_ice(&self.session_id, &local).await
    }
    async fn keepalive(&self) -> Result<()> {
        self.api.keepalive(&self.session_id).await
    }
}

pub type Repaint = Arc<dyn Fn() + Send + Sync>;

/// Full session: ask the service to start remote play, wait for the console, connect, play.
pub async fn run_session(
    auth: Arc<Auth>,
    console_id: String,
    cfg: StreamConfig,
    shared: Arc<Shared>,
    repaint: Repaint,
) {
    let result = run_session_inner(&auth, &console_id, cfg, &shared, &repaint).await;
    if let Err(e) = result {
        if !shared.stopped() {
            shared.fail(format!("{e:#}"));
        }
    }
    shared.set(Phase::Ended, "Session ended");
    repaint();
}

async fn run_session_inner(
    auth: &Arc<Auth>,
    console_id: &str,
    mut cfg: StreamConfig,
    shared: &Arc<Shared>,
    repaint: &Repaint,
) -> Result<()> {
    // Look for the console on the home network while the service wakes it up.
    let manual_ip = cfg.console_ip.trim().parse::<std::net::Ipv4Addr>().ok();
    if !cfg.console_ip.trim().is_empty() && manual_ip.is_none() {
        log::warn!("ignoring console IP setting {:?}: not an IPv4 address", cfg.console_ip);
    }
    let discovery = if manual_ip.is_none() {
        Some(tokio::task::spawn_blocking(|| {
            crate::xbox_lan::discover(Duration::from_secs(3))
        }))
    } else {
        None
    };
    shared.set(
        Phase::Starting,
        "Asking Xbox to start a remote play session…",
    );
    repaint();
    let token = auth.stream_token().await?;
    let api = XHome::new(&token.host, &token.token);
    let id = api
        .start(console_id, "en-US")
        .await
        .context("Could not start remote play")?;

    shared.set(
        Phase::WaitingForConsole,
        "Waiting for your Xbox… (up to a minute if it was asleep)",
    );
    repaint();
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut connected_sent = false;
    loop {
        if shared.stopped() {
            let _ = api.stop(&id).await;
            return Ok(());
        }
        if Instant::now() > deadline {
            let _ = api.stop(&id).await;
            bail!("Your Xbox didn't respond in time.");
        }
        let st = api.state(&id).await?;
        match st["state"].as_str().unwrap_or("") {
            "Provisioned" => break,
            "ReadyToConnect" if !connected_sent => {
                let t = auth.console_transfer_token().await?;
                api.connect(&id, &t).await?;
                connected_sent = true;
            }
            "Failed" => bail!("{}", describe_failure(&st["errorDetails"])),
            _ => {}
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    cfg.lan_ip = match (manual_ip, discovery) {
        (Some(ip), _) => {
            log::info!("console LAN address from settings: {ip}");
            Some(ip)
        }
        (None, Some(task)) => {
            let found = task.await.unwrap_or_default();
            let ip = crate::xbox_lan::pick(&found, &cfg.console_name);
            match ip {
                Some(ip) => log::info!("console LAN address found: {ip}"),
                None => log::info!(
                    "console not found on the LAN ({} answered); relying on the service's candidates",
                    found.len()
                ),
            }
            ip
        }
        (None, None) => None,
    };

    shared.set(Phase::Connecting, "Connecting over your home network…");
    repaint();
    let sig = Arc::new(XHomeSignaling {
        api: api.clone(),
        session_id: id.clone(),
    });
    let result = connect(sig, cfg, shared.clone(), repaint.clone()).await;
    let _ = api.stop(&id).await;
    result
}

/// UI demo (XMOUSE_DEMO=1): walk through the connection steps without a console.
pub async fn run_demo_session(shared: Arc<Shared>, repaint: Repaint) {
    for (phase, detail) in [
        (
            Phase::Starting,
            "Asking Xbox to start a remote play session…",
        ),
        (
            Phase::WaitingForConsole,
            "Waiting for your Xbox… (up to a minute if it was asleep)",
        ),
        (Phase::Connecting, "Connecting over your home network…"),
    ] {
        shared.set(phase, detail);
        repaint();
        tokio::time::sleep(Duration::from_millis(1500)).await;
        if shared.stopped() {
            return;
        }
    }
    shared.set(Phase::Connected, "Connected");
    repaint();
    // XMOUSE_TEST_VIDEO=30 (or 60): a moving test picture instead of a console, to check the
    // picture settings by eye.
    if let Some(fps) = std::env::var("XMOUSE_TEST_VIDEO")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
    {
        crate::testvideo::spawn(shared.clone(), repaint.clone(), fps);
    }
    while !shared.stopped() {
        tokio::time::sleep(Duration::from_millis(16)).await;
        shared.frame();
        shared.frames_sent.fetch_add(1, Ordering::Relaxed);
    }
}

fn video_feedback() -> Vec<RTCPFeedback> {
    // nack, nack pli and transport-cc are added by the default interceptors (listing them here
    // too duplicated every line in the offer).
    [("goog-remb", ""), ("ccm", "fir")]
        .iter()
        .map(|(t, p)| RTCPFeedback {
            typ: t.to_string(),
            parameter: p.to_string(),
        })
        .collect()
}

/// NACK + RTCP receiver reports, but deliberately *no* transport-wide congestion control
/// (transport-cc): with it negotiated the console sent audio but never started video, which is
/// how a libwebrtc sender behaves when it gets no usable TWCC feedback (video is paced and
/// paused, audio is not). Without it the console uses our REMB estimate instead.
fn interceptors(m: &mut MediaEngine) -> Registry {
    let registry = configure_nack(Registry::new(), m);
    configure_rtcp_reports(registry)
}

fn media_engine(cfg: &StreamConfig) -> Result<MediaEngine> {
    let mut m = MediaEngine::default();
    m.register_codec(
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.into(),
                clock_rate: 48000,
                channels: 2,
                sdp_fmtp_line: "minptime=10;useinbandfec=1;stereo=1".into(),
                rtcp_feedback: vec![],
            },
            payload_type: 111,
            ..Default::default()
        },
        RTPCodecType::Audio,
    )?;
    // TV Play only needs a tiny stream: ask for Baseline. PC Play prefers Main/High for quality.
    let profiles: &[(u8, &str)] = if cfg.mode == PlayMode::Pc && cfg.high_quality {
        &[(102, "4d001f"), (104, "640032"), (106, "42e01f")]
    } else {
        &[(106, "42e01f"), (108, "42001f")]
    };
    for (pt, profile) in profiles {
        m.register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_H264.into(),
                    clock_rate: 90000,
                    channels: 0,
                    sdp_fmtp_line: format!(
                        "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id={profile}"
                    ),
                    rtcp_feedback: video_feedback(),
                },
                payload_type: *pt,
                ..Default::default()
            },
            RTPCodecType::Video,
        )?;
        // No RTX on purpose: webrtc-rs 0.17 reads the RTX repair stream and throws it away
        // (RTPReceiver::receive_for_rtx), so every retransmission would be lost. Without RTX the
        // console resends lost packets on the main stream, which the NACK path does handle.
    }
    // Header extensions in the same set Chromium offers.
    let ext = |uri: &str| RTCRtpHeaderExtensionCapability { uri: uri.into() };
    for uri in [
        "urn:ietf:params:rtp-hdrext:ssrc-audio-level",
        "http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time",
        "urn:ietf:params:rtp-hdrext:sdes:mid",
    ] {
        m.register_header_extension(ext(uri), RTPCodecType::Audio, None)?;
    }
    for uri in [
        "urn:ietf:params:rtp-hdrext:toffset",
        "http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time",
        "urn:3gpp:video-orientation",
        "http://www.webrtc.org/experiments/rtp-hdrext/playout-delay",
        "http://www.webrtc.org/experiments/rtp-hdrext/video-content-type",
        "http://www.webrtc.org/experiments/rtp-hdrext/video-timing",
        "http://www.webrtc.org/experiments/rtp-hdrext/color-space",
        "urn:ietf:params:rtp-hdrext:sdes:mid",
        "urn:ietf:params:rtp-hdrext:sdes:rtp-stream-id",
        "urn:ietf:params:rtp-hdrext:sdes:repaired-rtp-stream-id",
    ] {
        m.register_header_extension(ext(uri), RTPCodecType::Video, None)?;
    }
    Ok(m)
}

/// Ask for at most `w`×`h` at 60 fps (RFC 6236 `imageattr`, the same form the console answers
/// with). Without it the console streams its own output size (2560×1440 here), which software
/// decoding can't keep up with at 60 fps.
pub fn limit_resolution(sdp: &str, w: u32, h: u32) -> String {
    let mut out = Vec::new();
    let mut in_video = false;
    for line in sdp.split("\r\n") {
        if line.starts_with("m=") {
            in_video = line.starts_with("m=video");
        }
        out.push(line.to_string());
        if in_video {
            if let Some(rest) = line.strip_prefix("a=rtpmap:") {
                if let Some((pt, codec)) = rest.split_once(' ') {
                    if codec.starts_with("H264/") {
                        out.push(format!(
                            "a=imageattr:{pt} recv [x=[0:{w}],y=[0:{h}],fps=[0:60]]"
                        ));
                    }
                }
            }
        }
    }
    out.join("\r\n")
}

/// webrtc-rs writes some lines with a trailing space (`a=rtcp-fb:102 nack `); browsers don't.
pub fn tidy_sdp(sdp: &str) -> String {
    sdp.split("\r\n")
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\r\n")
}

/// The SSRC the console declares for its video stream (`a=ssrc:` in the answer's video section).
pub fn answer_video_ssrc(sdp: &str) -> Option<u32> {
    let mut in_video = false;
    for line in sdp.lines().map(str::trim) {
        if line.starts_with("m=") {
            in_video = line.starts_with("m=video");
        } else if in_video {
            if let Some(rest) = line.strip_prefix("a=ssrc:") {
                if let Some(n) = rest.split_whitespace().next().and_then(|s| s.parse().ok()) {
                    return Some(n);
                }
            }
        }
    }
    None
}

/// Add `b=AS:<kbps>` to the video section (the browser client does the same).
pub fn set_video_bitrate(sdp: &str, kbps: u32) -> String {
    if kbps == 0 {
        return sdp.to_string();
    }
    let mut out = Vec::new();
    let mut in_video = false;
    let mut done = false;
    for line in sdp.split("\r\n") {
        if line.starts_with("m=") {
            in_video = line.starts_with("m=video");
        }
        if in_video && !done && line.starts_with("b=AS:") {
            continue;
        }
        out.push(line.to_string());
        if in_video && !done && line.starts_with("c=") {
            out.push(format!("b=AS:{kbps}"));
            done = true;
        }
    }
    out.join("\r\n")
}

/// Local ICE candidates from our SDP, in the browser JSON format the service expects.
pub fn local_candidates(sdp: &str) -> Vec<IceCandidate> {
    let mid = sdp
        .lines()
        .find_map(|l| l.trim().strip_prefix("a=mid:"))
        .unwrap_or("0")
        .to_string();
    sdp.lines()
        .filter_map(|l| l.trim().strip_prefix("a="))
        .filter(|l| l.starts_with("candidate:"))
        .map(|c| IceCandidate {
            candidate: c.to_string(),
            sdp_mid: Some(json!(mid)),
            sdp_mline_index: Some(json!(0)),
        })
        .collect()
}

fn remote_candidate(c: &IceCandidate) -> Option<RTCIceCandidateInit> {
    let s = c.candidate.trim();
    let s = s.strip_prefix("a=").unwrap_or(s).trim();
    if s.is_empty() || s.starts_with("end-of-candidates") {
        return None;
    }
    let as_string = |v: &Option<Value>| {
        v.as_ref().map(|v| {
            v.as_str()
                .map(|s| s.to_string())
                .unwrap_or_else(|| v.to_string())
        })
    };
    Some(RTCIceCandidateInit {
        candidate: s.to_string(),
        sdp_mid: as_string(&c.sdp_mid),
        sdp_mline_index: as_string(&c.sdp_mline_index).and_then(|s| s.parse().ok()),
        username_fragment: None,
    })
}

async fn send_json(dc: &RTCDataChannel, v: Value) -> Result<()> {
    // The console expects these as binary messages containing UTF-8 JSON.
    dc.send(&Bytes::from(v.to_string().into_bytes())).await?;
    Ok(())
}

fn message(target: &str, content: Value) -> Value {
    json!({
        "type": "Message",
        "content": content.to_string(),
        "id": uuid::Uuid::new_v4().to_string(),
        "target": target,
        "cv": "",
    })
}

/// Connect to the console and run until stopped or disconnected.
pub async fn connect<S: Signaling>(
    sig: Arc<S>,
    cfg: StreamConfig,
    shared: Arc<Shared>,
    repaint: Repaint,
) -> Result<()> {
    crate::init_crypto();
    let mut m = media_engine(&cfg)?;
    let registry = interceptors(&mut m);
    // The console sends full-size video datagrams (~1470 bytes). webrtc-rs reads with a 1460-byte
    // buffer by default and silently drops anything larger, so no video ever arrived while the
    // small audio packets did. Read with a buffer big enough for any UDP packet on the LAN.
    let mut settings = webrtc::api::setting_engine::SettingEngine::default();
    settings.set_receive_mtu(8192);
    // One UDP socket of our own for all ICE traffic (see `ice_socket`), IPv4 only.
    let mux = UDPMuxDefault::new(UDPMuxParams::new(tokio::net::UdpSocket::from_std(
        ice_socket()?,
    )?));
    settings.set_udp_network(UDPNetwork::Muxed(mux.clone()));
    settings.set_network_types(vec![NetworkType::Udp4]);
    let api = APIBuilder::new()
        .with_media_engine(m)
        .with_interceptor_registry(registry)
        .with_setting_engine(settings)
        .build();
    let pc = Arc::new(
        api.new_peer_connection(RTCConfiguration {
            ice_servers: if cfg.ice_servers.is_empty() {
                vec![]
            } else {
                vec![RTCIceServer {
                    urls: cfg.ice_servers.clone(),
                    ..Default::default()
                }]
            },
            ..Default::default()
        })
        .await?,
    );
    let result = run_connection(&pc, sig, &cfg, &shared, &repaint).await;
    let _ = pc.close().await;
    let _ = mux.close().await;
    result
}

/// The UDP socket ICE runs on.
///
/// webrtc-rs normally opens one socket per network interface and stops reading a socket at
/// its first receive error. On Windows a datagram answered with ICMP "port unreachable" makes
/// the *next* receive on that socket fail with WSAECONNRESET, so a single connectivity check
/// to a dead address (e.g. the Teredo-derived `<public ip>:3074` candidate) silently killed
/// the socket: the console's answers and checks were never read again and the connection
/// failed after 30 s although the LAN path had already worked. Here the socket is ours, with
/// that behaviour switched off.
fn ice_socket() -> std::io::Result<std::net::UdpSocket> {
    let socket = std::net::UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, 0))?;
    socket.set_nonblocking(true)?;
    #[cfg(windows)]
    disable_udp_connreset(&socket);
    log::info!("ICE socket: {}", socket.local_addr()?);
    Ok(socket)
}

#[cfg(windows)]
fn disable_udp_connreset(socket: &std::net::UdpSocket) {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{WSAIoctl, SOCKET};
    // _WSAIOW(IOC_VENDOR, 12)
    const SIO_UDP_CONNRESET: u32 = 0x9800_000C;
    let off: u32 = 0;
    let mut returned: u32 = 0;
    // SAFETY: a valid socket handle, a 4-byte input buffer, no output buffer, no overlapped I/O.
    let rc = unsafe {
        WSAIoctl(
            socket.as_raw_socket() as SOCKET,
            SIO_UDP_CONNRESET,
            &off as *const u32 as *const core::ffi::c_void,
            std::mem::size_of::<u32>() as u32,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
            None,
        )
    };
    if rc != 0 {
        log::warn!(
            "could not turn off UDP connection resets on the ICE socket: {}",
            std::io::Error::last_os_error()
        );
    }
}

async fn run_connection<S: Signaling>(
    pc: &Arc<RTCPeerConnection>,
    sig: Arc<S>,
    cfg: &StreamConfig,
    shared: &Arc<Shared>,
    repaint: &Repaint,
) -> Result<()> {
    let dc_init = |protocol: &str, ordered: Option<bool>| RTCDataChannelInit {
        ordered,
        protocol: Some(protocol.into()),
        ..Default::default()
    };
    let input = pc
        .create_data_channel("input", Some(dc_init("1.0", Some(true))))
        .await?;
    let _chat = pc
        .create_data_channel("chat", Some(dc_init("chatV1", None)))
        .await?;
    let control = pc
        .create_data_channel("control", Some(dc_init("controlV1", None)))
        .await?;
    let msg = pc
        .create_data_channel("message", Some(dc_init("messageV1", None)))
        .await?;

    pc.add_transceiver_from_kind(
        RTPCodecType::Audio,
        Some(RTCRtpTransceiverInit {
            // No microphone: receive only (same as the browser-based clients).
            direction: RTCRtpTransceiverDirection::Recvonly,
            send_encodings: vec![],
        }),
    )
    .await?;
    pc.add_transceiver_from_kind(
        RTPCodecType::Video,
        Some(RTCRtpTransceiverInit {
            direction: RTCRtpTransceiverDirection::Recvonly,
            send_encodings: vec![],
        }),
    )
    .await?;

    let start = Instant::now();
    let handshake_done = Arc::new(Notify::new());
    let failed = Arc::new(Notify::new());

    // message channel: handshake
    {
        let msg2 = msg.clone();
        msg.on_open(Box::new(move || {
            let msg2 = msg2.clone();
            Box::pin(async move {
                let hs = json!({ "type": "Handshake", "version": "messageV1", "id": uuid::Uuid::new_v4().to_string(), "cv": "" });
                if let Err(e) = send_json(&msg2, hs).await {
                    log::warn!("handshake send failed: {e}");
                }
            })
        }));
        let done = handshake_done.clone();
        msg.on_message(Box::new(move |m| {
            let done = done.clone();
            Box::pin(async move {
                if let Ok(v) = serde_json::from_slice::<Value>(&m.data) {
                    log::debug!("message channel: {v}");
                    if v["type"] == "HandshakeAck" {
                        done.notify_one();
                    }
                }
            })
        }));
    }
    control.on_message(Box::new(|m| {
        Box::pin(
            async move { log::debug!("control channel: {}", String::from_utf8_lossy(&m.data)) },
        )
    }));
    // Vibration / server metadata. Logged (kind + size) so a session can be diagnosed.
    input.on_message(Box::new(|m| {
        Box::pin(async move {
            let b = &m.data;
            let kind = if b.len() >= 2 {
                u16::from_le_bytes([b[0], b[1]])
            } else {
                0
            };
            if kind == 16 && b.len() >= 10 {
                let h = u32::from_le_bytes([b[2], b[3], b[4], b[5]]);
                let w = u32::from_le_bytes([b[6], b[7], b[8], b[9]]);
                log::info!("input channel: server metadata, video size {w}x{h}");
            } else {
                log::debug!("input channel: kind={kind} bytes={}", b.len());
            }
        })
    }));
    for (name, dc) in [
        ("input", &input),
        ("control", &control),
        ("message", &msg),
        ("chat", &_chat),
    ] {
        dc.on_close(Box::new(move || {
            log::info!("data channel '{name}' closed");
            Box::pin(async {})
        }));
        // (the message channel's on_open is the handshake, set above; don't replace it)
        if name != "message" {
            dc.on_open(Box::new(move || {
                log::info!("data channel '{name}' open");
                Box::pin(async {})
            }));
        }
    }

    pc.on_ice_connection_state_change(Box::new(|s| {
        log::info!("ICE connection: {s}");
        Box::pin(async {})
    }));
    {
        let failed = failed.clone();
        pc.on_peer_connection_state_change(Box::new(move |s| {
            log::info!("peer connection: {s}");
            if matches!(
                s,
                RTCPeerConnectionState::Failed
                    | RTCPeerConnectionState::Closed
                    | RTCPeerConnectionState::Disconnected
            ) {
                failed.notify_one();
            }
            Box::pin(async {})
        }));
    }

    {
        let shared = shared.clone();
        let repaint = repaint.clone();
        let decode = cfg.mode == PlayMode::Pc;
        pc.on_track(Box::new(move |track, _, _| {
            let shared = shared.clone();
            let repaint = repaint.clone();
            log::info!(
                "remote track: kind={} ssrc={} payload_type={}",
                track.kind(),
                track.ssrc(),
                track.payload_type()
            );
            tokio::spawn(async move {
                if track.kind() == RTPCodecType::Video {
                    read_video(track, shared, start, decode, repaint).await;
                } else if decode {
                    // PC Play: sound on this PC.
                    crate::audio::play(track, shared).await;
                } else {
                    // Sound plays on the TV; drain the audio track.
                    let mut n = 0u64;
                    while track.read_rtp().await.is_ok() {
                        n += 1;
                        if n == 1 || n % 2000 == 0 {
                            log::info!("audio RTP packets: {n}");
                        }
                    }
                    log::info!("audio track ended after {n} packets");
                }
            });
            Box::pin(async {})
        }));
    }

    // Offer → gather → exchange
    let offer = pc.create_offer(None).await?;
    let mut gathered = pc.gathering_complete_promise().await;
    pc.set_local_description(offer.clone()).await?;
    let _ = tokio::time::timeout(Duration::from_secs(3), gathered.recv()).await;
    let local_sdp = pc
        .local_description()
        .await
        .map(|d| d.sdp)
        .unwrap_or_default();
    let candidates = local_candidates(&local_sdp);
    log::info!("local ICE candidates: {}", candidates.len());
    for c in &candidates {
        log::info!("  local candidate: {}", c.candidate);
    }

    let answer = sig
        .exchange_sdp(limit_resolution(
            &tidy_sdp(&set_video_bitrate(&offer.sdp, cfg.video_bitrate)),
            1920,
            1080,
        ))
        .await
        .context("SDP exchange failed")?;
    log::info!("console SDP answer:\n{answer}");
    let video_ssrc = answer_video_ssrc(&answer);
    log::info!("console video ssrc: {video_ssrc:?}");
    pc.set_remote_description(RTCSessionDescription::answer(answer)?)
        .await?;
    for t in pc.get_transceivers().await {
        log::info!(
            "transceiver mid={:?} kind={} direction={}",
            t.mid(),
            t.kind(),
            t.current_direction()
        );
    }

    let mut remote = sig
        .exchange_ice(candidates)
        .await
        .context("ICE exchange failed")?;
    if let Some(ip) = cfg.lan_ip {
        let c = crate::xbox_lan::lan_candidate(ip);
        log::info!("adding direct LAN candidate: {}", c.trim());
        remote.insert(
            0,
            IceCandidate {
                candidate: c,
                sdp_mid: Some(json!("0")),
                sdp_mline_index: Some(json!("0")),
            },
        );
    }
    for c in &remote {
        if let Some(init) = remote_candidate(c) {
            if let Err(e) = pc.add_ice_candidate(init).await {
                log::warn!("ignoring remote candidate {}: {e}", c.candidate);
            }
        }
    }

    let mut stop_rx = shared.stop_tx.subscribe();
    tokio::select! {
        _ = handshake_done.notified() => {}
        _ = failed.notified() => bail!("Could not connect to your Xbox. Check that this PC and the console are on the same network."),
        _ = tokio::time::sleep(cfg.connect_timeout) => bail!("Timed out connecting to your Xbox. Check that this PC and the console are on the same network."),
        _ = stop_rx.changed() => return Ok(()),
    }

    // Start control + input (same order as the official web client).
    send_json(&control, json!({ "message": "authorizationRequest", "accessKey": "4BDB3609-C1F1-4195-9B37-FEFF45DA8B8E" })).await?;
    send_json(
        &control,
        json!({ "message": "gamepadChanged", "gamepadIndex": 0, "wasAdded": true }),
    )
    .await?;
    for m in [
        message(
            "/streaming/systemUi/configuration",
            json!({ "version": [0, 2, 0], "systemUis": [] }),
        ),
        message(
            "/streaming/properties/clientappinstallidchanged",
            json!({ "clientAppInstallId": uuid::Uuid::new_v4().to_string() }),
        ),
        message(
            "/streaming/characteristics/orientationchanged",
            json!({ "orientation": 0 }),
        ),
        message(
            "/streaming/characteristics/touchinputenabledchanged",
            json!({ "touchInputEnabled": false }),
        ),
        message(
            "/streaming/characteristics/clientdevicecapabilities",
            json!({}),
        ),
        message(
            "/streaming/characteristics/dimensionschanged",
            json!({
                "horizontal": 1920, "vertical": 1080, "preferredWidth": 1920, "preferredHeight": 1080,
                "safeAreaLeft": 0, "safeAreaTop": 0, "safeAreaRight": 1920, "safeAreaBottom": 1080,
                "supportsCustomResolution": true
            }),
        ),
    ] {
        send_json(&msg, m).await?;
    }
    let mut seq: u32 = 0;
    input
        .send(&Bytes::from(packet::client_metadata(seq, 1, ms(start))))
        .await?;
    if cfg.mode == PlayMode::Pc {
        shared.keyframe_wanted.store(true, Ordering::SeqCst);
    }

    shared.set(Phase::Connected, "Connected");
    repaint();

    let period = Duration::from_micros(1_000_000 / cfg.input_hz.clamp(30, 250) as u64);
    let mut input_tick = tokio::time::interval(period);
    input_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut keepalive = tokio::time::interval(Duration::from_secs(30));
    keepalive.tick().await;
    let mut stats_tick = tokio::time::interval(Duration::from_secs(5));
    stats_tick.tick().await;
    // A browser receiver sends RTCP feedback for the video stream from the start; webrtc-rs only
    // does once packets arrive. Until video flows, ask for a picture (PLI) and advertise bandwidth
    // (REMB) for the SSRC the console announced, once a second.
    let mut video_kick = tokio::time::interval(Duration::from_millis(1000));
    let mut kicks = 0u32;

    loop {
        tokio::select! {
            _ = input_tick.tick() => {
                let now = ms(start);
                let frame = shared.frame();
                let acks: Vec<FrameMeta> = {
                    let mut q = shared.acks.lock().unwrap();
                    let n = q.len().min(29);
                    q.drain(..n).collect()
                };
                seq = seq.wrapping_add(1);
                let p = packet::input_packet(seq, &acks, Some(&frame), now);
                if input.send(&Bytes::from(p)).await.is_err() {
                    bail!("Your Xbox closed the connection.");
                }
                shared.frames_sent.fetch_add(1, Ordering::Relaxed);
                if shared.keyframe_wanted.swap(false, Ordering::SeqCst) {
                    let _ = send_json(&control, json!({ "message": "videoKeyframeRequested", "ifrRequested": true })).await;
                }
            }
            _ = video_kick.tick(), if video_ssrc.is_some() => {
                // Receiver-side bandwidth estimate (REMB) for the whole session: without
                // transport-cc this is what the console's rate control runs on. Until video
                // flows, also ask for a full picture (PLI + keyframe request).
                let media_ssrc = video_ssrc.unwrap_or_default();
                let waiting = shared.video_packets.load(Ordering::Relaxed) == 0;
                kicks += 1;
                let mut pkts: Vec<Box<dyn webrtc::rtcp::packet::Packet + Send + Sync>> = vec![
                    Box::new(webrtc::rtcp::payload_feedbacks::receiver_estimated_maximum_bitrate::ReceiverEstimatedMaximumBitrate {
                        sender_ssrc: 1,
                        // LAN: don't hold the console back; its encoder picks what it can use
                        // (it settled around 16 Mbit/s at 1080p60 under a 25 Mbit/s cap).
                        bitrate: 60_000_000.0,
                        ssrcs: vec![media_ssrc],
                    }),
                ];
                if waiting && kicks <= 60 {
                    pkts.push(Box::new(webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication {
                        sender_ssrc: 1,
                        media_ssrc,
                    }));
                    let _ = send_json(&control, json!({ "message": "videoKeyframeRequested", "ifrRequested": true })).await;
                }
                match pc.write_rtcp(&pkts).await {
                    Ok(_) if waiting && (kicks == 1 || kicks % 10 == 0) => log::info!("sent video REMB+PLI #{kicks} for ssrc {media_ssrc}"),
                    Ok(_) => {}
                    Err(e) => log::warn!("video RTCP failed: {e}"),
                }
            }
            _ = stats_tick.tick() => {
                let f = shared.mapper.lock().unwrap().last_frame;
                let mut rtp = Vec::new();
                for (_, s) in pc.get_stats().await.reports {
                    if let Ok(v) = serde_json::to_value(&s) {
                        let ty = v["type"].as_str().unwrap_or_default().to_string();
                        if ty == "transport" || ty == "inbound-rtp" {
                            rtp.push(format!(
                                "{ty}{}: bytes={} packets={}",
                                v.get("kind").and_then(|k| k.as_str()).map(|k| format!("[{k}]")).unwrap_or_default(),
                                v["bytesReceived"], v["packetsReceived"]
                            ));
                        }
                    }
                }
                log::info!(
                    "stats: input sent={} video frames={} pad buttons={:#06x} l=({:.2},{:.2}) r=({:.2},{:.2}) | {}",
                    shared.frames_sent.load(Ordering::Relaxed),
                    shared.video_frames.load(Ordering::Relaxed),
                    f.buttons, f.lx, f.ly, f.rx, f.ry,
                    rtp.join("; ")
                );
            }
            _ = keepalive.tick() => {
                if let Err(e) = sig.keepalive().await {
                    log::warn!("keepalive failed: {e:#}");
                }
            }
            _ = failed.notified() => bail!("Your Xbox ended the session."),
            _ = stop_rx.changed() => {
                // Release everything on the console before leaving.
                let p = packet::input_packet(seq.wrapping_add(1), &[], Some(&PadFrame::default()), ms(start));
                let _ = input.send(&Bytes::from(p)).await;
                // Tell the console our pad is gone (mirror of the sign-on), so it hands games
                // back to the real controller instead of keeping a dead virtual pad.
                let _ = send_json(&control, json!({ "message": "gamepadChanged", "gamepadIndex": 0, "wasAdded": false })).await;
                // Give the data channel a moment to flush before the connection is closed.
                tokio::time::sleep(Duration::from_millis(150)).await;
                return Ok(());
            }
        }
    }
}

fn ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

async fn read_video(
    track: Arc<TrackRemote>,
    shared: Arc<Shared>,
    start: Instant,
    decode: bool,
    repaint: Repaint,
) {
    let decoder_tx = if decode {
        Some(crate::video::spawn_decoder(shared.clone(), repaint))
    } else {
        None
    };
    let mut current_ts: Option<u32> = None;
    let mut first_arrival = 0u32;
    let mut packets = 0u64;
    let mut frames = 0u64;
    while let Ok((pkt, _)) = track.read_rtp().await {
        let now = ms(start) as u32;
        packets += 1;
        shared.video_packets.fetch_add(1, Ordering::Relaxed);
        if pkt.header.marker {
            frames += 1;
        }
        if packets == 1 || packets % 5000 == 0 {
            log::info!(
                "video RTP packets: {packets}, frames (marker bits): {frames}, payload_type={}",
                pkt.header.payload_type
            );
        }
        if current_ts != Some(pkt.header.timestamp) {
            current_ts = Some(pkt.header.timestamp);
            first_arrival = now;
        }
        if pkt.header.marker {
            let mut q = shared.acks.lock().unwrap();
            q.push_back(FrameMeta {
                server_data_key: pkt.header.timestamp,
                first_packet_arrival_ms: first_arrival,
                submitted_ms: now,
                decoded_ms: now,
                rendered_ms: now,
            });
            while q.len() > 120 {
                q.pop_front();
            }
        }
        if let Some(tx) = &decoder_tx {
            if tx.send(pkt).is_err() {
                break;
            }
        }
    }
}

/// Ask the console for a full video frame (after a decode error).
pub fn request_keyframe(shared: &Shared) {
    shared.keyframe_wanted.store(true, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_queue_keeps_the_newest_few_frames_in_order_with_their_times() {
        let q = VideoQueue::default();
        assert!(q.is_empty() && q.drain().is_empty());
        let frame = |w: usize| VideoFrame::Nv12 {
            width: w,
            height: 2,
            data: vec![0; w * 3],
        };
        for w in [2, 4, 6, 8, 10] {
            q.push(frame(w));
        }
        // Bounded: the two oldest were dropped, never the newest.
        assert_eq!(q.newest_size(), Some((10, 2)));
        let frames = q.drain();
        let widths: Vec<usize> = frames
            .iter()
            .map(|f| match &f.frame {
                VideoFrame::Nv12 { width, .. } => *width,
                VideoFrame::Rgba(_) => 0,
            })
            .collect();
        assert_eq!(widths, [6, 8, 10]);
        assert_eq!(frames.len(), VIDEO_QUEUE);
        assert!(frames.windows(2).all(|p| p[0].at <= p[1].at));
        assert!(q.is_empty());
    }

    /// Diagnostic: print the SDP offer we send the console (`cargo test dump_offer -- --ignored --nocapture`).
    #[tokio::test]
    #[ignore]
    async fn dump_offer() {
        crate::init_crypto();
        let cfg = StreamConfig {
            mode: PlayMode::Pc,
            high_quality: true,
            ..Default::default()
        };
        let mut m = media_engine(&cfg).unwrap();
        let registry = interceptors(&mut m);
        let api = APIBuilder::new()
            .with_media_engine(m)
            .with_interceptor_registry(registry)
            .build();
        let pc = api
            .new_peer_connection(RTCConfiguration::default())
            .await
            .unwrap();
        for (n, p) in [
            ("input", "1.0"),
            ("chat", "chatV1"),
            ("control", "controlV1"),
            ("message", "messageV1"),
        ] {
            pc.create_data_channel(
                n,
                Some(RTCDataChannelInit {
                    ordered: Some(true),
                    protocol: Some(p.into()),
                    ..Default::default()
                }),
            )
            .await
            .unwrap();
        }
        for kind in [RTPCodecType::Audio, RTPCodecType::Video] {
            pc.add_transceiver_from_kind(
                kind,
                Some(RTCRtpTransceiverInit {
                    direction: RTCRtpTransceiverDirection::Recvonly,
                    send_encodings: vec![],
                }),
            )
            .await
            .unwrap();
        }
        let offer = pc.create_offer(None).await.unwrap();
        println!("{}", offer.sdp);
    }

    #[test]
    fn bitrate_munging() {
        let sdp = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\nc=IN IP4 0.0.0.0\r\nm=video 9 UDP/TLS/RTP/SAVPF 106\r\nc=IN IP4 0.0.0.0\r\na=mid:1\r\n";
        let out = set_video_bitrate(sdp, 1500);
        assert!(out.contains(
            "m=video 9 UDP/TLS/RTP/SAVPF 106\r\nc=IN IP4 0.0.0.0\r\nb=AS:1500\r\na=mid:1"
        ));
        assert_eq!(out.matches("b=AS").count(), 1);
        assert_eq!(set_video_bitrate(sdp, 0), sdp);
    }

    #[test]
    fn candidate_formats() {
        let sdp = "a=mid:0\r\na=candidate:1 1 udp 2130706431 192.168.1.2 50000 typ host\r\na=end-of-candidates\r\n";
        let c = local_candidates(sdp);
        assert_eq!(c.len(), 1);
        assert_eq!(
            c[0].candidate,
            "candidate:1 1 udp 2130706431 192.168.1.2 50000 typ host"
        );
        let r = remote_candidate(&IceCandidate {
            candidate: "a=candidate:1 1 UDP 1 10.0.0.5 9002 typ host ".into(),
            sdp_mid: Some(json!("0")),
            sdp_mline_index: Some(json!("0")),
        })
        .unwrap();
        assert_eq!(r.candidate, "candidate:1 1 UDP 1 10.0.0.5 9002 typ host");
        assert_eq!(r.sdp_mline_index, Some(0));
        assert!(remote_candidate(&IceCandidate {
            candidate: "a=end-of-candidates".into(),
            sdp_mid: None,
            sdp_mline_index: None
        })
        .is_none());
    }
}
