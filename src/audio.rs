//! Optional "Miracast TV" virtual audio output (PulseAudio / pipewire-pulse null sink).

use anyhow::{Context, Result, bail};
use std::process::Command;

pub const SINK_NAME: &str = "miracast_tv";

pub struct TvSink {
    module: String,
    previous_default: Option<String>,
}

fn pactl(args: &[&str]) -> Result<String> {
    let out = Command::new("pactl").args(args).output().context("pactl not found")?;
    if !out.status.success() {
        bail!("pactl {:?}: {}", args, String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

impl TvSink {
    /// Create a null sink, make it the default output and return its monitor source name.
    pub fn create(description: &str) -> Result<(Self, String)> {
        let previous_default = pactl(&["get-default-sink"]).ok();
        let props = format!("sink_properties=device.description=\"{}\"", description.replace('"', ""));
        let module = pactl(&["load-module", "module-null-sink", &format!("sink_name={SINK_NAME}"), &props])?;
        let _ = pactl(&["set-default-sink", SINK_NAME]);
        Ok((Self { module, previous_default }, format!("{SINK_NAME}.monitor")))
    }
}

impl Drop for TvSink {
    fn drop(&mut self) {
        if let Some(p) = &self.previous_default {
            if p != SINK_NAME {
                let _ = pactl(&["set-default-sink", p]);
            }
        }
        let _ = pactl(&["unload-module", &self.module]);
    }
}
