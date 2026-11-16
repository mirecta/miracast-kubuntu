//! Screen source via KWin's `zkde_screencast_unstable_v1` protocol.
//!
//! - `VirtualOutput`: KWin creates a brand-new monitor (shows up in Display
//!   Settings, you can drag windows onto it) and streams it over PipeWire.
//! - `Mirror`: streams an existing output.
//!
//! KWin only exposes this protocol to executables whose .desktop file lists
//! `X-KDE-Wayland-Interfaces=zkde_screencast_unstable_v1` (see `install`).

use anyhow::{Context, Result, anyhow, bail};
use std::sync::mpsc;
use std::time::Duration;
use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};

#[allow(non_upper_case_globals, non_camel_case_types, dead_code, clippy::all)]
pub mod protocol {
    use wayland_client;
    use wayland_client::protocol::*;
    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/zkde-screencast-unstable-v1.xml");
    }
    use self::__interfaces::*;
    wayland_scanner::generate_client_code!("protocols/zkde-screencast-unstable-v1.xml");
}

use protocol::zkde_screencast_stream_unstable_v1::{self as zstream, ZkdeScreencastStreamUnstableV1};
use protocol::zkde_screencast_unstable_v1::ZkdeScreencastUnstableV1;

const IFACE: &str = "zkde_screencast_unstable_v1";
const POINTER_EMBEDDED: u32 = 2;

#[derive(Debug, Clone)]
pub enum Source {
    VirtualOutput { name: String, width: i32, height: i32 },
    /// Mirror an existing output; `None` = first output, otherwise connector name (e.g. "eDP-1").
    Mirror(Option<String>),
}

/// Handle to a running screencast. The stream (and virtual monitor) lives as
/// long as the background thread, i.e. until the process exits or `stop` is called.
pub struct Screencast {
    pub node_id: u32,
    stop_tx: mpsc::Sender<()>,
}

impl Screencast {
    pub fn stop(&self) {
        let _ = self.stop_tx.send(());
    }
}

impl Drop for Screencast {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Connector names of the current outputs (e.g. ["eDP-1", "HDMI-A-1"]).
pub fn list_outputs() -> Result<Vec<String>> {
    let conn = Connection::connect_to_env()?;
    let mut queue = conn.new_event_queue::<State>();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());
    let mut st = State::default();
    queue.roundtrip(&mut st)?;
    queue.roundtrip(&mut st)?;
    Ok(st.outputs.into_iter().filter_map(|(_, n)| n).filter(|n| !n.starts_with("Virtual-")).collect())
}

/// Write a .desktop file so KWin grants `exe` the screencast protocol.
/// `menu_name = Some(..)` also makes it visible in the application menu.
pub fn install_desktop_file(exe: &std::path::Path, id: &str, menu_name: Option<&str>) -> Result<std::path::PathBuf> {
    let dir = std::path::PathBuf::from(std::env::var("HOME")?).join(".local/share/applications");
    std::fs::create_dir_all(&dir)?;
    let file = dir.join(format!("{id}.desktop"));
    let (name, nodisplay, terminal) = match menu_name {
        Some(n) => (n, "false", "false"),
        None => ("Miracast (CLI)", "true", "true"),
    };
    std::fs::write(
        &file,
        format!(
            "[Desktop Entry]\nType=Application\nName={name}\nComment=Use a Miracast TV as a monitor\n\
             Exec={}\nIcon=video-display\nTerminal={terminal}\nNoDisplay={nodisplay}\n\
             Categories=Utility;System;\nX-KDE-Wayland-Interfaces=zkde_screencast_unstable_v1\n",
            exe.display()
        ),
    )?;
    let _ = std::process::Command::new("kbuildsycoca6").arg("--noincremental").output();
    Ok(file)
}

#[derive(Default)]
struct State {
    screencast: Option<(u32, u32)>, // (name, version)
    outputs: Vec<(wl_output::WlOutput, Option<String>)>,
    event: Option<Result<u32, String>>,
    closed: bool,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(s: &mut Self, reg: &wl_registry::WlRegistry, ev: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_registry::Event::Global { name, interface, version } = ev {
            if interface == IFACE {
                s.screencast = Some((name, version));
            } else if interface == wl_output::WlOutput::interface().name {
                let o = reg.bind::<wl_output::WlOutput, _, _>(name, version.min(4), qh, ());
                s.outputs.push((o, None));
            }
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(s: &mut Self, o: &wl_output::WlOutput, ev: wl_output::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_output::Event::Name { name } = ev {
            if let Some(e) = s.outputs.iter_mut().find(|(x, _)| x == o) {
                e.1 = Some(name);
            }
        }
    }
}

impl Dispatch<ZkdeScreencastUnstableV1, ()> for State {
    fn event(_: &mut Self, _: &ZkdeScreencastUnstableV1, _: <ZkdeScreencastUnstableV1 as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZkdeScreencastStreamUnstableV1, ()> for State {
    fn event(s: &mut Self, _: &ZkdeScreencastStreamUnstableV1, ev: zstream::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match ev {
            zstream::Event::Created { node } => s.event = Some(Ok(node)),
            zstream::Event::Failed { error } => s.event = Some(Err(error)),
            zstream::Event::Closed => s.closed = true,
        }
    }
}

pub fn start(source: Source) -> Result<Screencast> {
    let (res_tx, res_rx) = mpsc::channel::<Result<u32>>();
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    std::thread::Builder::new()
        .name("wayland".into())
        .spawn(move || {
            if let Err(e) = run(source, &res_tx, stop_rx) {
                let _ = res_tx.send(Err(e));
            }
        })?;
    let node_id = res_rx
        .recv_timeout(Duration::from_secs(10))
        .context("KWin did not answer the screencast request")??;
    Ok(Screencast { node_id, stop_tx })
}

fn run(source: Source, res_tx: &mpsc::Sender<Result<u32>>, stop_rx: mpsc::Receiver<()>) -> Result<()> {
    let conn = Connection::connect_to_env().context("cannot connect to the Wayland compositor (is this a Plasma Wayland session?)")?;
    let mut queue = conn.new_event_queue::<State>();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());
    let mut st = State::default();
    queue.roundtrip(&mut st)?;
    queue.roundtrip(&mut st)?; // wl_output names

    let exe = std::fs::read_link("/proc/self/exe").map(|p| p.display().to_string()).unwrap_or_default();
    if st.screencast.is_none() && exe.ends_with(" (deleted)") {
        bail!("this program was updated while running – close it and start it again");
    }
    let (gname, gver) = st.screencast.ok_or_else(|| {
        anyhow!(
            "KWin does not expose {IFACE} to this program.\n\
             Run `miracast install` once (it registers the binary in a .desktop file with \
             X-KDE-Wayland-Interfaces) and start it again from the installed path."
        )
    })?;
    let reg = conn.display().get_registry(&qh, ());
    let sc: ZkdeScreencastUnstableV1 = reg.bind(gname, gver.min(4), &qh, ());

    let stream = match &source {
        Source::VirtualOutput { name, width, height } => {
            if sc.version() < 2 {
                bail!("KWin is too old for virtual outputs");
            }
            sc.stream_virtual_output(name.clone(), *width, *height, 1.0, POINTER_EMBEDDED, &qh, ())
        }
        Source::Mirror(want) => {
            let out = match want {
                None => st.outputs.first(),
                Some(n) => st.outputs.iter().find(|(_, name)| name.as_deref() == Some(n.as_str())),
            }
            .ok_or_else(|| {
                let names: Vec<_> = st.outputs.iter().filter_map(|(_, n)| n.clone()).collect();
                anyhow!("output not found; available: {}", names.join(", "))
            })?;
            sc.stream_output(&out.0, POINTER_EMBEDDED, &qh, ())
        }
    };

    while st.event.is_none() && !st.closed {
        queue.blocking_dispatch(&mut st)?;
    }
    match st.event.take() {
        Some(Ok(node)) => res_tx.send(Ok(node)).ok(),
        Some(Err(e)) => bail!("KWin refused the screencast: {e}"),
        None => bail!("KWin closed the screencast stream"),
    };

    // Keep dispatching until asked to stop (or the compositor closes the stream).
    loop {
        if stop_rx.try_recv().is_ok() || st.closed {
            break;
        }
        conn.flush()?;
        if let Some(guard) = queue.prepare_read() {
            let fd = guard.connection_fd();
            let mut pfd = [rustix_poll(fd)];
            if poll(&mut pfd, 200) {
                guard.read()?;
            }
        }
        queue.dispatch_pending(&mut st)?;
    }
    stream.close();
    sc.destroy();
    conn.flush()?;
    Ok(())
}

// Minimal poll(2) wrapper to wake up periodically and check the stop channel.
use std::os::fd::{AsRawFd, BorrowedFd};

#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

unsafe extern "C" {
    #[link_name = "poll"]
    fn c_poll(fds: *mut PollFd, nfds: u64, timeout: i32) -> i32;
}

fn rustix_poll(fd: BorrowedFd<'_>) -> PollFd {
    PollFd { fd: fd.as_raw_fd(), events: 1 /* POLLIN */, revents: 0 }
}

fn poll(fds: &mut [PollFd], timeout_ms: i32) -> bool {
    unsafe { c_poll(fds.as_mut_ptr(), fds.len() as u64, timeout_ms) > 0 }
}
