//! GStreamer pipeline: PipeWire screen -> H.264 (+AAC) -> MPEG-TS -> RTP/UDP (WFD stream).

use crate::wfd::{Audio, Negotiated};
use anyhow::{Context, Result, anyhow};
use gstreamer as gst;
use gstreamer::prelude::*;

#[derive(Debug, Clone, Copy, PartialEq, clap::ValueEnum)]
pub enum Encoder {
    Auto,
    Va,
    X264,
}

pub enum VideoInput {
    /// `capture_ts`: use KWin's capture timestamps (reliable for real outputs) instead of arrival time
    PipeWire { node: u32, capture_ts: bool },
    TestPattern,
}

pub struct Config<'a> {
    pub input: VideoInput,
    pub audio_device: Option<&'a str>,
    pub neg: &'a Negotiated,
    pub bitrate_kbps: u32,
    pub encoder: Encoder,
    pub host: String,
    pub port: u16,
    pub bind_port: u16,
}

pub fn has(name: &str) -> bool {
    gst::ElementFactory::find(name).is_some()
}

pub fn describe(c: &Config) -> Result<String> {
    let m = c.neg.mode;
    let (w, h, fps) = (m.width, m.height, m.fps);
    let src = match c.input {
        VideoInput::PipeWire { node, capture_ts } => {
            format!(
                "pipewiresrc path={node} {} ! video/x-raw,max-framerate={fps}/1 ! queue leaky=downstream max-size-buffers=2",
                // KWin limited to our fps; capture timestamps make backlog measurable (mirror)
                std::env::var("MIRACAST_PW")
                    .unwrap_or_else(|_| format!("do-timestamp={} keepalive-time=100 always-copy=true", !capture_ts))
            )
        }
        VideoInput::TestPattern => format!(
            "videotestsrc is-live=true pattern={} ! video/x-raw,width={w},height={h}",
            std::env::var("MIRACAST_TEST_PATTERN").unwrap_or_else(|_| "ball".into())
        ),
    };
    let use_va = match c.encoder {
        Encoder::Va => true,
        Encoder::X264 => false,
        Encoder::Auto => has("vah264enc") && has("vapostproc"),
    };
    let high = c.neg.profile == 0x02;
    let profile = if high { "high" } else { "constrained-baseline" };
    let kbps = c.bitrate_kbps;
    // CPB of ~250 ms keeps keyframes from being huge bursts
    let cpb = kbps / 4;
    let enc = if use_va {
        format!(
            "vapostproc add-borders=true ! video/x-raw(memory:VAMemory),format=NV12,width={w},height={h} ! \
             vah264enc name=venc rate-control=cbr bitrate={kbps} cpb-size={cpb} key-int-max={fps} b-frames=0 cabac={high}"
        )
    } else {
        if !has("x264enc") {
            return Err(anyhow!("no H.264 encoder found (install gstreamer1.0-plugins-ugly for x264 or gstreamer1.0-plugins-bad for VA-API)"));
        }
        format!(
            "videoconvert n-threads=4 ! videoscale add-borders=true ! video/x-raw,format=I420,width={w},height={h} ! \
             x264enc name=venc tune=zerolatency speed-preset=ultrafast bitrate={kbps} vbv-buf-capacity=300 key-int-max={fps} bframes=0"
        )
    };
    let mut s = format!(
        "{src} ! videorate ! video/x-raw,framerate={fps}/1 ! {enc} ! \
         video/x-h264,profile={profile},stream-format=byte-stream,alignment=au ! h264parse config-interval=-1 ! \
         queue max-size-time=100000000 ! mux.sink_4113 \
         mpegtsmux name=mux alignment=7 ! rtpmp2tpay pt=33 mtu=1400 ! \
         appsink name=rtpout sync=false async=false max-buffers=0 emit-signals=false"
    );
    if c.neg.audio == Audio::Aac {
        let dev = c.audio_device.unwrap_or("@DEFAULT_MONITOR@");
        s.push_str(&format!(
            " pulsesrc device={dev} do-timestamp=true provide-clock=false buffer-time=40000 latency-time=10000 ! \
             audioconvert ! audioresample ! audio/x-raw,rate=48000,channels=2 ! voaacenc bitrate=128000 ! \
             aacparse ! audio/mpeg,mpegversion=4,stream-format=adts ! queue max-size-time=100000000 ! mux.sink_4352"
        ));
    }
    Ok(s)
}

pub struct Stream {
    pub pipeline: gst::Pipeline,
    pub stats: std::sync::Arc<crate::sender::Stats>,
}

impl Stream {
    pub fn new(c: &Config, verbose: bool) -> Result<Self> {
        let desc = describe(c)?;
        if verbose {
            eprintln!("pipeline: {desc}");
        }
        let pipeline = gst::parse::launch(&desc)
            .context("cannot build GStreamer pipeline")?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow!("not a pipeline"))?;
        pipeline.use_clock(Some(&gst::SystemClock::obtain()));
        let appsink = pipeline
            .by_name("rtpout")
            .and_then(|e| e.downcast::<gstreamer_app::AppSink>().ok())
            .ok_or_else(|| anyhow!("appsink missing"))?;
        let dest = std::net::SocketAddr::new(c.host.parse().context("bad sink IP")?, c.port);
        let enc = pipeline.by_name("venc").ok_or_else(|| anyhow!("encoder missing"))?;
        let drop_late = matches!(c.input, VideoInput::PipeWire { capture_ts: true, .. });
        let stats = crate::sender::attach(&appsink, enc, dest, c.bind_port, c.bitrate_kbps, drop_late)?;
        Ok(Self { pipeline, stats })
    }

    pub fn play(&self) -> Result<()> {
        self.pipeline.set_state(gst::State::Playing).context("cannot start pipeline")?;
        Ok(())
    }

    pub fn pause(&self) {
        let _ = self.pipeline.set_state(gst::State::Paused);
    }

    pub fn stop(&self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }

    /// Ask the encoder for an IDR frame with SPS/PPS.
    pub fn force_keyframe(&self) {
        if let Some(enc) = self.pipeline.by_name("venc") {
            crate::sender::force_keyframe(&enc);
        }
    }

    pub fn bus(&self) -> gst::Bus {
        self.pipeline.bus().expect("pipeline has a bus")
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.stop();
    }
}
