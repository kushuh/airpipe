//! Notices when the computer wakes up from sleep (suspend).
//!
//! A speaker gives up on our session while the computer sleeps, but the
//! closed connection only shows once the computer is awake again. To the
//! session that looks just like another device taking the speaker over, so
//! the group asks a [`Watch`] whether the computer has just woken up.
//!
//! `Instant` stops while the computer sleeps (it is CLOCK_MONOTONIC on
//! Linux), but the uptime in `/proc/uptime` keeps counting (CLOCK_BOOTTIME).
//! When the uptime moves on further than `Instant` between two looks, the
//! difference is time spent asleep. Observed on 2026-11-03: after 14 hours
//! asleep, `Instant` had moved on by 1.6 s.

use std::time::{Duration, Instant};

/// Shorter gaps are rounding (`/proc/uptime` counts in steps of 10 ms), and
/// no speaker gives up on a session that quickly.
const MIN_SLEEP: Duration = Duration::from_secs(1);

/// Both clocks, read at the same moment.
#[derive(Debug, Clone, Copy)]
struct Sample {
    at: Instant,
    uptime: Duration,
}

/// Remembers when the computer last woke up. Call [`Watch::look`] every few
/// seconds, so that a wake-up is noticed soon after it happens.
#[derive(Debug, Default)]
pub struct Watch {
    last: Option<Sample>,
    woke_at: Option<Instant>,
}

impl Watch {
    /// Compares the clocks with the previous look. Where `/proc/uptime`
    /// cannot be read, wake-ups go unnoticed.
    pub fn look(&mut self) {
        let at = Instant::now();
        if let Some(uptime) = std::fs::read_to_string("/proc/uptime").ok().and_then(|s| parse_uptime(&s)) {
            self.observe(Sample { at, uptime });
        }
    }

    /// When the computer last woke up, as of the last look (noticed at most
    /// one look late).
    pub fn woke_at(&self) -> Option<Instant> {
        self.woke_at
    }

    fn observe(&mut self, now: Sample) {
        if let Some(last) = self.last
            && asleep_between(last, now) >= MIN_SLEEP
        {
            self.woke_at = Some(now.at);
        }
        self.last = Some(now);
    }
}

/// How long the computer slept between two samples: the time the uptime
/// counted and `Instant` did not.
fn asleep_between(a: Sample, b: Sample) -> Duration {
    let awake = b.at.saturating_duration_since(a.at);
    b.uptime.saturating_sub(a.uptime).saturating_sub(awake)
}

/// The first field of `/proc/uptime`: seconds since boot, sleep included.
fn parse_uptime(text: &str) -> Option<Duration> {
    let secs: f64 = text.split_whitespace().next()?.parse().ok()?;
    Duration::try_from_secs_f64(secs).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(s: f64) -> Duration {
        Duration::from_secs_f64(s)
    }

    /// A look `after` seconds of `Instant` time from `start`, with this uptime.
    fn look(watch: &mut Watch, start: Instant, after: f64, uptime: f64) {
        watch.observe(Sample { at: start + secs(after), uptime: secs(uptime) });
    }

    #[test]
    fn reads_proc_uptime() {
        assert_eq!(parse_uptime("350735.47 2343889.90\n"), Some(secs(350735.47)));
        assert_eq!(parse_uptime(""), None);
        assert_eq!(parse_uptime("-1.00 0.00\n"), None);
        assert_eq!(parse_uptime("abc 0.00\n"), None);
    }

    #[test]
    fn notices_time_spent_asleep() {
        let (start, mut watch) = (Instant::now(), Watch::default());
        look(&mut watch, start, 0.0, 1000.0);
        // Awake: both clocks move together, give or take the 10 ms steps.
        look(&mut watch, start, 2.0, 1002.0);
        look(&mut watch, start, 4.25, 1004.24);
        look(&mut watch, start, 6.0, 1006.01);
        assert_eq!(watch.woke_at, None);
        // 14 hours asleep between two looks that `Instant` puts 1.6 s apart.
        let asleep = 14.0 * 3600.0;
        look(&mut watch, start, 7.6, 1007.6 + asleep);
        assert_eq!(watch.woke_at, Some(start + secs(7.6)));
        // Later looks, awake again, keep the time of the wake-up.
        look(&mut watch, start, 9.6, 1009.6 + asleep);
        assert_eq!(watch.woke_at, Some(start + secs(7.6)));
    }

    #[test]
    fn a_gap_under_a_second_is_not_sleep() {
        let (start, mut watch) = (Instant::now(), Watch::default());
        look(&mut watch, start, 0.0, 50.0);
        look(&mut watch, start, 2.0, 52.9);
        assert_eq!(watch.woke_at, None);
        look(&mut watch, start, 4.0, 56.0);
        assert_eq!(watch.woke_at, Some(start + secs(4.0)));
    }
}
