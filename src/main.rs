use anyhow::Result;
use clap::{Parser, Subcommand};
use miracast::{AudioMode, CancellationToken, CastOpts, Status, p2p, pipeline};
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "miracast", version, about = "Miracast (Wi-Fi Display) source for KDE Plasma on Wayland")]
struct Cli {
    /// Print RTSP traffic and the GStreamer pipeline
    #[arg(short, long, global = true)]
    verbose: bool,
    #[command(subcommand)]
    cmd: Cmd,
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
        Cmd::Install => {
            let exe = std::env::current_exe()?.canonicalize()?;
            let f = miracast::screen::install_desktop_file(&exe, "io.github.miracast-rs", None)?;
            println!("Wrote {}\nKWin will now allow {} to create virtual monitors.", f.display(), exe.display());
            println!("Note: the permission is tied to this exact path – re-run `install` if you move the binary.");
            Ok(())
        }
        Cmd::FakeSink { addr, seconds, dump } => miracast::fake_sink::run(&addr, seconds, dump.as_deref(), cli.verbose).await,
        Cmd::Cast { peer, mirror, res, fps, bitrate, high, audio, encoder, no_p2p, test_pattern } => {
            let opts = CastOpts { peer, mirror, res, fps, bitrate, high, audio, encoder, no_p2p, test_pattern, verbose: cli.verbose };
            let cancel = CancellationToken::new();
            let c = cancel.clone();
            tokio::spawn(async move {
                let _ = tokio::signal::ctrl_c().await;
                println!("Stopping ...");
                c.cancel();
            });
            let report = Arc::new(|s: Status| match s {
                Status::Searching => println!("Searching for displays (TV must be in Screen Mirroring mode) ..."),
                Status::Connecting(n) => println!("Connecting to {n} (confirm the connection on the TV if asked) ..."),
                Status::WaitingForTv => println!("Waiting for the TV to connect on TCP 7236 ..."),
                Status::Streaming(d) => println!("Streaming {d}. Ctrl+C to stop."),
                Status::Info(i) => println!("{i}"),
            });
            miracast::cast(opts, report, cancel).await
        }
    }
}

pub fn print_peer(p: &p2p::Peer) {
    let kind = match p.wfd {
        Some(w) if p.is_sink() => format!("Miracast display{}", if w.session_available { "" } else { " (busy)" }),
        Some(_) => "Miracast source".into(),
        None => "Wi-Fi Direct device".into(),
    };
    println!("  {:<28} {}  {:>3}%  {}  {} {}", p.name, p.hw_address, p.strength, kind, p.manufacturer, p.model);
}

async fn scan(time: u64) -> Result<()> {
    println!("Scanning for {time}s ...");
    let seen = std::sync::Mutex::new(std::collections::HashSet::new());
    let found = std::sync::atomic::AtomicBool::new(false);
    miracast::scan(
        time,
        |peers| {
            for p in peers {
                found.store(true, std::sync::atomic::Ordering::Relaxed);
                if seen.lock().unwrap().insert(p.hw_address.clone()) {
                    print_peer(p);
                }
            }
        },
        CancellationToken::new(),
    )
    .await?;
    if !found.load(std::sync::atomic::Ordering::Relaxed) {
        println!("Nothing found. On the Samsung TV open Source -> Screen Mirroring.");
    }
    Ok(())
}
