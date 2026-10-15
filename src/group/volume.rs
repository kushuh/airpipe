//! Volume rules. Pure functions, so they can be audited and tested alone.
//!
//! AirPlay volume is in dB: -30 is the quietest audible step, 0 is the
//! speaker's **full power**, and -144 means mute. Each speaker maps the
//! system volume slider onto its own range from devices.toml:
//!
//! ```text
//! system volume   0% ─────────────────────────── 100%
//! AirPlay         mute   -30 dB ─────────────▶ max_volume_db
//! ```
//!
//! Safety rule: nothing above a speaker's ceiling is ever produced, whatever
//! the inputs. Every function below upholds it, and the tests check it.

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

/// Everything that decides one speaker's volume.
#[derive(Debug, Clone, Copy)]
pub struct VolumeInputs {
    pub settings: DeviceSettings,
    /// Live trim from the console, in dB.
    pub gain_db: f32,
    /// Extra ceiling for this run (`--max-volume`).
    pub cap_db: Option<f32>,
    /// Follow the system volume; otherwise play at `default_volume_db`.
    pub follow_system: bool,
}

impl VolumeInputs {
    /// The loudest value this speaker may receive.
    pub fn ceiling(&self) -> f32 {
        self.settings.max_volume_db.min(self.cap_db.unwrap_or(0.0)).clamp(MIN_DB, 0.0)
    }

    /// AirPlay dB for this speaker. Never above [`VolumeInputs::ceiling`].
    pub fn airplay_db(&self, system: Option<SystemVolume>) -> f32 {
        let ceiling = self.ceiling();
        let base = match system {
            Some(v) if self.follow_system => {
                if v.muted || v.level <= 0.0 {
                    return MUTED_DB;
                }
                MIN_DB + v.level.clamp(0.0, 1.0) * (ceiling - MIN_DB)
            }
            // Not following, or no report from the desktop yet.
            _ => self.settings.default_volume_db,
        };
        (base + self.gain_db).clamp(MIN_DB, ceiling)
    }

    /// Slider position at which this speaker plays its default level.
    pub fn default_level(&self) -> f32 {
        let ceiling = self.ceiling();
        if ceiling <= MIN_DB {
            return 1.0;
        }
        ((self.settings.default_volume_db - MIN_DB) / (ceiling - MIN_DB)).clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(max: f32, default: f32) -> VolumeInputs {
        VolumeInputs {
            settings: DeviceSettings { max_volume_db: max, default_volume_db: default, latency_offset_ms: 0 },
            gain_db: 0.0,
            cap_db: None,
            follow_system: true,
        }
    }

    fn sys(level: f32) -> Option<SystemVolume> {
        Some(SystemVolume { level, muted: false })
    }

    #[test]
    fn slider_maps_onto_device_range() {
        let v = inputs(-20.0, -26.0);
        assert_eq!(v.airplay_db(sys(1.0)), -20.0);
        assert_eq!(v.airplay_db(sys(0.5)), -25.0);
        assert_eq!(v.airplay_db(sys(0.0)), MUTED_DB);
        assert_eq!(v.airplay_db(Some(SystemVolume { level: 0.7, muted: true })), MUTED_DB);
        assert_eq!(v.airplay_db(None), -26.0, "default before the desktop reports");
    }

    #[test]
    fn default_level_round_trips() {
        let v = inputs(-20.0, -26.0);
        let level = v.default_level();
        assert!((level - 0.4).abs() < 1e-6);
        assert!((v.airplay_db(sys(level)) - -26.0).abs() < 1e-4);
    }

    #[test]
    fn never_above_ceiling_or_cap() {
        let mut v = inputs(-10.0, -20.0);
        v.cap_db = Some(-25.0);
        assert_eq!(v.airplay_db(sys(1.0)), -25.0);
        v.cap_db = None;
        v.gain_db = 5.0; // the console clamps gain to <= 0, but stay safe anyway
        assert_eq!(v.airplay_db(sys(1.0)), -10.0);
        v.settings.max_volume_db = 12.0; // a bad edit in devices.toml
        assert_eq!(v.airplay_db(sys(1.0)), 0.0);
    }

    #[test]
    fn fixed_levels_ignore_the_slider() {
        let mut v = inputs(-10.0, -18.0);
        v.follow_system = false;
        assert_eq!(v.airplay_db(sys(1.0)), -18.0);
        assert_eq!(v.airplay_db(sys(0.0)), -18.0);
        v.gain_db = -6.0;
        assert_eq!(v.airplay_db(None), -24.0);
    }
}
