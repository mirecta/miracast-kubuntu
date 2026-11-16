# miracast-rs

Miracast (Wi-Fi Display) source for KDE Plasma 6 on Wayland, in Rust.
The TV appears as an **extra monitor** (KWin virtual output) or mirrors an existing screen.

```
cargo install --path .
miracast install            # once: lets KWin give this binary the screencast protocol
miracast scan               # find TVs (TV: Source -> Screen Mirroring)
miracast cast               # first TV found, as an extra 1920x1080 monitor
miracast cast --peer "Miro telka" --fps 60 --bitrate 12000
miracast cast --mirror=eDP-1
miracast cast --audio both|tv|none   # tv = new "Miracast" audio output (default)
```

Pipeline: NetworkManager Wi-Fi P2P (D-Bus) -> RTSP/WFD (M1..M16) on TCP 7236 ->
KWin `zkde_screencast_unstable_v1` -> PipeWire -> VA-API H.264 (x264 fallback) + AAC ->
MPEG-TS -> RTP/UDP.

Testing without a TV: `miracast cast --no-p2p --test-pattern` + `miracast fake-sink --dump out.ts`.
