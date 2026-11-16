mod audio;
mod fake_sink;
mod p2p;
mod pipeline;
mod rtsp;
mod screen;
mod source;
mod wfd;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use futures_util::StreamExt;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::time::Duration;
use tokio::net::TcpListener;

#[derive(Parser)]
#[command(name = "miracast", version, about = "Miracast (Wi-Fi Display) source for KDE Plasma on Wayland")]
struct Cli {
    /// Print RTSP traffic and the GStreamer pipeline
    #[arg(short, long, global = true)]
    verbose: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, PartialEq, ValueEnum)]
enum AudioMode {
    /// Create a "Miracast TV" audio output and make it the default (sound only on TV)
    Tv,
    /// Capture whatever plays on the current default output (sound on both)
    Both,
    /// No audio
    None,
}

#[derive(Subcommand)]
enum Cmd {
    /// Look for Miracast displays (the TV must be in "Screen Mirroring" mode)
    Scan {
        #[arg(short, long, default_value_t = 15)]
        time: u64,
    },
    /// Connect to a TV and use it as an extra monitor (or mirror a screen)
    Cast {
        /// TV name or MAC address (substring match). Default: first display found
        #[arg(short, long)]
        peer: Option<String>,
        /// Mirror an existing output instead of creating a new monitor.
        /// Optionally a connector name, e.g. --mirror=eDP-1
        #[arg(long, num_args = 0..=1, default_missing_value = "")]
        mirror: Option<String>,
        /// Maximum vertical resolution (1080, 720, 480)
        #[arg(long, default_value_t = 1080)]
        res: i32,
        /// Maximum frame rate (30 or 60)
        #[arg(long, default_value_t = 30)]
        fps: i32,
        /// Video bitrate in kbit/s
        #[arg(long, default_value_t = 8000)]
        bitrate: u32,
        /// Use H.264 Constrained High profile if the TV supports it
        #[arg(long)]
        high: bool,
        #[arg(long, value_enum, default_value_t = AudioMode::Tv)]
        audio: AudioMode,
        #[arg(long, value_enum, default_value_t = pipeline::Encoder::Auto)]
        encoder: pipeline::Encoder,
        /// Skip Wi-Fi Direct, just wait for a sink on TCP 7236 (testing)
        #[arg(long, hide = true)]
        no_p2p: bool,
        /// Stream a test pattern instead of the screen (testing)
        #[arg(long, hide = true)]
        test_pattern: bool,
    },
    /// Register this binary with KWin (needed for the virtual monitor / screen capture)
    Install,
    /// Emulate a TV for local testing
    #[command(hide = true)]
    FakeSink {
        #[arg(long, default_value = "127.0.0.1:7236")]
        addr: String,
        #[arg(long, default_value_t = 8)]
        seconds: u64,
        #[arg(long)]
        dump: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Scan { time } => scan(time).await,
        Cmd::Install => install(),
        Cmd::FakeSink { addr, seconds, dump } => fake_sink::run(&addr, seconds, dump.as_deref(), cli.verbose).await,
        Cmd::Cast { peer, mirror, res, fps, bitrate, high, audio, encoder, no_p2p, test_pattern } => {
            let opts = CastOpts { peer, mirror, res, fps, bitrate, high, audio, encoder, no_p2p, test_pattern, verbose: cli.verbose };
            cast(opts).await
        }
    }
}

fn print_peer(p: &p2p::Peer) {
    let kind = match p.wfd {
        Some(w) if p.is_sink() => format!("Miracast display{}", if w.session_available { "" } else { " (busy)" }),
        Some(_) => "Miracast source".into(),
        None => "Wi-Fi Direct device".into(),
    };
    println!("  {:<28} {}  {:>3}%  {}  {} {}", p.name, p.hw_address, p.strength, kind, p.manufacturer, p.model);
}

async fn scan(time: u64) -> Result<()> {
    let p2p = p2p::P2p::new().await?;
    println!("Scanning on {} for {time}s ...", p2p.iface);
    let mut seen = std::collections::HashSet::new();
    let peers = p2p
        .discover(time, |peers| {
            for p in peers {
                if seen.insert(p.hw_address.clone()) {
                    print_peer(p);
                }
            }
            false
        })
        .await?;
    if peers.is_empty() {
        println!("Nothing found. On the Samsung TV open Source -> Screen Mirroring (or press the Miracast/Mirroring button).");
    }
    Ok(())
}

struct CastOpts {
    peer: Option<String>,
    mirror: Option<String>,
    res: i32,
    fps: i32,
    bitrate: u32,
    high: bool,
    audio: AudioMode,
    encoder: pipeline::Encoder,
    no_p2p: bool,
    test_pattern: bool,
    verbose: bool,
}

fn matches(p: &p2p::Peer, want: &Option<String>) -> bool {
    match want {
        Some(w) => {
            let w = w.to_lowercase();
            p.name.to_lowercase().contains(&w) || p.hw_address.to_lowercase() == w
        }
        None => p.is_sink(),
    }
}

async fn cast(o: CastOpts) -> Result<()> {
    gst::init()?;
    // The TV connects to us on 7236 right after the P2P link is up, so listen first.
    let listener = TcpListener::bind("0.0.0.0:7236").await.context("cannot listen on TCP 7236 (another instance running?)")?;

    let mut p2p_state = None;
    let mut tv_name = "TV".to_string();
    if !o.no_p2p {
        let p2p = p2p::P2p::new().await?;
        println!("Searching for displays on {} (TV must be in Screen Mirroring mode) ...", p2p.iface);
        let peers = p2p.discover(60, |peers| peers.iter().any(|p| matches(p, &o.peer))).await?;
        let Some(peer) = peers.into_iter().find(|p| matches(p, &o.peer)) else {
            bail!("no matching display found (try `miracast scan`)");
        };
        print!("Found:");
        print_peer(&peer);
        tv_name = peer.name.clone();
        println!("Connecting (confirm the connection on the TV if asked) ...");
        let active = p2p.connect(&peer).await?;
        let (iface, ip) = p2p.group_info(&active).await.unwrap_or_default();
        println!("Wi-Fi Direct link up on {iface}, our IP {}", ip.as_deref().unwrap_or("?"));
        p2p_state = Some((p2p, active));
    } else {
        println!("Waiting for a sink on TCP 7236 ...");
    }

    let result = session(&o, &listener, &tv_name).await;

    if let Some((p2p, active)) = p2p_state {
        p2p.disconnect(&active).await;
    }
    result
}

async fn session(o: &CastOpts, listener: &TcpListener, tv_name: &str) -> Result<()> {
    let mut src = tokio::time::timeout(Duration::from_secs(40), source::WfdSource::accept(listener, o.verbose))
        .await
        .context("the TV did not open the RTSP control connection")??;
    println!("TV connected from {}", src.sink_ip);

    let prefs = wfd::Prefs { max_height: o.res, max_fps: o.fps, audio: o.audio != AudioMode::None, high_profile: o.high };
    let neg = src.negotiate(&prefs).await?;
    println!(
        "Negotiated {}x{}@{} H.264 {}, audio: {:?}",
        neg.mode.width,
        neg.mode.height,
        neg.mode.fps,
        if neg.profile == 2 { "CHP" } else { "CBP" },
        neg.audio
    );

    // Screen source
    let cast = if o.test_pattern {
        None
    } else {
        let s = match &o.mirror {
            Some(name) => screen::Source::Mirror((!name.is_empty()).then(|| name.clone())),
            None => screen::Source::VirtualOutput {
                name: format!("Miracast {tv_name}"),
                width: neg.mode.width,
                height: neg.mode.height,
            },
        };
        let c = screen::start(s)?;
        if o.mirror.is_none() {
            println!("Virtual monitor {}x{} created – arrange it in System Settings → Display", neg.mode.width, neg.mode.height);
        }
        Some(c)
    };

    // Audio
    let mut tv_sink = None;
    let audio_dev = match (neg.audio, o.audio) {
        (wfd::Audio::Aac, AudioMode::Tv) => match audio::TvSink::create(&format!("Miracast {tv_name}")) {
            Ok((s, mon)) => {
                tv_sink = Some(s);
                Some(mon)
            }
            Err(e) => {
                eprintln!("warning: cannot create TV audio output ({e}), capturing default output instead");
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
    println!("Streaming to {}:{rtp_port}. Ctrl+C to stop.", src.sink_ip);

    let mut bus = stream.bus().stream();
    let mut keepalive = tokio::time::interval(Duration::from_secs(20));
    keepalive.tick().await;
    let mut kf = tokio::time::interval(Duration::from_millis(500));
    let mut kf_left = 4; // a few early IDRs help sinks that join late

    let res = loop {
        tokio::select! {
            ev = src.next_event() => match ev {
                Ok(source::Event::Teardown) => { println!("TV closed the session"); break Ok(()); }
                Ok(source::Event::IdrRequest) => stream.force_keyframe(),
                Ok(source::Event::Pause) => stream.pause(),
                Ok(source::Event::Play) => { let _ = stream.play(); stream.force_keyframe(); }
                Ok(_) => {}
                Err(e) => break Err(e.context("RTSP connection lost")),
            },
            _ = keepalive.tick() => { if let Err(e) = src.keepalive().await { break Err(e); } }
            _ = kf.tick(), if kf_left > 0 => { kf_left -= 1; stream.force_keyframe(); }
            msg = bus.next() => {
                use gst::MessageView;
                let Some(msg) = msg else { break Ok(()) };
                match msg.view() {
                    MessageView::Error(e) => break Err(anyhow::anyhow!("pipeline error from {:?}: {} ({:?})",
                        e.src().map(|s| s.path_string()), e.error(), e.debug())),
                    MessageView::Warning(w) if o.verbose => eprintln!("pipeline warning: {}", w.error()),
                    MessageView::Eos(_) => break Ok(()),
                    _ => {}
                }
            }
            _ = tokio::signal::ctrl_c() => {
                println!("Stopping ...");
                src.trigger_teardown().await;
                tokio::time::sleep(Duration::from_millis(300)).await;
                break Ok(());
            }
        }
    };
    stream.stop();
    drop(tv_sink);
    if let Some(c) = cast {
        c.stop();
    }
    res
}

fn install() -> Result<()> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let dir = std::path::PathBuf::from(std::env::var("HOME")?).join(".local/share/applications");
    std::fs::create_dir_all(&dir)?;
    let file = dir.join("io.github.miracast-rs.desktop");
    std::fs::write(
        &file,
        format!(
            "[Desktop Entry]\nType=Application\nName=Miracast\nComment=Use a Miracast TV as a monitor\n\
             Exec={}\nIcon=video-display\nTerminal=true\nNoDisplay=true\n\
             X-KDE-Wayland-Interfaces=zkde_screencast_unstable_v1\n",
            exe.display()
        ),
    )?;
    let _ = std::process::Command::new("kbuildsycoca6").arg("--noincremental").output();
    println!("Wrote {}\nKWin will now allow {} to create virtual monitors.", file.display(), exe.display());
    println!("Note: the permission is tied to this exact path – re-run `install` if you move the binary.");
    Ok(())
}
