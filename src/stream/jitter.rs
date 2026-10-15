//! Jitter buffer between the audio source and the packet clock.
//!
//! The source (PipeWire) delivers audio in bursts, one graph quantum at a
//! time, paced by the graph's clock. We send packets paced by our own clock.
//! Two things follow:
//!
//! 1. We keep a small cushion of audio (`target` frames) so a packet can
//!    always be filled even if the next burst is a little late.
//! 2. The two clocks drift apart slowly (tens of ppm). When the average fill
//!    moves away from the target we drop or repeat a single frame per packet,
//!    which is inaudible and corrects up to ~125 frames per second.
//!
//! If the cushion runs dry (the source stopped, e.g. nothing is playing) we
//! emit silence and wait for the cushion to refill before resuming.

use rtrb::Consumer;

/// Smoothing factor of the fill average (per packet, ~8 ms).
const EMA_ALPHA: f64 = 0.01;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Audio was produced from the source.
    Audio,
    /// The cushion is empty or still refilling; silence was produced.
    Silence,
}

pub struct JitterBuffer {
    /// Interleaved stereo samples.
    source: Consumer<i16>,
    /// Desired buffered frames before and while playing.
    target: usize,
    /// Allowed deviation of the average fill before correcting.
    tolerance: usize,
    filling: bool,
    avg_fill: f64,
    pub dropped_frames: u64,
    pub repeated_frames: u64,
    pub underruns: u64,
}

impl JitterBuffer {
    pub fn new(source: Consumer<i16>, target_frames: usize) -> Self {
        Self {
            source,
            target: target_frames,
            tolerance: target_frames / 2,
            filling: true,
            avg_fill: target_frames as f64,
            dropped_frames: 0,
            repeated_frames: 0,
            underruns: 0,
        }
    }

    fn buffered_frames(&self) -> usize {
        self.source.slots() / 2
    }

    /// Re-anchors the drift estimate on the current fill. Call when the
    /// consumer switches from bursts back to steady per-packet reads.
    pub fn resync(&mut self) {
        self.avg_fill = self.buffered_frames() as f64;
    }

    /// Fills `out` (interleaved stereo, `out.len() / 2` frames).
    ///
    /// `track_drift`: whether reads are paced one packet at a time. When they
    /// come in bursts (the engine's idle mode) the fill level says nothing
    /// about clock drift, so no correction is made.
    pub fn fill(&mut self, out: &mut [i16], track_drift: bool) -> Outcome {
        let frames = out.len() / 2;
        let available = self.buffered_frames();

        if self.filling {
            if available < self.target + frames {
                out.fill(0);
                return Outcome::Silence;
            }
            self.filling = false;
            // Seed with the target, not the current fill: a start-up burst
            // above target is normal and must not trigger corrections.
            self.avg_fill = self.target as f64;
        }
        if available > self.target * 4 + frames {
            // Far too much queued (source much faster than us, or we stalled):
            // skip ahead instead of letting latency grow without bound.
            let excess = available - self.target;
            self.discard(excess);
            self.dropped_frames += excess as u64;
            self.avg_fill = self.target as f64;
            return self.fill(out, track_drift);
        }
        if available < frames {
            // Ran dry: play what is left, pad with silence, and refill.
            let n = available * 2;
            self.read_into(&mut out[..n]);
            out[n..].fill(0);
            self.filling = true;
            self.underruns += 1;
            return Outcome::Silence;
        }

        if !track_drift {
            self.read_into(out);
            return Outcome::Audio;
        }
        self.avg_fill += EMA_ALPHA * (available as f64 - self.avg_fill);
        let high = (self.target + self.tolerance) as f64;
        let low = self.target.saturating_sub(self.tolerance) as f64;

        if self.avg_fill > high && available > frames {
            // Source clock is fast: consume one extra frame and discard it.
            self.read_into(out);
            let mut skip = [0i16; 2];
            self.read_into(&mut skip);
            self.dropped_frames += 1;
            self.avg_fill -= 1.0;
        } else if self.avg_fill < low && frames > 1 {
            // Source clock is slow: read one frame less and repeat the last.
            let n = out.len() - 2;
            self.read_into(&mut out[..n]);
            out[n] = out[n - 2];
            out[n + 1] = out[n - 1];
            self.repeated_frames += 1;
            self.avg_fill += 1.0;
        } else {
            self.read_into(out);
        }
        Outcome::Audio
    }

    fn discard(&mut self, frames: usize) {
        let chunk = self.source.read_chunk(frames * 2).expect("caller checked availability");
        chunk.commit_all();
    }

    fn read_into(&mut self, out: &mut [i16]) {
        let chunk = self.source.read_chunk(out.len()).expect("caller checked availability");
        let (a, b) = chunk.as_slices();
        out[..a.len()].copy_from_slice(a);
        out[a.len()..].copy_from_slice(b);
        chunk.commit_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtrb::RingBuffer;

    fn push_frames(p: &mut rtrb::Producer<i16>, start: i16, frames: usize) {
        for i in 0..frames {
            let v = start.wrapping_add(i as i16);
            p.push(v).unwrap();
            p.push(-v).unwrap();
        }
    }

    #[test]
    fn waits_for_cushion_then_plays_in_order() {
        let (mut p, c) = RingBuffer::new(1 << 16);
        let mut jb = JitterBuffer::new(c, 1000);
        let mut out = vec![1i16; 352 * 2];

        push_frames(&mut p, 0, 500);
        assert_eq!(jb.fill(&mut out, true), Outcome::Silence);
        assert!(out.iter().all(|&s| s == 0));

        push_frames(&mut p, 500, 1000);
        assert_eq!(jb.fill(&mut out, true), Outcome::Audio);
        assert_eq!(&out[..4], &[0, 0, 1, -1]);
        assert_eq!(out[351 * 2], 351);
        assert_eq!(jb.fill(&mut out, true), Outcome::Audio);
        assert_eq!(out[0], 352, "continues exactly where it left off");
    }

    #[test]
    fn runaway_source_is_capped() {
        let (mut p, c) = RingBuffer::new(1 << 20);
        let mut jb = JitterBuffer::new(c, 1000);
        let mut out = vec![0i16; 352 * 2];
        push_frames(&mut p, 0, 1400);
        jb.fill(&mut out, true);
        push_frames(&mut p, 0, 10_000);
        assert_eq!(jb.fill(&mut out, true), Outcome::Audio);
        assert!(jb.buffered_frames() <= 1000, "latency was bounded back to the target");
    }

    #[test]
    fn bursty_reads_do_not_trigger_drift_correction() {
        let (mut p, c) = RingBuffer::new(1 << 20);
        let mut jb = JitterBuffer::new(c, 3072);
        let mut out = vec![0i16; 352 * 2];
        push_frames(&mut p, 0, 3072 + 352);
        // Idle mode: 12 packets read back to back after 12 packets arrived.
        for _ in 0..200 {
            push_frames(&mut p, 0, 352 * 12);
            for _ in 0..12 {
                jb.fill(&mut out, false);
            }
        }
        assert_eq!(jb.dropped_frames + jb.repeated_frames, 0, "no corrections from burst reads");
        jb.resync();
        assert!(jb.buffered_frames() >= 3072, "the cushion is intact when streaming starts");
    }

    #[test]
    fn underrun_pads_and_refills() {
        let (mut p, c) = RingBuffer::new(1 << 16);
        let mut jb = JitterBuffer::new(c, 100);
        let mut out = vec![0i16; 352 * 2];
        push_frames(&mut p, 1, 452);
        assert_eq!(jb.fill(&mut out, true), Outcome::Audio);
        assert_eq!(jb.fill(&mut out, true), Outcome::Silence, "only 100 frames left");
        assert_eq!(out[99 * 2], 452);
        assert_eq!(out[100 * 2], 0);
        assert_eq!(jb.underruns, 1);
        push_frames(&mut p, 0, 200);
        assert_eq!(jb.fill(&mut out, true), Outcome::Silence, "refilling to the target first");
    }

    #[test]
    fn corrects_a_fast_source_by_dropping() {
        let (mut p, c) = RingBuffer::new(1 << 20);
        let mut jb = JitterBuffer::new(c, 2000);
        let mut out = vec![0i16; 352 * 2];
        push_frames(&mut p, 0, 2400);
        // Source delivers one extra frame every 4 packets (~700 ppm, far
        // worse than real clocks but within the 1 frame/packet correction).
        for i in 0..3000 {
            push_frames(&mut p, 0, if i % 4 == 0 { 353 } else { 352 });
            jb.fill(&mut out, true);
        }
        assert!(jb.dropped_frames > 0);
        assert_eq!(jb.repeated_frames, 0);
        assert!(jb.buffered_frames() < 2000 + 1000 + 400, "fill stays bounded");
    }

    #[test]
    fn corrects_a_slow_source_by_repeating() {
        let (mut p, c) = RingBuffer::new(1 << 20);
        let mut jb = JitterBuffer::new(c, 2000);
        let mut out = vec![0i16; 352 * 2];
        push_frames(&mut p, 0, 2400);
        // Long enough for the deficit (1 frame / 4 packets) to cross the band.
        for i in 0..12_000 {
            push_frames(&mut p, 0, if i % 4 == 0 { 351 } else { 352 });
            jb.fill(&mut out, true);
        }
        assert!(jb.repeated_frames > 0);
        assert_eq!(jb.underruns, 0);
    }
}
