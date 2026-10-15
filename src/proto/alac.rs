//! Minimal ALAC "encoder" producing *uncompressed* ALAC frames.
//!
//! AirPlay realtime streams must carry ALAC, but ALAC frames may escape to
//! verbatim PCM. That keeps CPU use and latency negligible at the cost of
//! ~1.4 Mbit/s per speaker, which any LAN handles. The frame layout is:
//!
//! ```text
//! element tag   3 bits  = 1 (channel pair element, i.e. stereo)
//! instance tag  4 bits  = 0
//! reserved     12 bits  = 0
//! has-size      1 bit   = 1 (explicit sample count follows)
//! shift         2 bits  = 0
//! uncompressed  1 bit   = 1
//! sample count 32 bits
//! samples       16 bits per channel, interleaved L R, big endian
//! end tag       3 bits  = 7
//! ```
//! padded with zero bits to a byte boundary.

/// Writes MSB-first bit fields into a byte buffer.
///
/// Bits are collected in a 64-bit accumulator and flushed a byte at a time,
/// so writing a 16-bit sample costs two byte pushes instead of 16 bit steps.
struct BitWriter<'a> {
    out: &'a mut Vec<u8>,
    acc: u64,
    /// Number of valid (not yet flushed) low bits in `acc`; always < 8 between calls.
    pending: u32,
}

impl<'a> BitWriter<'a> {
    fn new(out: &'a mut Vec<u8>) -> Self {
        Self { out, acc: 0, pending: 0 }
    }

    /// Appends the `n` least significant bits of `value`, most significant first.
    fn put(&mut self, value: u32, n: u32) {
        debug_assert!(n <= 32);
        let mask = if n == 32 { u32::MAX } else { (1u32 << n) - 1 };
        self.acc = (self.acc << n) | u64::from(value & mask);
        self.pending += n;
        while self.pending >= 8 {
            self.pending -= 8;
            self.out.push((self.acc >> self.pending) as u8);
        }
    }

    /// Flushes the last partial byte, padded with zero bits.
    fn finish(self) {
        if self.pending > 0 {
            self.out.push((self.acc << (8 - self.pending)) as u8);
        }
    }
}

/// Bytes needed for one encoded frame of `frames` stereo samples.
pub const fn encoded_len(frames: usize) -> usize {
    // 23 header bits + 32 size bits + 32 bits per frame + 3 end bits, rounded up.
    (23 + 32 + 32 * frames + 3).div_ceil(8)
}

/// Encodes interleaved signed 16-bit stereo samples as one uncompressed ALAC frame.
///
/// `samples.len()` must be even (left, right, left, right, ...).
pub fn encode_stereo_s16(samples: &[i16], out: &mut Vec<u8>) {
    assert!(samples.len().is_multiple_of(2), "samples must be interleaved stereo");
    let frames = samples.len() / 2;
    out.reserve(encoded_len(frames));

    let mut w = BitWriter::new(out);
    w.put(1, 3); // channel pair element
    w.put(0, 4); // instance tag
    w.put(0, 12); // reserved
    w.put(1, 1); // has size
    w.put(0, 2); // no shifted bytes
    w.put(1, 1); // not compressed
    w.put(frames as u32, 32);
    for &s in samples {
        w.put(s as u16 as u32, 16);
    }
    w.put(7, 3); // end of frame
    w.finish();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_bits_match_reference_layout() {
        let mut out = Vec::new();
        encode_stereo_s16(&[0x1234, -2], &mut out);
        assert_eq!(out.len(), encoded_len(1));
        // Bits 0..23: 001 0000 000000000000 1 00 1, i.e.
        // byte0: 0010_0000  byte1: 0000_0000  byte2: 0001_001x (x = first size bit)
        assert_eq!(out[0], 0b0010_0000);
        assert_eq!(out[1], 0b0000_0000);
        assert_eq!(out[2] & 0b1111_1110, 0b0001_0010);
    }

    #[test]
    fn samples_are_big_endian_after_header() {
        let mut out = Vec::new();
        encode_stereo_s16(&[0x1234, 0x5678], &mut out);
        // Rebuild the bitstream as a u128 to read fields at bit offsets.
        let mut bits: u128 = 0;
        for b in &out {
            bits = (bits << 8) | *b as u128;
        }
        let total = out.len() * 8;
        let field = |offset: usize, n: usize| -> u32 { ((bits >> (total - offset - n)) & ((1u128 << n) - 1)) as u32 };
        assert_eq!(field(23, 32), 1, "frame count");
        assert_eq!(field(55, 16), 0x1234, "left sample");
        assert_eq!(field(71, 16), 0x5678, "right sample");
        assert_eq!(field(87, 3), 7, "end tag");
    }

    /// The obvious bit-at-a-time writer, kept as a test oracle.
    fn reference_encode(samples: &[i16]) -> Vec<u8> {
        let mut bits: Vec<bool> = Vec::new();
        let mut put = |v: u32, n: u32| (0..n).rev().for_each(|i| bits.push((v >> i) & 1 == 1));
        put(1, 3);
        put(0, 4);
        put(0, 12);
        put(1, 1);
        put(0, 2);
        put(1, 1);
        put((samples.len() / 2) as u32, 32);
        for &s in samples {
            put(s as u16 as u32, 16);
        }
        put(7, 3);
        bits.chunks(8).map(|c| c.iter().enumerate().fold(0u8, |b, (i, &bit)| b | ((bit as u8) << (7 - i)))).collect()
    }

    #[test]
    fn matches_reference_writer() {
        // Deterministic pseudo-random samples (xorshift), including extremes.
        let mut x: u32 = 0x1234_5678;
        let mut samples: Vec<i16> = (0..352 * 2)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as i16
            })
            .collect();
        samples[0] = i16::MIN;
        samples[1] = i16::MAX;
        let mut out = Vec::new();
        encode_stereo_s16(&samples, &mut out);
        assert_eq!(out, reference_encode(&samples));
    }

    #[test]
    fn standard_packet_size() {
        let samples = vec![0i16; 352 * 2];
        let mut out = Vec::new();
        encode_stereo_s16(&samples, &mut out);
        // 23 + 32 + 352 * 32 + 3 = 11322 bits -> 1416 bytes
        assert_eq!(out.len(), 1416);
        assert_eq!(out.len(), encoded_len(352));
    }
}
