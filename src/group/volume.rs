//! Volume rules. Pure functions, so they can be audited and tested alone.
//!
//! AirPlay volume is in dB: -30 is the quietest audible step, 0 is the
//! speaker's **full power**, and -144 means mute. The system volume slider
//! covers a speaker's whole range, so 40% means 40% of what the speaker can
//! do, on every speaker:
//!
//! ```text
//! system volume   0% ─────────────────────────────── 100%
//! AirPlay         mute   -30 dB ───────────────────▶ 0 dB
//! ```
//!
//! Each speaker has (devices.toml):
//! * `default_volume`: where the slider is put at start-up, so a fresh start
//!   is never too loud (low for powerful speakers, higher for quiet ones);
//! * `max_volume`: an optional hard cap (100% = no cap).
//!
//! Safety rule: nothing above a speaker's cap is ever produced, whatever the
//! inputs. Every function below upholds it, and the tests check it.

use crate::store::DeviceSettings;

/// Quietest audible AirPlay volume; the bottom of the slider range.
pub const MIN_DB: f32 = -30.0;
/// AirPlay's "muted" value.
pub const MUTED_DB: f32 = -144.0;

/// The output's volume as reported by PipeWire.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SystemVolume {
    /// Slider position, 0.0..=1.0.
    pub level: f32,
    pub muted: bool,
}

/// AirPlay dB for a slider position (0.0..=1.0) on the full range.
pub fn level_to_db(level: f32) -> f32 {
    MIN_DB + level.clamp(0.0, 1.0) * -MIN_DB
}

/// Slider position (0.0..=1.0) for an AirPlay dB value; the inverse of
/// [`level_to_db`] (for display).
pub fn db_to_level(db: f32) -> f32 {
    ((db - MIN_DB) / -MIN_DB).clamp(0.0, 1.0)
}

/// Everything that decides one speaker's volume.
#[derive(Debug, Clone, Copy)]
pub struct VolumeInputs {
    pub settings: DeviceSettings,
    /// Live trim (`airpipe gain`), in dB.
    pub gain_db: f32,
    /// Extra cap for this run (`--max-volume`), as a slider position.
    pub cap: Option<f32>,
    /// Follow the system volume; otherwise play at `default_volume`.
    pub follow_system: bool,
}

impl VolumeInputs {
    /// The highest slider position this speaker may play at.
    pub fn ceiling_level(&self) -> f32 {
        self.settings.max_volume.min(self.cap.unwrap_or(1.0)).clamp(0.0, 1.0)
    }

    /// The loudest AirPlay value this speaker may receive.
    pub fn ceiling(&self) -> f32 {
        level_to_db(self.ceiling_level())
    }

    /// AirPlay dB for this speaker. Never above [`VolumeInputs::ceiling`].
    pub fn airplay_db(&self, system: Option<SystemVolume>) -> f32 {
        if self.ceiling_level() <= 0.0 {
            return MUTED_DB; // max_volume = 0: this speaker stays silent
        }
        let level = match system {
            Some(v) if self.follow_system => {
                if v.muted || v.level <= 0.0 {
                    return MUTED_DB;
                }
                v.level
            }
            // Not following, or no report from the desktop yet.
            _ => self.settings.default_volume,
        };
        (level_to_db(level) + self.gain_db).clamp(MIN_DB, self.ceiling())
    }

    /// Slider position to set at start-up: the speaker's default, capped.
    pub fn default_level(&self) -> f32 {
        self.settings.default_volume.clamp(0.0, self.ceiling_level())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(default: f32, max: f32) -> VolumeInputs {
        VolumeInputs {
            settings: DeviceSettings { default_volume: default, max_volume: max, latency_offset_ms: 0 },
            gain_db: 0.0,
            cap: None,
            follow_system: true,
        }
    }

    fn sys(level: f32) -> Option<SystemVolume> {
        Some(SystemVolume { level, muted: false })
    }

    #[test]
    fn slider_covers_the_full_range() {
        let v = inputs(0.25, 1.0);
        assert_eq!(v.airplay_db(sys(1.0)), 0.0, "100% is the speaker's maximum");
        assert_eq!(v.airplay_db(sys(0.4)), -18.0, "40% is 40% of the range");
        assert_eq!(db_to_level(-18.0), 0.4);
        assert_eq!(v.airplay_db(sys(0.0)), MUTED_DB);
        assert_eq!(v.airplay_db(Some(SystemVolume { level: 0.7, muted: true })), MUTED_DB);
    }

    #[test]
    fn start_level_is_the_device_default() {
        assert_eq!(inputs(0.25, 1.0).default_level(), 0.25);
        assert_eq!(inputs(0.5, 1.0).default_level(), 0.5);
        assert_eq!(inputs(0.25, 1.0).airplay_db(None), level_to_db(0.25), "before the desktop reports");
        assert_eq!(inputs(0.6, 0.4).default_level(), 0.4, "never above the cap");
    }

    #[test]
    fn never_above_the_cap() {
        let mut v = inputs(0.25, 0.5);
        assert_eq!(v.airplay_db(sys(1.0)), level_to_db(0.5));
        v.cap = Some(0.1);
        assert_eq!(v.airplay_db(sys(1.0)), level_to_db(0.1), "the run-time cap is lower");
        v.cap = None;
        v.gain_db = 5.0; // `airpipe gain` clamps to <= 0, but stay safe anyway
        assert_eq!(v.airplay_db(sys(0.5)), level_to_db(0.5));
        v.settings.max_volume = 7.0; // a bad value: still capped at the speaker maximum
        assert!(v.airplay_db(sys(1.0)) <= 0.0);
        v.settings.max_volume = 0.0;
        assert_eq!(v.airplay_db(sys(1.0)), MUTED_DB, "a cap of 0 is silence, not the quietest step");
    }

    #[test]
    fn fixed_levels_ignore_the_slider() {
        let mut v = inputs(0.4, 1.0);
        v.follow_system = false;
        assert_eq!(v.airplay_db(sys(1.0)), -18.0);
        assert_eq!(v.airplay_db(sys(0.0)), -18.0);
        v.gain_db = -6.0;
        assert_eq!(v.airplay_db(None), -24.0);
    }
}
