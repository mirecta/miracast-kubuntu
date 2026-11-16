//! Minimal RTSP 1.0 message framing, used by both the WFD source and the test sink.

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub struct Message {
    pub start: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Message {
    pub fn is_response(&self) -> bool {
        self.start.starts_with("RTSP/")
    }
    pub fn status(&self) -> u16 {
        self.start.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0)
    }
    pub fn method(&self) -> &str {
        self.start.split_whitespace().next().unwrap_or("")
    }
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
    pub fn cseq(&self) -> u32 {
        self.header("CSeq").and_then(|v| v.trim().parse().ok()).unwrap_or(0)
    }
}

pub struct Conn {
    rx: mpsc::Receiver<Result<Message>>,
    writer: OwnedWriteHalf,
    next_cseq: u32,
    verbose: bool,
}

impl Conn {
    pub fn new(stream: TcpStream, verbose: bool) -> Self {
        let (r, w) = stream.into_split();
        let (tx, rx) = mpsc::channel(16);
        // The reader runs in its own task so `read()` is cancel-safe (usable in select!).
        tokio::spawn(async move {
            let mut reader = BufReader::new(r);
            loop {
                let m = read_message(&mut reader, verbose).await;
                let stop = m.is_err();
                if tx.send(m).await.is_err() || stop {
                    break;
                }
            }
        });
        Self { rx, writer: w, next_cseq: 1, verbose }
    }

    pub async fn read(&mut self) -> Result<Message> {
        match self.rx.recv().await {
            Some(m) => m,
            None => bail!("connection closed by peer"),
        }
    }

    async fn write(&mut self, start: &str, headers: &[(&str, String)], body: &str) -> Result<()> {
        let mut s = format!("{start}\r\n");
        for (k, v) in headers {
            s.push_str(&format!("{k}: {v}\r\n"));
        }
        if !body.is_empty() {
            s.push_str("Content-Type: text/parameters\r\n");
            s.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        s.push_str("\r\n");
        s.push_str(body);
        if self.verbose {
            eprintln!(">>> {}\n", s.trim_end());
        }
        self.writer.write_all(s.as_bytes()).await?;
        Ok(())
    }

    /// Send a request; returns its CSeq.
    pub async fn request(&mut self, method: &str, url: &str, extra: &[(&str, String)], body: &str) -> Result<u32> {
        let cseq = self.next_cseq;
        self.next_cseq += 1;
        let mut h = vec![("CSeq", cseq.to_string())];
        h.extend(extra.iter().cloned());
        self.write(&format!("{method} {url} RTSP/1.0"), &h, body).await?;
        Ok(cseq)
    }

    pub async fn respond(&mut self, req: &Message, status: &str, extra: &[(&str, String)], body: &str) -> Result<()> {
        let mut h = vec![("CSeq", req.cseq().to_string())];
        h.extend(extra.iter().cloned());
        self.write(&format!("RTSP/1.0 {status}"), &h, body).await
    }
}

async fn read_message(reader: &mut BufReader<OwnedReadHalf>, verbose: bool) -> Result<Message> {
    let mut start = String::new();
    while start.trim().is_empty() {
        start.clear();
        if reader.read_line(&mut start).await? == 0 {
            bail!("connection closed by peer");
        }
    }
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            bail!("connection closed by peer");
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let mut msg = Message { start: start.trim_end().to_string(), headers, body: String::new() };
    let len: usize = msg.header("Content-Length").and_then(|v| v.trim().parse().ok()).unwrap_or(0);
    if len > 0 {
        let mut buf = vec![0u8; len];
        reader.read_exact(&mut buf).await.context("short RTSP body")?;
        msg.body = String::from_utf8_lossy(&buf).into_owned();
    }
    if verbose {
        let h: String = msg.headers.iter().map(|(k, v)| format!("{k}: {v}\n")).collect();
        eprintln!("<<< {}\n{}{}\n", msg.start, h, msg.body);
    }
    Ok(msg)
}
