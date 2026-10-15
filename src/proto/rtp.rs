//! Wire formats of the UDP packets exchanged during a realtime AirPlay stream.
//!
//! | Packet            | Direction        | Port            | Header     |
//! |-------------------|------------------|-----------------|------------|
//! | Audio             | sender → speaker | speaker data    | `80 60/e0` |
//! | Sync              | sender → speaker | speaker control | `90/80 d4` |
//! | Retransmit request| speaker → sender | our control     | `80 d5`    |
//! | Timing request    | speaker → sender | our timing      | `80 d2`    |
//! | Timing reply      | sender → speaker | (reply)         | `80 d3`    |
//!
//! Audio payload encryption lives in [`crate::crypto::audio`]; this module
//! only deals with layout.

use super::ntp::NtpTime;

/// Frames (stereo samples) carried by each audio packet.
pub const FRAMES_PER_PACKET: u32 = 352;
/// Sample rate of the stream we negotiate (ALAC/44100/16/2).
pub const SAMPLE_RATE: u32 = 44_100;
/// RTP payload type for realtime audio.
pub const PAYLOAD_TYPE_REALTIME: u8 = 0x60;
/// RTP marker bit, set on the first packet after (re)starting a stream.
pub const MARKER: u8 = 0x80;

pub const AUDIO_HEADER_LEN: usize = 12;

/// Fixed 12-byte RTP header of an audio packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioHeader {
    pub marker: bool,
    pub seq: u16,
    pub timestamp: u32,
    pub ssrc: u32,
}

impl AudioHeader {
    pub fn encode(&self) -> [u8; AUDIO_HEADER_LEN] {
        let mut h = [0u8; AUDIO_HEADER_LEN];
        h[0] = 0x80;
        h[1] = PAYLOAD_TYPE_REALTIME | if self.marker { MARKER } else { 0 };
        h[2..4].copy_from_slice(&self.seq.to_be_bytes());
        h[4..8].copy_from_slice(&self.timestamp.to_be_bytes());
        h[8..12].copy_from_slice(&self.ssrc.to_be_bytes());
        h
    }
}

pub const SYNC_LEN: usize = 20;

/// Control-channel sync packet telling the speaker which RTP timestamp should
/// be audible at which instant of our clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sync {
    /// First sync of a (re)started stream. Sets the extension bit (0x90).
    pub first: bool,
    /// RTP timestamp that must be playing at `now`.
    pub playing_rtptime: u32,
    /// Our clock at the instant packet `next_rtptime` is due (its scheduled
    /// time, not the moment the packet happens to be built).
    pub now: NtpTime,
    /// RTP timestamp of the next packet we are going to send.
    pub next_rtptime: u32,
}

impl Sync {
    pub fn encode(&self) -> [u8; SYNC_LEN] {
        let mut p = [0u8; SYNC_LEN];
        p[0] = if self.first { 0x90 } else { 0x80 };
        p[1] = 0xd4;
        // 0x0007 is what Apple senders use; AirPlay 2 receivers take their
        // latency from the SETUP request instead.
        p[2] = 0x00;
        p[3] = 0x07;
        p[4..8].copy_from_slice(&self.playing_rtptime.to_be_bytes());
        p[8..16].copy_from_slice(&self.now.to_be_bytes());
        p[16..20].copy_from_slice(&self.next_rtptime.to_be_bytes());
        p
    }
}

/// A speaker asking us to resend `count` packets starting at `first_seq`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetransmitRequest {
    pub first_seq: u16,
    pub count: u16,
}

impl RetransmitRequest {
    pub fn parse(p: &[u8]) -> Option<Self> {
        if p.len() < 8 || p[0] != 0x80 || p[1] & 0x7f != 0x55 {
            return None;
        }
        Some(Self { first_seq: u16::from_be_bytes([p[4], p[5]]), count: u16::from_be_bytes([p[6], p[7]]) })
    }
}

pub const TIMING_LEN: usize = 32;

/// Answers an NTP-style timing request (`80 d2`) with a timing reply (`80 d3`).
///
/// The speaker's transmit timestamp is echoed as the reference timestamp, and
/// our receive/transmit timestamps are filled in so the speaker can compute
/// its offset to our clock.
pub fn timing_reply(request: &[u8], received: NtpTime, transmitted: NtpTime) -> Option<[u8; TIMING_LEN]> {
    if request.len() != TIMING_LEN || request[0] != 0x80 || request[1] & 0x7f != 0x52 {
        return None;
    }
    let mut r = [0u8; TIMING_LEN];
    r[0] = 0x80;
    r[1] = 0xd3;
    r[2] = request[2];
    r[3] = request[3];
    r[8..16].copy_from_slice(&request[24..32]);
    r[16..24].copy_from_slice(&received.to_be_bytes());
    r[24..32].copy_from_slice(&transmitted.to_be_bytes());
    Some(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_header_layout() {
        let h = AudioHeader { marker: true, seq: 0x0102, timestamp: 0x03040506, ssrc: 0x0708090a }.encode();
        assert_eq!(h, [0x80, 0xe0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        let h = AudioHeader { marker: false, seq: 0, timestamp: 0, ssrc: 0 }.encode();
        assert_eq!(&h[..2], &[0x80, 0x60]);
    }

    #[test]
    fn sync_layout() {
        let p =
            Sync { first: true, playing_rtptime: 1000, now: NtpTime(0x1122334455667788), next_rtptime: 89200 }.encode();
        assert_eq!(&p[..4], &[0x90, 0xd4, 0x00, 0x07]);
        assert_eq!(u32::from_be_bytes(p[4..8].try_into().unwrap()), 1000);
        assert_eq!(&p[8..16], &[0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88]);
        assert_eq!(u32::from_be_bytes(p[16..20].try_into().unwrap()), 89200);
    }

    #[test]
    fn retransmit_request_parsing() {
        let r = RetransmitRequest::parse(&[0x80, 0xd5, 0x00, 0x01, 0xbd, 0x0d, 0x00, 0x02]).unwrap();
        assert_eq!(r, RetransmitRequest { first_seq: 0xbd0d, count: 2 });
        assert!(RetransmitRequest::parse(&[0x80, 0xd4, 0, 1, 0, 0, 0, 1]).is_none());
        assert!(RetransmitRequest::parse(&[0x80, 0xd5]).is_none());
    }

    #[test]
    fn timing_reply_echoes_client_time() {
        let mut req = [0u8; 32];
        req[0] = 0x80;
        req[1] = 0xd2;
        req[2] = 0x00;
        req[3] = 0x07;
        req[24..32].copy_from_slice(&[9, 9, 9, 9, 8, 8, 8, 8]);
        let reply = timing_reply(&req, NtpTime(1), NtpTime(2)).unwrap();
        assert_eq!(&reply[..4], &[0x80, 0xd3, 0x00, 0x07]);
        assert_eq!(&reply[8..16], &[9, 9, 9, 9, 8, 8, 8, 8]);
        assert_eq!(u64::from_be_bytes(reply[16..24].try_into().unwrap()), 1);
        assert_eq!(u64::from_be_bytes(reply[24..32].try_into().unwrap()), 2);
        assert!(timing_reply(&req[..31], NtpTime(1), NtpTime(2)).is_none());
    }
}
