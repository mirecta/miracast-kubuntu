//! Paced RTP/UDP sender.
//!
//! The encoder emits a whole frame at once; a keyframe is ~100+ RTP packets in
//! a single burst, which overflows Wi-Fi / TV receive queues and shows up as a
//! broken picture until the next IDR. We spread packets out with a token bucket
//! and mark them as video traffic (DSCP CS5 -> WMM AC_VI).

use anyhow::{Context, Result};
use gstreamer as gst;
use gstreamer_app as gst_app;
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct Stats {
    pub sent: AtomicU64,
    pub dropped: AtomicU64,
}

pub fn attach(appsink: &gst_app::AppSink, dest: SocketAddr, bind_port: u16, bitrate_kbps: u32) -> Result<Arc<Stats>> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_reuse_address(true)?;
    let _ = sock.set_send_buffer_size(4 << 20);
    let _ = sock.set_tos_v4(0xa0); // CS5 -> video access category
    sock.bind(&SockAddr::from(SocketAddr::from(([0, 0, 0, 0], bind_port))))
        .or_else(|_| sock.bind(&SockAddr::from(SocketAddr::from(([0, 0, 0, 0], 0)))))
        .context("cannot bind UDP socket")?;
    sock.connect(&SockAddr::from(dest)).context("cannot connect UDP socket")?;
    let sock: std::net::UdpSocket = sock.into();

    let stats = Arc::new(Stats::default());
    // ~2 s worth of packets at 20 Mbit/s; anything beyond that is hopeless anyway.
    let (tx, rx) = mpsc::sync_channel::<gst::Buffer>(4000);

    let st = stats.clone();
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let mut push = |b: &gst::BufferRef| {
                    if tx.try_send(b.to_owned()).is_err() {
                        st.dropped.fetch_add(1, Ordering::Relaxed);
                    }
                };
                if let Some(list) = sample.buffer_list() {
                    list.iter().for_each(&mut push);
                } else if let Some(b) = sample.buffer() {
                    push(b);
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    // Send at up to 3x the video bitrate (plus audio/overhead), with a small burst allowance.
    let rate = (bitrate_kbps.max(2000) as f64 * 1000.0 / 8.0) * 3.0 + 200_000.0; // bytes/s
    let burst = Duration::from_secs_f64(16_384.0 / rate);
    let st = stats.clone();
    std::thread::Builder::new().name("rtp-pacer".into()).spawn(move || {
        let mut next = Instant::now();
        for buf in rx {
            let Ok(map) = buf.map_readable() else { continue };
            let now = Instant::now();
            if next + burst < now {
                next = now - burst;
            }
            if next > now {
                std::thread::sleep(next - now);
            }
            // ENOBUFS / EAGAIN: back off briefly and retry once
            if sock.send(&map).is_err() {
                std::thread::sleep(Duration::from_millis(2));
                if sock.send(&map).is_err() {
                    st.dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
            st.sent.fetch_add(1, Ordering::Relaxed);
            next += Duration::from_secs_f64(map.len() as f64 / rate);
        }
    })?;
    Ok(stats)
}
