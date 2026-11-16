//! Miracast (Wi-Fi Display) source for KDE Plasma on Wayland.

pub mod audio;
pub mod cast;
pub mod fake_sink;
pub mod p2p;
pub mod pipeline;
pub mod rtsp;
pub mod screen;
pub mod sender;
pub mod source;
pub mod wfd;

pub use cast::{AudioMode, CastOpts, Reporter, Status, cast, scan};
pub use tokio_util::sync::CancellationToken;
