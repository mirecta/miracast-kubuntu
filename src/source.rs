//! WFD source side of the RTSP control protocol (messages M1..M16).

use crate::rtsp::{Conn, Message};
use crate::wfd::{self, Negotiated, Prefs};
use anyhow::{Context, Result, bail};
use std::collections::VecDeque;
use std::net::IpAddr;
use std::time::Duration;
use tokio::net::TcpListener;

const URL: &str = "rtsp://localhost/wfd1.0";
pub const SERVER_RTP_PORT: u16 = 19000;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Event {
    Setup { client_port: u16 },
    Play,
    Pause,
    Teardown,
    IdrRequest,
}

pub struct WfdSource {
    conn: Conn,
    pub local_ip: IpAddr,
    pub sink_ip: IpAddr,
    session: String,
    events: VecDeque<Event>,
}

impl WfdSource {
    pub async fn accept(listener: &TcpListener, verbose: bool) -> Result<Self> {
        let (stream, peer) = listener.accept().await?;
        stream.set_nodelay(true)?;
        let local_ip = stream.local_addr()?.ip();
        let session = format!("{:08X}", std::process::id() ^ (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs() as u32));
        Ok(Self { conn: Conn::new(stream, verbose), local_ip, sink_ip: peer.ip(), session, events: VecDeque::new() })
    }

    fn session_hdr(&self) -> (&'static str, String) {
        ("Session", self.session.clone())
    }

    async fn handle_request(&mut self, req: Message) -> Result<()> {
        match req.method() {
            "OPTIONS" => {
                // M2
                let public = "org.wfa.wfd1.0, SETUP, TEARDOWN, PLAY, PAUSE, GET_PARAMETER, SET_PARAMETER".to_string();
                self.conn.respond(&req, "200 OK", &[("Public", public)], "").await?;
            }
            "SETUP" => {
                // M6
                let transport = req.header("Transport").unwrap_or("").to_string();
                let client_port = transport
                    .split(';')
                    .find_map(|p| p.trim().strip_prefix("client_port="))
                    .and_then(|p| p.split('-').next()?.parse().ok())
                    .unwrap_or(0);
                let t = format!("RTP/AVP/UDP;unicast;client_port={client_port};server_port={}-{}", SERVER_RTP_PORT, SERVER_RTP_PORT + 1);
                let s = format!("{};timeout=30", self.session);
                self.conn.respond(&req, "200 OK", &[("Session", s), ("Transport", t)], "").await?;
                self.events.push_back(Event::Setup { client_port });
            }
            "PLAY" => {
                // M7
                let s = self.session_hdr();
                self.conn.respond(&req, "200 OK", &[s], "").await?;
                self.events.push_back(Event::Play);
            }
            "PAUSE" => {
                let s = self.session_hdr();
                self.conn.respond(&req, "200 OK", &[s], "").await?;
                self.events.push_back(Event::Pause);
            }
            "TEARDOWN" => {
                let s = self.session_hdr();
                self.conn.respond(&req, "200 OK", &[s], "").await?;
                self.events.push_back(Event::Teardown);
            }
            "SET_PARAMETER" => {
                self.conn.respond(&req, "200 OK", &[], "").await?;
                if req.body.contains("wfd_idr_request") {
                    self.events.push_back(Event::IdrRequest);
                }
            }
            "GET_PARAMETER" => {
                self.conn.respond(&req, "200 OK", &[], "").await?;
            }
            _ => {
                self.conn.respond(&req, "501 Not Implemented", &[], "").await?;
            }
        }
        Ok(())
    }

    /// Send a request and wait for its response, servicing sink requests meanwhile.
    async fn call(&mut self, method: &str, url: &str, extra: &[(&str, String)], body: &str) -> Result<Message> {
        let cseq = self.conn.request(method, url, extra, body).await?;
        let wait = async {
            loop {
                let m = self.conn.read().await?;
                if m.is_response() {
                    if m.cseq() == cseq {
                        return Ok::<_, anyhow::Error>(m);
                    }
                } else {
                    self.handle_request(m).await?;
                }
            }
        };
        let resp = tokio::time::timeout(Duration::from_secs(15), wait).await.with_context(|| format!("no reply to {method}"))??;
        if resp.status() != 200 {
            bail!("sink answered {method} with {}", resp.start);
        }
        Ok(resp)
    }

    /// M1, M3, M4: capability exchange.
    pub async fn negotiate(&mut self, prefs: &Prefs) -> Result<Negotiated> {
        self.call("OPTIONS", "*", &[("Require", "org.wfa.wfd1.0".into())], "").await.context("M1")?;
        let m3 = self
            .call("GET_PARAMETER", URL, &[], "wfd_video_formats\r\nwfd_audio_codecs\r\nwfd_client_rtp_ports\r\n")
            .await
            .context("M3")?;
        let params = wfd::parse_params(&m3.body);
        let neg = wfd::negotiate(&params, prefs)?;

        let mut body = format!("wfd_video_formats: {}\r\n", neg.video_formats_param());
        if let Some(a) = neg.audio_codecs_param() {
            body.push_str(&format!("wfd_audio_codecs: {a}\r\n"));
        }
        body.push_str(&format!("wfd_presentation_URL: rtsp://{}/wfd1.0/streamid=0 none\r\n", self.local_ip));
        body.push_str(&format!("wfd_client_rtp_ports: RTP/AVP/UDP;unicast {} 0 mode=play\r\n", neg.rtp_port));
        self.call("SET_PARAMETER", URL, &[], &body).await.context("M4")?;
        Ok(neg)
    }

    /// Optional extensions, asked separately so a picky sink can't break M3/M4.
    /// Returns human-readable notes for the log.
    pub async fn try_low_latency(&mut self) -> Vec<String> {
        let mut notes = Vec::new();
        let q = "microsoft_latency_management_capability\r\nmicrosoft_format_change_capability\r\n\
                 microsoft_diagnostics_capability\r\nmicrosoft_cursor\r\nwfd_display_edid\r\n";
        let params = match self.call("GET_PARAMETER", URL, &[], q).await {
            Ok(r) => wfd::parse_params(&r.body),
            Err(e) => {
                notes.push(format!("TV: no extension info ({e})"));
                return notes;
            }
        };
        let mut keys: Vec<_> = params.iter().filter(|(k, _)| k.starts_with("microsoft_")).collect();
        keys.sort();
        notes.push(format!(
            "TV extensions: {}",
            if keys.is_empty() { "none".to_string() } else { keys.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(", ") }
        ));
        let lm = params.get("microsoft_latency_management_capability").map(|v| v.to_ascii_lowercase());
        if lm.as_deref().is_some_and(|v| v != "none") {
            match self.call("SET_PARAMETER", URL, &[], "microsoft_latency_management_capability: low\r\n").await {
                Ok(_) => notes.push("TV low-latency mode requested".into()),
                Err(e) => notes.push(format!("TV refused low-latency mode ({e})")),
            }
        }
        notes
    }

    /// M5 trigger SETUP, then wait for M6 SETUP and M7 PLAY. Returns the sink's RTP port.
    pub async fn setup_and_play(&mut self, default_port: u16) -> Result<u16> {
        self.call("SET_PARAMETER", URL, &[], "wfd_trigger_method: SETUP\r\n").await.context("M5")?;
        let mut port = default_port;
        let wait = async {
            loop {
                match self.next_event().await? {
                    Event::Setup { client_port } if client_port != 0 => port = client_port,
                    Event::Play => return Ok::<_, anyhow::Error>(()),
                    Event::Teardown => bail!("sink tore down the session during setup"),
                    _ => {}
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(20), wait).await.context("sink did not SETUP/PLAY")??;
        Ok(port)
    }

    /// Next event from the sink (cancel-safe).
    pub async fn next_event(&mut self) -> Result<Event> {
        loop {
            if let Some(e) = self.events.pop_front() {
                return Ok(e);
            }
            let m = self.conn.read().await?;
            if !m.is_response() {
                self.handle_request(m).await?;
            }
        }
    }

    /// M16 keep-alive (response is consumed by `next_event`).
    pub async fn keepalive(&mut self) -> Result<()> {
        let s = self.session_hdr();
        self.conn.request("GET_PARAMETER", URL, &[s], "").await?;
        Ok(())
    }

    /// M5 with TEARDOWN trigger: ask the sink to close the session.
    pub async fn trigger_teardown(&mut self) {
        let _ = self.conn.request("SET_PARAMETER", URL, &[], "wfd_trigger_method: TEARDOWN\r\n").await;
    }
}
