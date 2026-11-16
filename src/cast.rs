//! The whole casting flow (discovery -> P2P -> RTSP -> screen -> stream), shared by CLI and GUI.

use crate::{audio, p2p, pipeline, screen, source, wfd};
use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, PartialEq, Debug, clap::ValueEnum)]
pub enum AudioMode {
    /// Create a "Miracast TV" audio output and make it the default (sound only on TV)
    Tv,
    /// Capture whatever plays on the current default output (sound on both)
    Both,
    /// No audio
    None,
}

#[derive(Clone, Debug)]
pub struct CastOpts {
    /// TV name substring or MAC; None = first Miracast display
    pub peer: Option<String>,
    /// None = new virtual monitor; Some("") = first output; Some(name) = that output
    pub mirror: Option<String>,
    pub res: i32,
    pub fps: i32,
    pub bitrate: u32,
    pub high: bool,
    pub audio: AudioMode,
    pub encoder: pipeline::Encoder,
    pub no_p2p: bool,
    pub test_pattern: bool,
    pub verbose: bool,
}

impl Default for CastOpts {
    fn default() -> Self {
        Self {
            peer: None,
            mirror: None,
            res: 1080,
            fps: 30,
            bitrate: 8000,
            high: false,
            audio: AudioMode::Tv,
            encoder: pipeline::Encoder::Auto,
            no_p2p: false,
            test_pattern: false,
            verbose: false,
        }
    }
}

#[derive(Clone, Debug)]
pub enum Status {
    Searching,
    Connecting(String),
    WaitingForTv,
    Streaming(String),
    Info(String),
}

pub type Reporter = Arc<dyn Fn(Status) + Send + Sync>;

fn matches(p: &p2p::Peer, want: &Option<String>) -> bool {
    match want {
        Some(w) => {
            let w = w.to_lowercase();
            p.name.to_lowercase().contains(&w) || p.hw_address.to_lowercase() == w
        }
        None => p.is_sink(),
    }
}

/// Run a full cast session until the TV disconnects, an error occurs or `cancel` fires.
pub async fn cast(o: CastOpts, report: Reporter, cancel: CancellationToken) -> Result<()> {
    gst::init()?;
    // The TV connects to us on 7236 right after the P2P link is up, so listen first.
    let listener = TcpListener::bind("0.0.0.0:7236").await.context("cannot listen on TCP 7236 (is another instance running?)")?;

    let mut p2p_state = None;
    let mut tv_name = "TV".to_string();
    let result = async {
        if !o.no_p2p {
            let p2p = p2p::P2p::new().await?;
            report(Status::Searching);
            let peers = tokio::select! {
                r = p2p.discover(60, |peers| peers.iter().any(|p| matches(p, &o.peer))) => r?,
                _ = cancel.cancelled() => { p2p.stop_find().await; return Ok(()); }
            };
            let peer = peers.into_iter().find(|p| matches(p, &o.peer)).ok_or_else(|| anyhow!("no matching display found"))?;
            tv_name = peer.name.clone();
            report(Status::Connecting(peer.name.clone()));
            let p2p = Arc::new(p2p);
            let active = tokio::select! {
                r = p2p.connect(&peer) => r?,
                _ = cancel.cancelled() => return Ok(()),
            };
            let (iface, ip) = p2p.group_info(&active).await.unwrap_or_default();
            report(Status::Info(format!("Wi-Fi Direct link up on {iface}, our IP {}", ip.as_deref().unwrap_or("?"))));
            p2p_state = Some((p2p, active));
        }
        report(Status::WaitingForTv);
        session(&o, &listener, &tv_name, &report, &cancel).await
    }
    .await;

    if let Some((p2p, active)) = p2p_state {
        p2p.disconnect(&active).await;
    }
    result
}

async fn session(o: &CastOpts, listener: &TcpListener, tv_name: &str, report: &Reporter, cancel: &CancellationToken) -> Result<()> {
    let mut src = tokio::select! {
        r = tokio::time::timeout(Duration::from_secs(40), source::WfdSource::accept(listener, o.verbose)) =>
            r.context("the TV did not open the RTSP control connection")??,
        _ = cancel.cancelled() => return Ok(()),
    };
    report(Status::Info(format!("TV connected from {}", src.sink_ip)));

    let prefs = wfd::Prefs { max_height: o.res, max_fps: o.fps, audio: o.audio != AudioMode::None, high_profile: o.high };
    let neg = src.negotiate(&prefs).await?;
    let desc = format!(
        "{}x{}@{} H.264 {}, audio {}",
        neg.mode.width,
        neg.mode.height,
        neg.mode.fps,
        if neg.profile == 2 { "CHP" } else { "CBP" },
        if neg.audio == wfd::Audio::Aac { "AAC" } else { "off" }
    );
    report(Status::Info(format!("Negotiated {desc}")));

    let cast = if o.test_pattern {
        None
    } else {
        let s = match &o.mirror {
            Some(name) => screen::Source::Mirror((!name.is_empty()).then(|| name.clone())),
            None => screen::Source::VirtualOutput { name: format!("Miracast {tv_name}"), width: neg.mode.width, height: neg.mode.height },
        };
        let c = screen::start(s)?;
        if o.mirror.is_none() {
            report(Status::Info("Virtual monitor created – arrange it in System Settings → Display".into()));
        }
        Some(c)
    };

    let mut tv_sink = None;
    let audio_dev = match (neg.audio, o.audio) {
        (wfd::Audio::Aac, AudioMode::Tv) => match audio::TvSink::create(&format!("Miracast {tv_name}")) {
            Ok((s, mon)) => {
                tv_sink = Some(s);
                Some(mon)
            }
            Err(e) => {
                report(Status::Info(format!("cannot create TV audio output ({e}), capturing default output")));
                None
            }
        },
        _ => None,
    };

    let rtp_port = src.setup_and_play(neg.rtp_port).await?;
    let cfg = pipeline::Config {
        input: match &cast {
            Some(c) => pipeline::VideoInput::PipeWire(c.node_id),
            None => pipeline::VideoInput::TestPattern,
        },
        audio_device: audio_dev.as_deref(),
        neg: &neg,
        bitrate_kbps: o.bitrate,
        encoder: o.encoder,
        host: src.sink_ip.to_string(),
        port: rtp_port,
        bind_port: source::SERVER_RTP_PORT,
    };
    let stream = pipeline::Stream::new(&cfg, o.verbose)?;
    stream.play()?;
    report(Status::Streaming(format!("{tv_name}: {desc}")));

    let mut bus = stream.bus().stream();
    let mut keepalive = tokio::time::interval(Duration::from_secs(20));
    keepalive.tick().await;
    let mut kf = tokio::time::interval(Duration::from_millis(500));
    let mut kf_left = 4; // a few early IDRs help sinks that join late
    let mut idr_requests = 0u32;
    let mut last_dropped = 0u64;

    let res = loop {
        tokio::select! {
            ev = src.next_event() => match ev {
                Ok(source::Event::Teardown) => { report(Status::Info("TV closed the session".into())); break Ok(()); }
                Ok(source::Event::IdrRequest) => {
                    idr_requests += 1;
                    report(Status::Info(format!("TV reported a broken picture, sending keyframe (#{idr_requests})")));
                    stream.force_keyframe();
                }
                Ok(source::Event::Pause) => stream.pause(),
                Ok(source::Event::Play) => { let _ = stream.play(); stream.force_keyframe(); }
                Ok(_) => {}
                Err(e) => break Err(e.context("RTSP connection lost")),
            },
            _ = keepalive.tick() => {
                if let Err(e) = src.keepalive().await { break Err(e); }
                let d = stream.stats.dropped.load(std::sync::atomic::Ordering::Relaxed);
                if d > last_dropped {
                    report(Status::Info(format!("{} packets dropped locally in the last 20 s", d - last_dropped)));
                    last_dropped = d;
                }
            }
            _ = kf.tick(), if kf_left > 0 => { kf_left -= 1; stream.force_keyframe(); }
            msg = bus.next() => {
                use gst::MessageView;
                let Some(msg) = msg else { break Ok(()) };
                match msg.view() {
                    MessageView::Error(e) => break Err(anyhow!("pipeline error from {:?}: {} ({:?})",
                        e.src().map(|s| s.path_string()), e.error(), e.debug())),
                    MessageView::Warning(w) if o.verbose => eprintln!("pipeline warning: {}", w.error()),
                    MessageView::Eos(_) => break Ok(()),
                    _ => {}
                }
            }
            _ = cancel.cancelled() => {
                src.trigger_teardown().await;
                tokio::time::sleep(Duration::from_millis(300)).await;
                break Ok(());
            }
        }
    };
    stream.stop();
    drop(tv_sink);
    drop(cast);
    res
}

/// Scan for peers, reporting the full list each time it changes.
pub async fn scan(secs: u64, on_update: impl Fn(&[p2p::Peer]) + Send, cancel: CancellationToken) -> Result<()> {
    let p2p = p2p::P2p::new().await?;
    let mut last = Vec::<String>::new();
    let r = tokio::select! {
        r = p2p.discover(secs, |peers| {
            let keys: Vec<String> = peers.iter().map(|p| format!("{}{}", p.hw_address, p.name)).collect();
            if keys != last {
                last = keys;
                on_update(peers);
            }
            false
        }) => r.map(|_| ()),
        _ = cancel.cancelled() => Ok(()),
    };
    p2p.stop_find().await;
    if r.is_err() {
        bail!("{}", r.unwrap_err());
    }
    Ok(())
}
