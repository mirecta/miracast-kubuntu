//! Paced, congestion-aware RTP/UDP sender.
//!
//! - Packets are spread out with a token bucket (a keyframe is 100+ packets that
//!   would otherwise hit the Wi-Fi queue as one burst) and marked DSCP CS5 (WMM video).
//! - Latency is bounded: the socket buffer is small and packets that waited too
//!   long are dropped, followed by a fresh keyframe, so the picture never lags
//!   seconds behind.
//! - Adaptive bitrate: queueing delay lowers the encoder bitrate, a clean link
//!   slowly raises it back to the configured maximum.

use anyhow::{Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Queueing delay that counts as congestion.
const CONGESTED: Duration = Duration::from_millis(40);
/// Packets older than this are dropped (picture would be too late anyway).
const STALE: Duration = Duration::from_millis(200);
const MIN_KBPS: u32 = 1500;

#[derive(Default)]
pub struct Stats {
    pub sent: AtomicU64,
    pub dropped: AtomicU64,
    pub flushes: AtomicU64,
    pub bitrate_kbps: AtomicU32,
}

pub fn force_keyframe(enc: &gst::Element) {
    let s = gst::Structure::builder("GstForceKeyUnit")
        .field("running-time", u64::MAX)
        .field("all-headers", true)
        .field("count", 0u32)
        .build();
    enc.send_event(gst::event::CustomUpstream::new(s));
}

struct RateControl {
    enc: gst::Element,
    max: u32,
    cur: u32,
    last_change: Instant,
    last_congestion: Instant,
}

impl RateControl {
    fn set(&mut self, kbps: u32, stats: &Stats) {
        let kbps = kbps.clamp(MIN_KBPS, self.max);
        if kbps != self.cur {
            self.cur = kbps;
            self.enc.set_property("bitrate", kbps);
            if self.enc.find_property("cpb-size").is_some() {
                self.enc.set_property("cpb-size", kbps / 4);
            }
            stats.bitrate_kbps.store(kbps, Ordering::Relaxed);
        }
        self.last_change = Instant::now();
    }

    fn congestion(&mut self, severe: bool, stats: &Stats) {
        let now = Instant::now();
        self.last_congestion = now;
        if severe {
            self.set(self.cur * 6 / 10, stats);
        } else if now - self.last_change > Duration::from_millis(700) {
            self.set(self.cur * 8 / 10, stats);
        }
    }

    fn tick(&mut self, stats: &Stats) {
        let now = Instant::now();
        if self.cur < self.max && now - self.last_congestion > Duration::from_secs(4) && now - self.last_change > Duration::from_secs(2) {
            self.set((self.cur * 11 / 10).max(self.cur + 250), stats);
        }
    }
}

pub fn attach(appsink: &gst_app::AppSink, enc: gst::Element, dest: SocketAddr, bind_port: u16, bitrate_kbps: u32) -> Result<Arc<Stats>> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_reuse_address(true)?;
    // Small buffer: when the link can't keep up we want to notice (send blocks)
    // instead of silently queueing seconds of video in the kernel.
    let _ = sock.set_send_buffer_size(128 << 10);
    let _ = sock.set_tos_v4(0xa0); // CS5 -> WMM video access category
    sock.bind(&SockAddr::from(SocketAddr::from(([0, 0, 0, 0], bind_port))))
        .or_else(|_| sock.bind(&SockAddr::from(SocketAddr::from(([0, 0, 0, 0], 0)))))
        .context("cannot bind UDP socket")?;
    sock.connect(&SockAddr::from(dest)).context("cannot connect UDP socket")?;
    let sock: std::net::UdpSocket = sock.into();

    let stats = Arc::new(Stats::default());
    stats.bitrate_kbps.store(bitrate_kbps, Ordering::Relaxed);
    let (tx, rx) = mpsc::sync_channel::<(Instant, gst::Buffer)>(3000);

    let st = stats.clone();
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let now = Instant::now();
                let mut push = |b: &gst::BufferRef| {
                    if tx.try_send((now, b.to_owned())).is_err() {
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

    let st = stats.clone();
    let max = bitrate_kbps.max(MIN_KBPS);
    std::thread::Builder::new().name("rtp-pacer".into()).spawn(move || {
        let mut rc = RateControl { enc, max, cur: max, last_change: Instant::now(), last_congestion: Instant::now() };
        let mut next = Instant::now();
        let mut dropping = false;
        loop {
            let (t, buf) = match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(x) => x,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(_) => break,
            };
            let now = Instant::now();
            let delay = now - t;
            if delay > STALE {
                // Too late to be useful: drop everything queued and restart from a keyframe.
                let mut n = 1;
                while rx.try_recv().is_ok() {
                    n += 1;
                }
                st.dropped.fetch_add(n, Ordering::Relaxed);
                st.flushes.fetch_add(1, Ordering::Relaxed);
                if !dropping {
                    rc.congestion(true, &st);
                    force_keyframe(&rc.enc);
                }
                dropping = true;
                next = Instant::now();
                continue;
            }
            dropping = false;
            if delay > CONGESTED {
                rc.congestion(false, &st);
            } else {
                rc.tick(&st);
            }

            let Ok(map) = buf.map_readable() else { continue };
            // Pace at 2.5x the current bitrate (+ audio/overhead), allowing a 16 KB burst.
            let rate = rc.cur as f64 * 1000.0 / 8.0 * 2.5 + 100_000.0;
            let burst = Duration::from_secs_f64(16_384.0 / rate);
            if next + burst < now {
                next = now - burst;
            }
            if next > now {
                std::thread::sleep(next - now);
            }
            // Blocking send: if the kernel buffer is full we wait here, which shows up as delay above.
            if sock.send(&map).is_err() {
                st.dropped.fetch_add(1, Ordering::Relaxed);
            } else {
                st.sent.fetch_add(1, Ordering::Relaxed);
            }
            next += Duration::from_secs_f64(map.len() as f64 / rate);
        }
    })?;
    Ok(stats)
}
