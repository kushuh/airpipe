//! Global settings: `$XDG_CONFIG_HOME/airpipe/config.toml`.
//!
//! The file is written once, as a commented template, the first time airpipe
//! runs, and never modified by airpipe afterwards: it belongs to the user.
//! Every key is optional; missing keys take the defaults below. Unknown keys
//! are rejected so that typos do not silently do nothing.
//!
//! Precedence: command-line flags > config.toml > built-in defaults.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// Allowed values for an explicit `latency_ms` (also enforced by the CLI).
pub const LATENCY_MS_RANGE: std::ops::RangeInclusive<u32> = 500..=10_000;

/// Every speaker keeps at least this much audio buffered. Sent as AirPlay's
/// `latencyMin` (the 250 ms OwnTone and Apple senders use).
pub const MIN_SPEAKER_BUFFER_MS: u32 = 250;

/// Allowed per-speaker `latency_offset_ms` (devices.toml). The lower bound
/// keeps [`MIN_SPEAKER_BUFFER_MS`] of buffer even at the smallest latency.
pub const LATENCY_OFFSET_MS_RANGE: std::ops::RangeInclusive<i32> =
    -((*LATENCY_MS_RANGE.start() - MIN_SPEAKER_BUFFER_MS) as i32)..=1000;

/// Buffering presets. Audio is lossless at every setting; latency only
/// trades responsiveness against robustness to network hiccups.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum LatencyProfile {
    /// ~0.5 s: snappy, fine on wired or very clean Wi-Fi.
    LowLatency,
    /// ~1 s: good default for most Wi-Fi.
    Balanced,
    /// ~2 s: survives busy or weak Wi-Fi without dropouts.
    Robust,
}

impl LatencyProfile {
    /// (playback latency, jitter cushion) for this profile.
    pub fn parameters(self) -> (u32, usize) {
        match self {
            LatencyProfile::LowLatency => (500, 2048),
            LatencyProfile::Balanced => (1000, 3072),
            LatencyProfile::Robust => (2000, 4096),
        }
    }
}

/// What the PipeWire output's volume is set to when airpipe starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StartVolume {
    /// Each speaker's `default_volume_db` from devices.toml (safe, predictable).
    DeviceDefault,
    /// Whatever the desktop remembered from last time.
    Last,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub output: OutputConfig,
    pub audio: AudioConfig,
    pub volume: VolumeConfig,
    pub network: NetworkConfig,
    pub metadata: MetadataConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OutputConfig {
    /// Speakers used by `airpipe run` when none are given on the command line.
    pub speakers: Vec<String>,
    /// PipeWire node name of the output.
    pub node_name: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AudioConfig {
    pub latency: LatencyProfile,
    /// Exact latency in milliseconds; overrides the profile when set.
    pub latency_ms: Option<u32>,
    /// Release speakers after this many seconds without audio.
    pub idle_timeout_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VolumeConfig {
    /// Follow the system volume of the AirPlay output. When false, every
    /// speaker plays at its `default_volume_db` from devices.toml.
    pub follow_system: bool,
    pub start: StartVolume,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NetworkConfig {
    pub timing_port: u16,
    pub control_port: u16,
    /// Source name shown by speakers and their apps.
    pub client_name: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MetadataConfig {
    /// Send now-playing info and accept speaker remote control (MPRIS).
    pub enabled: bool,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self { speakers: Vec::new(), node_name: "airpipe".into() }
    }
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self { latency: LatencyProfile::Balanced, latency_ms: None, idle_timeout_secs: 30 }
    }
}

impl Default for VolumeConfig {
    fn default() -> Self {
        Self { follow_system: true, start: StartVolume::DeviceDefault }
    }
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self { timing_port: 6111, control_port: 6112, client_name: "airpipe".into() }
    }
}

impl Default for MetadataConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Commented template written on first run. Every value equals the default.
pub const TEMPLATE: &str = r#"# airpipe settings. All keys are optional; the values shown are the defaults.
# Per-speaker settings (volume levels, sync offsets, pairing keys) live in
# devices.toml next to this file.
# Command-line flags override this file. Docs: README.md, "Configuration".

[output]
# Speakers played by `airpipe run` when none are given on the command line,
# e.g. ["Bureau"] or ["Kitchen", "Living Room"] for a synchronized group.
speakers = []
# PipeWire node name of the output.
node_name = "airpipe"

[audio]
# Buffering vs. responsiveness. Audio is lossless at every setting.
#   "low-latency"  ~0.5 s  snappy; best on wired or very clean Wi-Fi
#   "balanced"     ~1 s    good for most Wi-Fi
#   "robust"       ~2 s    no dropouts on busy or weak Wi-Fi
# Video players compensate for the latency automatically.
latency = "balanced"
# Exact latency in milliseconds (500-10000); overrides `latency` when set.
# latency_ms = 1500
# Release speakers after this many seconds without audio, so other devices
# (phones, other computers) can use them.
idle_timeout_secs = 30

[volume]
# Follow the system volume of the AirPlay output (volume keys, desktop
# slider, any mixer). When false, each speaker plays at its fixed
# default_volume_db from devices.toml.
follow_system = true
# Volume when airpipe starts:
#   "device-default"  each speaker's default_volume_db (devices.toml)
#   "last"            whatever the desktop remembered from last time
start = "device-default"

[network]
# UDP ports speakers send clock-sync and retransmit requests to.
# Keep them in sync with your firewall rules.
timing_port = 6111
control_port = 6112
# Source name shown by speakers and their apps.
client_name = "airpipe"

[metadata]
# Send now-playing info (title, artist, artwork, progress) and let speaker
# buttons control the active media player (MPRIS).
enabled = true
"#;

impl Config {
    /// Effective (latency in ms, jitter cushion in frames).
    pub fn latency(&self) -> (u32, usize) {
        let (profile_ms, jitter) = self.audio.latency.parameters();
        (self.audio.latency_ms.unwrap_or(profile_ms), jitter)
    }

    pub fn idle_timeout(&self) -> Duration {
        Duration::from_secs(self.audio.idle_timeout_secs)
    }

    fn validate(&self) -> Result<()> {
        if let Some(ms) = self.audio.latency_ms
            && !LATENCY_MS_RANGE.contains(&ms)
        {
            bail!("audio.latency_ms must be between {} and {}", LATENCY_MS_RANGE.start(), LATENCY_MS_RANGE.end());
        }
        if self.network.timing_port == self.network.control_port {
            bail!("network.timing_port and network.control_port must differ");
        }
        if self.output.node_name.trim().is_empty() {
            bail!("output.node_name must not be empty");
        }
        Ok(())
    }

    pub fn parse(text: &str) -> Result<Self> {
        let cfg: Config = toml::from_str(text)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Loads the config without writing anything; a missing file means defaults.
    pub fn load(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(text) => Self::parse(&text).with_context(|| format!("invalid {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Loads the config, creating the commented template if it is missing.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if !path.exists() {
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
            }
            fs::write(path, TEMPLATE).with_context(|| format!("writing {}", path.display()))?;
        }
        Self::load(path)
    }
}

/// `$XDG_CONFIG_HOME/airpipe`, or `~/.config/airpipe` (XDG Base Directory spec).
pub fn config_dir() -> Result<PathBuf> {
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).filter(|p| p.is_absolute());
    let base = match xdg {
        Some(p) => p,
        None => {
            PathBuf::from(std::env::var_os("HOME").context("neither XDG_CONFIG_HOME nor HOME is set")?).join(".config")
        }
    };
    Ok(base.join("airpipe"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_parses_to_defaults() {
        let cfg = Config::parse(TEMPLATE).unwrap();
        let def = Config::default();
        assert_eq!(cfg.latency(), def.latency());
        assert_eq!(cfg.audio.idle_timeout_secs, def.audio.idle_timeout_secs);
        assert_eq!(cfg.volume.follow_system, def.volume.follow_system);
        assert_eq!(cfg.volume.start, def.volume.start);
        assert_eq!(cfg.network.timing_port, def.network.timing_port);
        assert_eq!(cfg.network.control_port, def.network.control_port);
        assert_eq!(cfg.output.node_name, def.output.node_name);
        assert!(cfg.output.speakers.is_empty());
        assert!(cfg.metadata.enabled);
    }

    #[test]
    fn empty_file_is_all_defaults() {
        let cfg = Config::parse("").unwrap();
        assert_eq!(cfg.latency(), (1000, 3072));
    }

    #[test]
    fn explicit_latency_overrides_profile() {
        let cfg = Config::parse("[audio]\nlatency = \"robust\"\nlatency_ms = 1500\n").unwrap();
        assert_eq!(cfg.latency(), (1500, 4096));
        let cfg = Config::parse("[audio]\nlatency = \"low-latency\"\n").unwrap();
        assert_eq!(cfg.latency().0, 500);
    }

    #[test]
    fn typos_and_bad_values_are_rejected() {
        assert!(Config::parse("[audio]\nlatnecy = \"robust\"\n").is_err());
        assert!(Config::parse("[audio]\nlatency = \"fast\"\n").is_err());
        assert!(Config::parse("[audio]\nlatency_ms = 100\n").is_err());
        assert!(Config::parse("[network]\ntiming_port = 7000\ncontrol_port = 7000\n").is_err());
    }

    #[test]
    fn speakers_and_volume() {
        let cfg = Config::parse(
            "[output]\nspeakers = [\"Kitchen\", \"Living Room\"]\n[volume]\nfollow_system = false\nstart = \"last\"\n",
        )
        .unwrap();
        assert_eq!(cfg.output.speakers, vec!["Kitchen", "Living Room"]);
        assert!(!cfg.volume.follow_system);
        assert_eq!(cfg.volume.start, StartVolume::Last);
    }
}
