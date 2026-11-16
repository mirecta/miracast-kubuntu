//! A tiny WFD *sink* emulator used to test the source without a TV.

use crate::rtsp::{Conn, Message};
use anyhow::{Context, Result, bail};
use std::io::Write;
use std::time::{Duration, Instant};
use tokio::net::{TcpStream, UdpSocket};

const CAPS: &str = "wfd_video_formats: 00 00 03 10 0001ffff 1fffffff 00001fff 00 0000 0000 10 none none\r\n\
wfd_audio_codecs: LPCM 00000002 00, AAC 00000001 00\r\n\
wfd_client_rtp_ports: RTP/AVP/UDP;unicast 19500 0 mode=play\r\n";

pub async fn run(addr: &str, seconds: u64, dump: Option<&str>, verbose: bool) -> Result<()> {
    let udp = UdpSocket::bind("0.0.0.0:19500").await?;
    let stream = loop {
        match TcpStream::connect(addr).await {
            Ok(s) => break s,
            Err(_) => tokio::time::sleep(Duration::from_millis(300)).await,
        }
    };
    let mut c = Conn::new(stream, verbose);
    let mut dump = match dump {
        Some(p) => Some(std::fs::File::create(p)?),
        None => None,
    };
    let mut setup_cseq = 0;
    let mut play_cseq = 0;
    let mut playing_since: Option<Instant> = None;
    let (mut packets, mut bytes) = (0u64, 0u64);
    let mut buf = vec![0u8; 2048];
    let mut session = String::new();

    loop {
        if let Some(t) = playing_since {
            if t.elapsed() > Duration::from_secs(seconds) {
                c.request("TEARDOWN", "rtsp://src/wfd1.0/streamid=0", &[("Session", session.clone())], "").await?;
                break;
            }
        }
        tokio::select! {
            m = c.read() => {
                let m: Message = m?;
                if m.is_response() {
                    if m.status() != 200 { bail!("source error: {}", m.start); }
                    if m.cseq() == setup_cseq {
                        session = m.header("Session").unwrap_or("").split(';').next().unwrap_or("").to_string();
                        play_cseq = c.request("PLAY", "rtsp://src/wfd1.0/streamid=0", &[("Session", session.clone())], "").await?;
                    } else if m.cseq() == play_cseq {
                        println!("fake-sink: PLAY ok, receiving");
                        playing_since = Some(Instant::now());
                    }
                    continue;
                }
                match m.method() {
                    "OPTIONS" => {
                        c.respond(&m, "200 OK", &[("Public", "org.wfa.wfd1.0, GET_PARAMETER, SET_PARAMETER".into())], "").await?;
                        c.request("OPTIONS", "*", &[("Require", "org.wfa.wfd1.0".into())], "").await?;
                    }
                    "GET_PARAMETER" => {
                        let body = if m.body.trim().is_empty() { "" } else { CAPS };
                        c.respond(&m, "200 OK", &[], body).await?;
                    }
                    "SET_PARAMETER" => {
                        c.respond(&m, "200 OK", &[], "").await?;
                        if m.body.contains("wfd_video_formats") {
                            println!("fake-sink: M4 {}", m.body.replace("\r\n", " | "));
                        }
                        if m.body.contains("wfd_trigger_method: SETUP") {
                            setup_cseq = c.request("SETUP", "rtsp://src/wfd1.0/streamid=0",
                                &[("Transport", "RTP/AVP/UDP;unicast;client_port=19500".into())], "").await?;
                        }
                        if m.body.contains("wfd_trigger_method: TEARDOWN") {
                            c.request("TEARDOWN", "rtsp://src/wfd1.0/streamid=0", &[("Session", session.clone())], "").await?;
                            break;
                        }
                    }
                    _ => c.respond(&m, "200 OK", &[], "").await?,
                }
            }
            r = udp.recv(&mut buf) => {
                let n = r.context("udp")?;
                packets += 1;
                bytes += n as u64;
                if let Some(f) = dump.as_mut() {
                    // strip the 12-byte RTP header -> raw MPEG-TS
                    f.write_all(&buf[12.min(n)..n])?;
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(200)) => {}
        }
    }
    let secs = playing_since.map(|t| t.elapsed().as_secs_f64()).unwrap_or(1.0);
    println!("fake-sink: {packets} RTP packets, {:.0} kbit/s", bytes as f64 * 8.0 / 1000.0 / secs);
    Ok(())
}
