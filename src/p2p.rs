//! Wi-Fi Direct (P2P) discovery and connection through NetworkManager's D-Bus API.

use anyhow::{Context, Result, anyhow, bail};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use zbus::Connection;
use zbus::proxy::CacheProperties;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, Value};

const NM: &str = "org.freedesktop.NetworkManager";
const NM_PATH: &str = "/org/freedesktop/NetworkManager";
const DEVICE_TYPE_WIFI_P2P: u32 = 30;

/// WFD IE subelements we advertise: Device Information (id 0, len 6):
/// device info 0x0090 = WFD source + session available, control port 7236,
/// max throughput 200 Mbit/s.
pub const SOURCE_WFD_IES: [u8; 9] = [0x00, 0x00, 0x06, 0x00, 0x90, 0x1c, 0x44, 0x00, 0xc8];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WfdRole {
    Source,
    PrimarySink,
    SecondarySink,
    DualRole,
}

#[derive(Debug, Clone)]
pub struct Peer {
    pub path: OwnedObjectPath,
    pub name: String,
    pub manufacturer: String,
    pub model: String,
    pub hw_address: String,
    pub strength: u8,
    pub wfd: Option<WfdInfo>,
}

#[derive(Debug, Clone, Copy)]
pub struct WfdInfo {
    pub role: WfdRole,
    pub session_available: bool,
    #[allow(dead_code)]
    pub rtsp_port: u16,
}

impl Peer {
    pub fn is_sink(&self) -> bool {
        matches!(self.wfd.map(|w| w.role), Some(WfdRole::PrimarySink | WfdRole::DualRole))
    }
}

/// Parse WFD subelements (optionally wrapped in a vendor IE header).
pub fn parse_wfd_ies(mut b: &[u8]) -> Option<WfdInfo> {
    if b.len() >= 6 && b[0] == 0xdd && b[2..6] == [0x50, 0x6f, 0x9a, 0x0a] {
        b = &b[6..];
    }
    while b.len() >= 3 {
        let id = b[0];
        let len = u16::from_be_bytes([b[1], b[2]]) as usize;
        let data = b.get(3..3 + len)?;
        if id == 0 && len >= 4 {
            let info = u16::from_be_bytes([data[0], data[1]]);
            let role = match info & 0b11 {
                0 => WfdRole::Source,
                1 => WfdRole::PrimarySink,
                2 => WfdRole::SecondarySink,
                _ => WfdRole::DualRole,
            };
            return Some(WfdInfo {
                role,
                session_available: (info >> 4) & 0b11 == 1,
                rtsp_port: u16::from_be_bytes([data[2], data[3]]),
            });
        }
        b = &b[3 + len..];
    }
    None
}

pub struct P2p {
    conn: Connection,
    device: OwnedObjectPath,
    pub iface: String,
}

async fn proxy<'a>(conn: &Connection, path: impl TryInto<ObjectPath<'a>, Error = zbus::zvariant::Error>, iface: &'a str) -> Result<zbus::Proxy<'a>> {
    Ok(zbus::proxy::Builder::<zbus::Proxy>::new(conn)
        .destination(NM)?
        .path(path)?
        .interface(iface)?
        .cache_properties(CacheProperties::No)
        .build()
        .await?)
}

impl P2p {
    pub async fn new() -> Result<Self> {
        let conn = Connection::system().await.context("cannot connect to the system D-Bus")?;
        let nm = proxy(&conn, NM_PATH, NM).await?;
        let devices: Vec<OwnedObjectPath> = nm.call("GetDevices", &()).await.context("NetworkManager is not running?")?;
        for d in devices {
            let p = proxy(&conn, d.as_str(), "org.freedesktop.NetworkManager.Device").await?;
            if p.get_property::<u32>("DeviceType").await? == DEVICE_TYPE_WIFI_P2P {
                let iface: String = p.get_property("Interface").await?;
                return Ok(Self { conn, device: d.clone(), iface });
            }
        }
        bail!("NetworkManager has no Wi-Fi P2P device (does the Wi-Fi driver support P2P?)")
    }

    async fn dev(&self) -> Result<zbus::Proxy<'_>> {
        proxy(&self.conn, self.device.as_str(), "org.freedesktop.NetworkManager.Device.WifiP2P").await
    }

    pub async fn start_find(&self, timeout_s: i32) -> Result<()> {
        let mut opts: HashMap<&str, Value> = HashMap::new();
        opts.insert("timeout", Value::from(timeout_s.clamp(1, 600)));
        self.dev().await?.call::<_, _, ()>("StartFind", &(opts,)).await.context("StartFind failed")
    }

    pub async fn stop_find(&self) {
        if let Ok(d) = self.dev().await {
            let _ = d.call::<_, _, ()>("StopFind", &()).await;
        }
    }

    pub async fn peers(&self) -> Result<Vec<Peer>> {
        let paths: Vec<OwnedObjectPath> = self.dev().await?.get_property("Peers").await?;
        let mut out = Vec::new();
        for path in paths {
            let p = proxy(&self.conn, path.as_str(), "org.freedesktop.NetworkManager.WifiP2PPeer").await?;
            let ies: Vec<u8> = p.get_property("WfdIEs").await.unwrap_or_default();
            out.push(Peer {
                name: p.get_property("Name").await.unwrap_or_default(),
                manufacturer: p.get_property("Manufacturer").await.unwrap_or_default(),
                model: p.get_property("Model").await.unwrap_or_default(),
                hw_address: p.get_property("HwAddress").await.unwrap_or_default(),
                strength: p.get_property("Strength").await.unwrap_or_default(),
                wfd: parse_wfd_ies(&ies),
                path,
            });
        }
        Ok(out)
    }

    /// Discover peers for `secs` seconds, calling `on_new` for each newly seen peer.
    pub async fn discover(&self, secs: u64, mut stop_when: impl FnMut(&[Peer]) -> bool) -> Result<Vec<Peer>> {
        self.start_find(secs as i32 + 5).await?;
        let deadline = Instant::now() + Duration::from_secs(secs);
        let mut peers = Vec::new();
        while Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(700)).await;
            peers = self.peers().await?;
            if stop_when(&peers) {
                break;
            }
        }
        self.stop_find().await;
        Ok(peers)
    }

    /// Connect to a peer; returns the active connection path once activated.
    /// The connection is volatile and bound to our D-Bus connection, so NM tears
    /// it down automatically when this process exits.
    pub async fn connect(&self, peer: &Peer) -> Result<OwnedObjectPath> {
        let mut con: HashMap<&str, Value> = HashMap::new();
        con.insert("id", Value::from("Miracast (miracast-rs)"));
        con.insert("type", Value::from("wifi-p2p"));
        con.insert("autoconnect", Value::from(false));
        let mut p2p: HashMap<&str, Value> = HashMap::new();
        p2p.insert("peer", Value::from(peer.hw_address.as_str()));
        p2p.insert("wfd-ies", Value::from(SOURCE_WFD_IES.to_vec()));
        let mut ip4: HashMap<&str, Value> = HashMap::new();
        ip4.insert("method", Value::from("auto"));
        let mut ip6: HashMap<&str, Value> = HashMap::new();
        ip6.insert("method", Value::from("disabled"));
        let mut settings: HashMap<&str, HashMap<&str, Value>> = HashMap::new();
        settings.insert("connection", con);
        settings.insert("wifi-p2p", p2p);
        settings.insert("ipv4", ip4);
        settings.insert("ipv6", ip6);
        let mut opts: HashMap<&str, Value> = HashMap::new();
        opts.insert("persist", Value::from("volatile"));
        opts.insert("bind-activation", Value::from("dbus-client"));

        let nm = proxy(&self.conn, NM_PATH, NM).await?;
        let (_conn_path, active, _res): (OwnedObjectPath, OwnedObjectPath, HashMap<String, zbus::zvariant::OwnedValue>) = nm
            .call("AddAndActivateConnection2", &(settings, &self.device, &peer.path, opts))
            .await
            .context("AddAndActivateConnection2 failed")?;

        let ac = proxy(&self.conn, active.as_str(), "org.freedesktop.NetworkManager.Connection.Active").await?;
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let state: u32 = ac.get_property("State").await.map_err(|_| anyhow!("P2P connection was removed (rejected on the TV?)"))?;
            match state {
                2 => return Ok(active),
                3 | 4 => bail!("P2P connection failed (rejected on the TV, or timed out)"),
                _ => {}
            }
            if Instant::now() > deadline {
                bail!("timed out waiting for the P2P connection");
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Interface name of the P2P group (e.g. p2p-wlp4s0-0) and our IPv4 address there.
    pub async fn group_info(&self, active: &OwnedObjectPath) -> Result<(String, Option<String>)> {
        let ac = proxy(&self.conn, active.as_str(), "org.freedesktop.NetworkManager.Connection.Active").await?;
        let devs: Vec<OwnedObjectPath> = ac.get_property("Devices").await?;
        let mut iface = String::new();
        if let Some(d) = devs.first() {
            let p = proxy(&self.conn, d.as_str(), "org.freedesktop.NetworkManager.Device").await?;
            iface = p.get_property("IpInterface").await.unwrap_or_default();
        }
        let ip4: OwnedObjectPath = ac.get_property("Ip4Config").await?;
        let mut addr = None;
        if ip4.as_str() != "/" {
            let p = proxy(&self.conn, ip4.as_str(), "org.freedesktop.NetworkManager.IP4Config").await?;
            let data: Vec<HashMap<String, zbus::zvariant::OwnedValue>> = p.get_property("AddressData").await.unwrap_or_default();
            addr = data.first().and_then(|m| m.get("address")).and_then(|v| String::try_from(v.clone()).ok());
        }
        Ok((iface, addr))
    }

    pub async fn disconnect(&self, active: &OwnedObjectPath) {
        if let Ok(nm) = proxy(&self.conn, NM_PATH, NM).await {
            let _ = nm.call::<_, _, ()>("DeactivateConnection", &(active,)).await;
        }
    }
}
