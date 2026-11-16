//! Wi-Fi Display capability parsing / format selection (WFD spec 1.0 parameters).

use anyhow::{Result, bail};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VideoMode {
    pub cea_bit: u32,
    pub width: i32,
    pub height: i32,
    pub fps: i32,
    /// WFD level bitmap: 0x01=3.1 0x02=3.2 0x04=4 0x08=4.1 0x10=4.2
    pub level: u8,
}

/// CEA modes we can produce (progressive only), in order of preference.
pub const CEA_MODES: &[VideoMode] = &[
    VideoMode { cea_bit: 8, width: 1920, height: 1080, fps: 60, level: 0x10 },
    VideoMode { cea_bit: 7, width: 1920, height: 1080, fps: 30, level: 0x04 },
    VideoMode { cea_bit: 6, width: 1280, height: 720, fps: 60, level: 0x02 },
    VideoMode { cea_bit: 5, width: 1280, height: 720, fps: 30, level: 0x01 },
    VideoMode { cea_bit: 0, width: 640, height: 480, fps: 60, level: 0x01 },
];

#[derive(Debug, Clone)]
pub struct H264Codec {
    pub profile: u8,
    pub level: u8,
    pub cea: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Audio {
    Aac,
    None,
}

#[derive(Debug, Clone)]
pub struct Negotiated {
    pub mode: VideoMode,
    pub profile: u8, // 0x01 CBP, 0x02 CHP
    pub audio: Audio,
    pub rtp_port: u16,
}

pub fn parse_params(body: &str) -> HashMap<String, String> {
    body.lines()
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect()
}

fn hex(s: &str) -> Option<u32> {
    u32::from_str_radix(s, 16).ok()
}

/// "00 00 02 04 0001FFFF 1FFFFFFF 00000FFF 00 0000 0000 00 none none, 01 ..."
pub fn parse_video_formats(v: &str) -> Vec<H264Codec> {
    let v = v.trim();
    if v.eq_ignore_ascii_case("none") {
        return vec![];
    }
    let mut toks = v.splitn(3, ' ');
    let (_native, _pref, rest) = (toks.next(), toks.next(), toks.next().unwrap_or(""));
    rest.split(',')
        .filter_map(|entry| {
            let t: Vec<&str> = entry.split_whitespace().collect();
            if t.len() < 3 {
                return None;
            }
            Some(H264Codec { profile: hex(t[0])? as u8, level: hex(t[1])? as u8, cea: hex(t[2])? })
        })
        .collect()
}

/// Returns supported audio formats as (codec, mode-bitmap).
pub fn parse_audio_codecs(v: &str) -> Vec<(String, u32)> {
    if v.trim().eq_ignore_ascii_case("none") {
        return vec![];
    }
    v.split(',')
        .filter_map(|e| {
            let t: Vec<&str> = e.split_whitespace().collect();
            Some((t.first()?.to_ascii_uppercase(), hex(t.get(1)?)?))
        })
        .collect()
}

/// "RTP/AVP/UDP;unicast 1028 0 mode=play" -> 1028
pub fn parse_rtp_port(v: &str) -> Option<u16> {
    v.split_whitespace().nth(1)?.parse().ok()
}

pub struct Prefs {
    pub max_height: i32,
    pub max_fps: i32,
    pub audio: bool,
    pub high_profile: bool,
}

pub fn negotiate(params: &HashMap<String, String>, prefs: &Prefs) -> Result<Negotiated> {
    let codecs = parse_video_formats(params.get("wfd_video_formats").map(String::as_str).unwrap_or("none"));
    if codecs.is_empty() {
        bail!("sink reported no supported H.264 formats: {:?}", params.get("wfd_video_formats"));
    }
    let want_profile = if prefs.high_profile { 0x02 } else { 0x01 };
    let mut chosen = None;
    'outer: for mode in CEA_MODES.iter().filter(|m| m.height <= prefs.max_height && m.fps <= prefs.max_fps) {
        for profile in [want_profile, 0x01] {
            for c in &codecs {
                if c.profile & profile != 0 && c.cea & (1 << mode.cea_bit) != 0 && c.level >= mode.level {
                    chosen = Some((*mode, profile));
                    break 'outer;
                }
            }
        }
    }
    // Every WFD sink must support 640x480p60 CBP.
    let (mode, profile) = chosen.unwrap_or((CEA_MODES[CEA_MODES.len() - 1], 0x01));

    let audio = if prefs.audio {
        let ac = parse_audio_codecs(params.get("wfd_audio_codecs").map(String::as_str).unwrap_or("none"));
        if ac.iter().any(|(c, m)| c == "AAC" && m & 1 != 0) { Audio::Aac } else { Audio::None }
    } else {
        Audio::None
    };

    let rtp_port = params.get("wfd_client_rtp_ports").and_then(|v| parse_rtp_port(v)).unwrap_or(1028);
    Ok(Negotiated { mode, profile, audio, rtp_port })
}

impl Negotiated {
    pub fn video_formats_param(&self) -> String {
        format!(
            "00 00 {:02X} {:02X} {:08X} 00000000 00000000 00 0000 0000 00 none none",
            self.profile,
            self.mode.level,
            1u32 << self.mode.cea_bit
        )
    }

    pub fn audio_codecs_param(&self) -> Option<&'static str> {
        match self.audio {
            Audio::Aac => Some("AAC 00000001 00"),
            Audio::None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samsung_like() {
        let body = "wfd_video_formats: 00 00 03 10 0001ffff 1fffffff 00001fff 00 0000 0000 10 none none\r\n\
                    wfd_audio_codecs: LPCM 00000002 00, AAC 00000001 00\r\n\
                    wfd_client_rtp_ports: RTP/AVP/UDP;unicast 19000 0 mode=play\r\n";
        let p = parse_params(body);
        let n = negotiate(&p, &Prefs { max_height: 1080, max_fps: 30, audio: true, high_profile: false }).unwrap();
        assert_eq!(n.mode.width, 1920);
        assert_eq!(n.mode.fps, 30);
        assert_eq!(n.audio, Audio::Aac);
        assert_eq!(n.rtp_port, 19000);
        assert_eq!(n.video_formats_param(), "00 00 01 04 00000080 00000000 00000000 00 0000 0000 00 none none");
    }

    #[test]
    fn limited_sink() {
        let p = parse_params("wfd_video_formats: 40 00 01 01 00000021 00000000 00000000 00 0000 0000 00 none none\nwfd_audio_codecs: none\n");
        let n = negotiate(&p, &Prefs { max_height: 1080, max_fps: 60, audio: true, high_profile: true }).unwrap();
        assert_eq!((n.mode.width, n.mode.fps), (1280, 30));
        assert_eq!(n.profile, 1);
        assert_eq!(n.audio, Audio::None);
    }
}
