//! Global settings: `$XDG_CONFIG_HOME/airpipe/config.toml`.
//!
//! The file is written as a commented template the first time airpipe runs.
//! It belongs to the user: airpipe only changes it when asked to
//! (`airpipe use`, `add`, `remove` for the speakers, `airpipe config set` and
//! `unset` for one setting), editing it in place so comments and everything
//! else are kept. Every key is optional; missing keys take the defaults
//! below. Unknown keys are rejected so that typos do not silently do nothing.
//!
//! Precedence: command-line flags > config.toml > built-in defaults.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Allowed values for an explicit `latency_ms` (also enforced by the CLI).
/// Below ~250 ms, sound cuts out unless the network is excellent, and some
/// speakers ignore the request; that is the user's call to make.
pub const LATENCY_MS_RANGE: std::ops::RangeInclusive<u32> = 1..=10_000;

/// The buffer AirPlay senders normally require (OwnTone, Apple: 250 ms).
/// Announced as `latencyMin`, unless the chosen latency is lower. A negative
/// per-speaker offset never takes a speaker below it (or below the latency
/// itself, if that is lower).
pub const MIN_SPEAKER_BUFFER_MS: u32 = 250;

/// Allowed per-speaker `latency_offset_ms` (devices.toml). Negative offsets
/// are further limited by the engine (see [`MIN_SPEAKER_BUFFER_MS`]).
pub const LATENCY_OFFSET_MS_RANGE: std::ops::RangeInclusive<i32> = -1000..=1000;

/// Longest `output.node_name` and `network.client_name`.
const MAX_NAME_CHARS: usize = 64;

/// Our own cushion against PipeWire's bursty delivery when `latency_ms` is
/// below the low-latency preset: about one graph quantum (23 ms), so it adds
/// as little delay as possible.
const SMALL_JITTER_FRAMES: usize = 1024;

/// Buffering presets. Audio is lossless at every setting; latency only
/// trades responsiveness against robustness to network hiccups.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, clap::ValueEnum)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum StartVolume {
    /// Each speaker's `default_volume` from devices.toml (safe, predictable).
    DeviceDefault,
    /// Whatever the desktop remembered from last time.
    Last,
}

/// All of config.toml. Also printed by `config --json` (`api.rs`), so its
/// field names and their serde spelling are part of that interface.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub output: OutputConfig,
    pub audio: AudioConfig,
    pub volume: VolumeConfig,
    pub network: NetworkConfig,
    pub metadata: MetadataConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct OutputConfig {
    /// Speakers airpipe plays to (set with `airpipe use`).
    pub speakers: Vec<String>,
    /// PipeWire node name of the output.
    pub node_name: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AudioConfig {
    pub latency: LatencyProfile,
    /// Exact latency in milliseconds; overrides the profile when set.
    pub latency_ms: Option<u32>,
    /// Release speakers after this many seconds without audio; 0 keeps them
    /// connected.
    pub idle_timeout_secs: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct VolumeConfig {
    /// Follow the system volume of the AirPlay output. When false, every
    /// speaker plays at its `default_volume` from devices.toml.
    pub follow_system: bool,
    pub start: StartVolume,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct NetworkConfig {
    pub timing_port: u16,
    pub control_port: u16,
    /// Source name shown by speakers and their apps.
    pub client_name: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
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
# Speakers to play to, e.g. ["Bureau"], or ["Kitchen", "Living Room"] for a
# synchronized group. `airpipe use <speaker>...` sets this for you; after
# editing by hand, run `airpipe reload`.
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
# Exact latency in milliseconds (1-10000); overrides `latency` when set.
# Below ~250 sound may cut out unless your network is excellent, and some
# speakers do not go that low.
# latency_ms = 1500
# Release speakers after this many seconds without audio, so other devices
# (phones, other computers) can use them. 0 keeps them connected once sound
# has played: no reconnection delay after a pause, but the speakers stay
# awake, airpipe keeps streaming silence (Wi-Fi, a little battery), and
# other devices have to take them over.
idle_timeout_secs = 30

[volume]
# Follow the system volume of the AirPlay output (volume keys, desktop
# slider, any mixer). The slider covers each speaker's whole range: 40% is
# 40% of what the speaker can do. When false, each speaker plays at its fixed
# default_volume from devices.toml.
follow_system = true
# Volume when airpipe starts:
#   "device-default"  each speaker's default_volume (devices.toml): low for
#                     powerful speakers, higher for quiet ones
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
        match self.audio.latency_ms {
            Some(ms) if ms < LatencyProfile::LowLatency.parameters().0 => (ms, SMALL_JITTER_FRAMES),
            Some(ms) => (ms, jitter),
            None => (profile_ms, jitter),
        }
    }

    /// How long without audio before speakers are released; None keeps them
    /// connected (`idle_timeout_secs = 0`).
    pub fn idle_timeout(&self) -> Option<Duration> {
        (self.audio.idle_timeout_secs > 0).then(|| Duration::from_secs(self.audio.idle_timeout_secs))
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
        // Both end up in PipeWire properties and RTSP headers.
        for (key, name) in
            [("output.node_name", &self.output.node_name), ("network.client_name", &self.network.client_name)]
        {
            if name.trim().is_empty() || name.chars().count() > MAX_NAME_CHARS || name.chars().any(char::is_control) {
                bail!("{key} must be 1 to {MAX_NAME_CHARS} characters, without control characters");
            }
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

/// Settings `airpipe config set` and `unset` may change, and how their values
/// are written. `output.speakers` has its own commands (`use`, `add`, `remove`).
const SETTABLE: [(&str, Kind); 10] = [
    ("output.node_name", Kind::Text),
    ("audio.latency", Kind::Text),
    ("audio.latency_ms", Kind::Integer),
    ("audio.idle_timeout_secs", Kind::Integer),
    ("volume.follow_system", Kind::Bool),
    ("volume.start", Kind::Text),
    ("network.timing_port", Kind::Integer),
    ("network.control_port", Kind::Integer),
    ("network.client_name", Kind::Text),
    ("metadata.enabled", Kind::Bool),
];

/// The TOML type of a settable value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Text,
    Integer,
    Bool,
}

/// Sets `output.speakers`, keeping the rest of the file (and its comments)
/// as it is. Creates the file from the template if needed.
pub fn save_speakers(path: &Path, speakers: &[String]) -> Result<()> {
    edit_file(path, |text| with_speakers(text, speakers))
}

/// Sets one setting (or, with `None`, removes it so the default applies),
/// keeping the rest of the file as it is. Creates the file if needed.
pub fn save_setting(path: &Path, key: &str, value: Option<&str>) -> Result<()> {
    edit_file(path, |text| with_setting(text, key, value))
}

fn edit_file(path: &Path, edit: impl FnOnce(&str) -> Result<String>) -> Result<()> {
    let _lock = crate::store::edit_lock(path.parent().unwrap_or(Path::new(".")))?;
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => TEMPLATE.to_string(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let new = edit(&text).with_context(|| format!("not saved to {}", path.display()))?;
    crate::store::write_private(path, &new)
}

/// `text` with `output.speakers` replaced; fails if the result is not a valid config.
fn with_speakers(text: &str, speakers: &[String]) -> Result<String> {
    let list: toml_edit::Array = speakers.iter().map(String::as_str).collect();
    edit_text(text, "output", "speakers", Some(toml_edit::value(list)))
}

/// `text` with one setting changed (`None` removes it); fails if the key is
/// not settable, the value has the wrong type, or the result is not a valid
/// config (out of range, unknown latency profile...).
fn with_setting(text: &str, key: &str, value: Option<&str>) -> Result<String> {
    let Some(&(_, kind)) = SETTABLE.iter().find(|(k, _)| *k == key) else {
        let keys: Vec<_> = SETTABLE.iter().map(|(k, _)| *k).collect();
        bail!("{key:?} cannot be set this way; settable: {}", keys.join(", "));
    };
    let (table, name) = key.split_once('.').expect("settable keys are table.key");
    let item = match (value, kind) {
        (None, _) => None,
        (Some(v), Kind::Text) => Some(toml_edit::value(v)),
        (Some(v), Kind::Integer) => {
            Some(toml_edit::value(v.trim().parse::<i64>().with_context(|| format!("{key} must be a whole number"))?))
        }
        (Some(v), Kind::Bool) => Some(toml_edit::value(match v.trim() {
            "true" => true,
            "false" => false,
            _ => bail!("{key} must be true or false"),
        })),
    };
    edit_text(text, table, name, item)
}

/// `text` with `[table] key` replaced by `item` (or removed), keeping
/// comments; fails if the result is not a valid config.
fn edit_text(text: &str, table: &str, key: &str, item: Option<toml_edit::Item>) -> Result<String> {
    let mut doc: toml_edit::DocumentMut = text.parse()?;
    match item {
        Some(item) => {
            let t = doc.entry(table).or_insert(toml_edit::table());
            let Some(t) = t.as_table_like_mut() else { bail!("[{table}] is not a table") };
            replace_value(t, key, item);
        }
        None => remove_keeping_comments(&mut doc, table, key),
    }
    let new = doc.to_string();
    Config::parse(&new)?;
    Ok(new)
}

/// Sets `key` in `t`. An existing key only gets a new value, so the comment
/// lines above it and a comment after it on the same line stay in place.
pub(crate) fn replace_value(t: &mut dyn toml_edit::TableLike, key: &str, item: toml_edit::Item) {
    match t.get_mut(key) {
        Some(old) => {
            let decor = old.as_value().map(|v| v.decor().clone());
            *old = item;
            if let (Some(decor), Some(v)) = (decor, old.as_value_mut()) {
                *v.decor_mut() = decor;
            }
        }
        None => {
            t.insert(key, item);
        }
    }
}

/// Removes `[table] key` (nothing to do if absent). The comment lines above
/// a key belong to it in toml_edit, and they usually document the setting
/// (the template documents every key that way), so they move to the next
/// key, or before the next table, or to the end of the file.
fn remove_keeping_comments(doc: &mut toml_edit::DocumentMut, table: &str, key: &str) {
    let Some(item) = doc.get_mut(table) else { return };
    if !item.is_table() {
        // An inline table (`audio = { ... }`) holds no comment lines.
        if let Some(t) = item.as_table_like_mut() {
            t.remove(key);
        }
        return;
    }
    let Some(t) = item.as_table_mut() else { return };
    let keys: Vec<String> = t.iter().map(|(k, _)| k.to_string()).collect();
    let Some(pos) = keys.iter().position(|k| k == key) else { return };
    let prefix = |decor: &toml_edit::Decor| decor.prefix().and_then(|p| p.as_str()).unwrap_or("").to_string();
    let comments = t.key(key).map(|k| prefix(k.leaf_decor())).unwrap_or_default();
    t.remove(key);
    if comments.trim().is_empty() {
        return;
    }
    if let Some(next) = keys.get(pos + 1)
        && let Some(mut k) = t.key_mut(next)
    {
        let own = prefix(k.leaf_decor());
        k.leaf_decor_mut().set_prefix(comments + &own);
        return;
    }
    let here = t.position();
    let mut next: Option<&mut toml_edit::Table> = None;
    for (_, item) in doc.as_table_mut().iter_mut() {
        if let Some(other) = item.as_table_mut()
            && other.position() > here
            && next.as_ref().is_none_or(|n| other.position() < n.position())
        {
            next = Some(other);
        }
    }
    match next {
        Some(n) => {
            let own = prefix(n.decor());
            n.decor_mut().set_prefix(comments + &own);
        }
        None => {
            let trailing = doc.trailing().as_str().unwrap_or("").to_string();
            doc.set_trailing(trailing + &comments);
        }
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
        // Very low latencies also get a small cushion of our own.
        let cfg = Config::parse("[audio]\nlatency_ms = 5\n").unwrap();
        assert_eq!(cfg.latency(), (5, SMALL_JITTER_FRAMES));
    }

    #[test]
    fn typos_and_bad_values_are_rejected() {
        assert!(Config::parse("[audio]\nlatnecy = \"robust\"\n").is_err());
        assert!(Config::parse("[audio]\nlatency = \"fast\"\n").is_err());
        assert!(Config::parse("[audio]\nlatency_ms = 0\n").is_err());
        assert!(Config::parse("[audio]\nlatency_ms = 10001\n").is_err());
        assert!(Config::parse("[network]\ntiming_port = 7000\ncontrol_port = 7000\n").is_err());
    }

    #[test]
    fn saving_speakers_keeps_the_rest_of_the_file() {
        let text = TEMPLATE.replace("latency = \"balanced\"", "latency = \"robust\" # my wifi is bad");
        let new = with_speakers(&text, &["Kitchen".into(), "Living Room".into()]).unwrap();
        assert!(new.contains("speakers = [\"Kitchen\", \"Living Room\"]"), "{new}");
        assert!(new.contains("# my wifi is bad"), "{new}");
        assert!(new.contains("# synchronized group"), "{new}");
        let cfg = Config::parse(&new).unwrap();
        assert_eq!(cfg.output.speakers, vec!["Kitchen", "Living Room"]);
        assert_eq!(cfg.audio.latency, LatencyProfile::Robust);
        // No [output] table yet, then back to none.
        let new = with_speakers("[audio]\nidle_timeout_secs = 5\n", &["Bureau".into()]).unwrap();
        assert_eq!(Config::parse(&new).unwrap().output.speakers, vec!["Bureau"]);
        assert!(Config::parse(&with_speakers(&new, &[]).unwrap()).unwrap().output.speakers.is_empty());
        assert!(with_speakers("[audio]\nlatnecy = 1\n", &[]).is_err(), "never writes an invalid file");
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

    #[test]
    fn zero_idle_timeout_keeps_speakers_connected() {
        assert_eq!(Config::default().idle_timeout(), Some(Duration::from_secs(30)));
        let cfg = Config::parse("[audio]\nidle_timeout_secs = 0\n").unwrap();
        assert_eq!(cfg.idle_timeout(), None);
    }

    #[test]
    fn settings_are_changed_in_place_and_checked() {
        let text = TEMPLATE.replace("latency = \"balanced\"", "latency = \"balanced\" # my wifi");
        let new = with_setting(&text, "audio.latency", Some("robust")).unwrap();
        assert!(new.contains("latency = \"robust\" # my wifi"), "{new}");
        assert!(new.contains("# Buffering vs. responsiveness"), "the template's comments stay");
        let cfg = Config::parse(&new).unwrap();
        assert_eq!(cfg.audio.latency, LatencyProfile::Robust);

        let new = with_setting(&new, "audio.latency_ms", Some("250")).unwrap();
        assert_eq!(Config::parse(&new).unwrap().latency().0, 250);
        let new = with_setting(&new, "audio.latency_ms", None).unwrap();
        assert_eq!(Config::parse(&new).unwrap().audio.latency_ms, None, "unset brings the default back");
        let new = with_setting(&new, "volume.follow_system", Some("false")).unwrap();
        assert!(!Config::parse(&new).unwrap().volume.follow_system);
        let new = with_setting("", "metadata.enabled", Some("false")).unwrap();
        assert!(!Config::parse(&new).unwrap().metadata.enabled, "a missing table is created");

        // Refused: wrong type, unknown value, out of range, conflicting, not settable.
        assert!(with_setting(TEMPLATE, "volume.follow_system", Some("maybe")).is_err());
        assert!(with_setting(TEMPLATE, "audio.latency_ms", Some("fast")).is_err());
        assert!(with_setting(TEMPLATE, "audio.latency", Some("fast")).is_err());
        assert!(with_setting(TEMPLATE, "audio.latency_ms", Some("0")).is_err());
        assert!(with_setting(TEMPLATE, "network.timing_port", Some("6112")).is_err());
        assert!(with_setting(TEMPLATE, "output.speakers", Some("Kitchen")).is_err());
        assert!(with_setting(TEMPLATE, "audio.typo", Some("1")).is_err());
        assert!(with_setting(TEMPLATE, "network.client_name", Some("a\r\nX: y")).is_err(), "header injection");
        assert!(with_setting(TEMPLATE, "network.client_name", Some(&"x".repeat(65))).is_err());
        assert!(with_setting(TEMPLATE, "network.client_name", Some("Living room Mac")).is_ok());
    }

    #[test]
    fn unset_keeps_the_comments_that_document_a_setting() {
        // The last key of [audio]: its comments move before [volume].
        let new = with_setting(TEMPLATE, "audio.idle_timeout_secs", None).unwrap();
        assert!(new.contains("# latency_ms = 1500"), "{new}");
        assert!(new.contains("# Release speakers after this many seconds"), "{new}");
        assert!(!new.contains("idle_timeout_secs = 30"), "{new}");
        assert_eq!(Config::parse(&new).unwrap().audio.idle_timeout_secs, 30, "the default applies");
        // A key in the middle: its comments move to the next key.
        let new = with_setting(TEMPLATE, "audio.latency", None).unwrap();
        assert!(new.contains("# Buffering vs. responsiveness"), "{new}");
        assert!(!new.contains("latency = \"balanced\""), "{new}");
        // The last key of the file.
        let new = with_setting(TEMPLATE, "metadata.enabled", None).unwrap();
        assert!(new.contains("# Send now-playing info"), "{new}");
        // Nothing to remove: the text is unchanged (no empty table added).
        assert_eq!(
            with_setting("[audio]\nlatency = \"robust\"\n", "metadata.enabled", None).unwrap(),
            "[audio]\nlatency = \"robust\"\n"
        );
    }
}
