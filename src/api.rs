//! The machine-readable interface for desktop integrations: what
//! `airpipe status --json`, `list --json` and `config --json` print.
//!
//! UIs (such as a desktop shell plugin) depend on these shapes, versioned by
//! [`VERSION`], which every reply carries as `api`:
//!
//! * Adding a field, or a new value of an enumerated field such as a
//!   speaker's `state`, keeps the version: clients ignore what they do not
//!   know.
//! * Renaming or removing a field, or changing its type, unit or whether it
//!   can be null, needs a new version. `config` is [`Config`] as is, so the
//!   serde names in `config.rs` are part of this interface too.
//!
//! Volumes are percentages of the speaker's full range (like the system
//! volume slider and devices.toml), with one decimal.
//!
//! Text that comes from the network (`name`, `model`, `problem`,
//! `last_error`, `unresponsive`) has control and bidirectional characters
//! removed, but is otherwise untrusted: show it as plain text, and pass
//! speaker names after `--` on a command line.

use serde::{Serialize, Serializer};

use crate::config::Config;

/// Version of this interface.
pub const VERSION: u32 = 1;

/// `airpipe status --json`.
#[derive(Debug, Serialize)]
pub struct Status {
    pub api: u32,
    /// False when airpipe is not running; everything else is then empty.
    pub running: bool,
    pub output: Option<Output>,
    pub speakers: Vec<Speaker>,
}

impl Status {
    pub fn not_running() -> Self {
        Self { api: VERSION, running: false, output: None, speakers: Vec::new() }
    }
}

/// The PipeWire output.
#[derive(Debug, Serialize)]
pub struct Output {
    /// Its name in desktop sound settings ("AirPlay: Kitchen").
    pub name: String,
    /// Its PipeWire node name, e.g. for `pactl set-sink-volume`.
    pub node_name: String,
    /// Whether the system volume controls the speakers.
    pub follow_system: bool,
    /// System volume of the output, once the desktop has reported it; None
    /// as well when `follow_system` is false.
    #[serde(serialize_with = "one_decimal_opt")]
    pub volume: Option<f32>,
    pub muted: bool,
    /// Seconds since the last audible sound; None before the first one.
    pub last_audio_secs: Option<u64>,
}

/// One speaker in use.
#[derive(Debug, Serialize)]
pub struct Speaker {
    /// Its AirPlay device ID, once found on the network.
    pub id: Option<String>,
    pub name: String,
    pub state: SpeakerState,
    /// With `retrying`: seconds until the next attempt.
    pub retry_in_secs: Option<u64>,
    /// Volume it plays at now; None when muted.
    #[serde(serialize_with = "one_decimal_opt")]
    pub volume: Option<f32>,
    #[serde(serialize_with = "one_decimal")]
    pub default_volume: f32,
    /// Its ceiling: `max_volume`, lowered by `--max-volume` if given.
    #[serde(serialize_with = "one_decimal")]
    pub max_volume: f32,
    /// Live trim from `airpipe gain`, in dB.
    pub gain_db: f32,
    pub address: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SpeakerState {
    Idle,
    Connecting,
    Streaming,
    /// Not reachable; see `retry_in_secs`.
    Retrying,
    /// Another device is playing to it.
    InUse,
}

/// `airpipe list --json`.
#[derive(Debug, Serialize)]
pub struct Found {
    pub api: u32,
    pub speakers: Vec<FoundSpeaker>,
    /// Advertised but not resolved (often waking up).
    pub unresponsive: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct FoundSpeaker {
    /// Its AirPlay device ID (keys its devices.toml entry).
    pub id: String,
    pub name: String,
    pub address: String,
    pub model: String,
    pub airplay2: bool,
    pub needs_pairing: bool,
    pub paired: bool,
    pub needs_password: bool,
    /// Why it cannot be used right now (not answering, not reachable...).
    pub problem: Option<String>,
}

/// `airpipe config --json`.
#[derive(Debug, Serialize)]
pub struct Settings<'a> {
    pub api: u32,
    pub config_file: String,
    pub devices_file: String,
    /// config.toml, with defaults filled in.
    pub config: &'a Config,
    /// The latency in use, in ms: the profile's, or `latency_ms`.
    pub effective_latency_ms: u32,
    /// Every speaker recorded in devices.toml.
    pub devices: Vec<KnownSpeaker>,
}

/// A speaker's entry in devices.toml, with its settings as airpipe applies them.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct KnownSpeaker {
    /// Its AirPlay device ID, which keys the entry.
    pub id: String,
    pub name: String,
    pub model: String,
    #[serde(serialize_with = "one_decimal")]
    pub default_volume: f32,
    #[serde(serialize_with = "one_decimal")]
    pub max_volume: f32,
    pub latency_offset_ms: i32,
    pub paired: bool,
}

/// A slider position (0.0..=1.0) as a percentage.
pub fn percent(level: f32) -> f32 {
    level * 100.0
}

/// Percentages go out with one decimal, so a UI shows 60, not 60.000004.
fn one_decimal<S: Serializer>(value: &f32, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_f32((value * 10.0).round() / 10.0)
}

fn one_decimal_opt<S: Serializer>(value: &Option<f32>, s: S) -> Result<S::Ok, S::Error> {
    match value {
        Some(v) => one_decimal(v, s),
        None => s.serialize_none(),
    }
}

/// Serializes a reply. These types always serialize (no maps with
/// non-string keys, no failing `Serialize` impls).
pub fn to_json<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("api types always serialize")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_shapes_are_stable() {
        assert_eq!(to_json(&Status::not_running()), r#"{"api":1,"running":false,"output":null,"speakers":[]}"#);
        let cfg = Config::parse("[audio]\nlatency = \"low-latency\"\n").unwrap();
        let settings = Settings {
            api: VERSION,
            config_file: "c".into(),
            devices_file: "d".into(),
            config: &cfg,
            effective_latency_ms: 500,
            devices: vec![KnownSpeaker {
                id: "50:5B".into(),
                name: "Bureau".into(),
                model: "Phantom".into(),
                default_volume: percent(0.6),
                max_volume: 100.0,
                latency_offset_ms: 0,
                paired: false,
            }],
        };
        let json: serde_json::Value = serde_json::from_str(&to_json(&settings)).unwrap();
        assert_eq!(json["config"]["audio"]["latency"], "low-latency", "the same spelling as config.toml");
        assert_eq!(json["config"]["audio"]["latency_ms"], serde_json::Value::Null);
        assert_eq!(json["config"]["volume"]["start"], "device-default");
        assert_eq!(json["config"]["output"]["speakers"], serde_json::json!([]));
        assert_eq!(json["devices"][0]["default_volume"], 60.0, "one decimal, not 60.000004");
        assert_eq!(to_json(&SpeakerState::InUse), r#""in-use""#);
    }
}
