//! The sender clock, expressed as 64-bit NTP timestamps.
//!
//! In NTP timing mode every speaker in a group synchronises to *our* clock by
//! sending timing requests to the timing port. Audio timing (sync packets) is
//! expressed in the same clock, so all that matters is that it is monotonic
//! and shared by every component. We anchor it to the wall clock once at
//! startup and then advance it with a monotonic `Instant`, so it never jumps
//! when the system time is adjusted.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Seconds between the NTP epoch (1900) and the Unix epoch (1970).
const NTP_UNIX_OFFSET: u64 = 2_208_988_800;

/// 32.32 fixed point NTP timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct NtpTime(pub u64);

impl NtpTime {
    pub fn from_duration_since_1900(d: Duration) -> Self {
        let frac = ((d.subsec_nanos() as u128) << 32) / 1_000_000_000;
        NtpTime((d.as_secs() << 32) | frac as u64)
    }

    pub fn to_be_bytes(self) -> [u8; 8] {
        self.0.to_be_bytes()
    }
}

/// Monotonic clock that reports NTP time.
#[derive(Debug, Clone, Copy)]
pub struct Clock {
    anchor_instant: Instant,
    anchor_since_1900: Duration,
}

impl Clock {
    pub fn new() -> Self {
        let since_unix = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        Self { anchor_instant: Instant::now(), anchor_since_1900: since_unix + Duration::from_secs(NTP_UNIX_OFFSET) }
    }

    pub fn now(&self) -> NtpTime {
        self.at(Instant::now())
    }

    /// NTP time of a monotonic instant (e.g. when a packet is *due*, rather
    /// than when the thread happened to wake up). Instants before the clock
    /// was created are not used by airpipe and map to the anchor.
    pub fn at(&self, instant: Instant) -> NtpTime {
        NtpTime::from_duration_since_1900(
            self.anchor_since_1900 + instant.saturating_duration_since(self.anchor_instant),
        )
    }
}

impl Default for Clock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fraction_encoding() {
        let t = NtpTime::from_duration_since_1900(Duration::from_millis(1500));
        assert_eq!(t.0 >> 32, 1);
        assert_eq!(t.0 & 0xffff_ffff, 0x8000_0000);
    }

    #[test]
    fn clock_is_monotonic_and_near_wall_time() {
        let c = Clock::new();
        let a = c.now();
        let b = c.now();
        assert!(b >= a);
        let unix_secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        assert!(((a.0 >> 32) as i64 - (unix_secs + NTP_UNIX_OFFSET) as i64).abs() <= 1);
    }
}
