//! The audio engine: turns a PCM ring buffer into paced, encrypted RTP
//! packets and fans them out to every speaker in the group.
//!
//! # Timeline
//!
//! Packet `k` carries frames `[k·352, (k+1)·352)` with RTP timestamp
//! `rtp0 + k·352` and is due at `t0 + k·352/44100` ([`packet_time`]). Sync
//! packets tell every speaker "RTP timestamp `next − latency` is playing at
//! the moment packet `next` is due", so a packet is played `latency` after
//! it is due. All speakers share the same timestamps and the same clock (see
//! `timing.rs`), which is what keeps a multi-room group in sync. Sync packets
//! carry the packet's *scheduled* time, so thread scheduling jitter never
//! moves the speakers' timeline.
//!
//! # Threads and real-time rules
//!
//! The engine runs on its own thread with absolute deadlines, so tokio
//! scheduling never delays audio. The group adds/removes targets through a
//! mutex taken briefly a few times per packet. In steady state the thread does not
//! allocate: every buffer is sized up front and reused.
//!
//! While no speaker is connected the engine still drains the source (to
//! notice when audio starts) but wakes up only every `IDLE_BATCH` packets.

pub mod jitter;
pub mod timing;

use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

use crate::config::MIN_SPEAKER_BUFFER_MS;
use crate::crypto::audio::AudioCipher;
use crate::proto::alac;
use crate::proto::ntp::Clock;
use crate::proto::rtp::{AUDIO_HEADER_LEN, AudioHeader, FRAMES_PER_PACKET, RetransmitRequest, SAMPLE_RATE, Sync};
use crate::random;
use jitter::{JitterBuffer, Outcome};

/// Packets kept for retransmission (~8 s of audio).
const HISTORY: usize = 1024;
/// Send a sync packet to every speaker this often (in packets, ~1 s).
const SYNC_EVERY: u64 = 125;
/// Nonce space for marker-bit copies of packets, disjoint from packet counters.
const MARKED_NONCE_BIT: u64 = 1 << 63;
/// With no speaker connected, wake up only every this many packets (~96 ms).
const IDLE_BATCH: u64 = 12;
/// If the thread falls further behind than this, skip ahead instead of
/// sending a burst of stale packets.
const MAX_LAG_PACKETS: u64 = 50;
/// Encrypted packet size: header, ALAC payload, Poly1305 tag, nonce tail.
const PACKET_LEN: usize = AUDIO_HEADER_LEN + alac::encoded_len(FRAMES_PER_PACKET as usize) + 16 + 8;

/// Control-port limits, so spoofed retransmit requests can neither stall the
/// audio thread nor turn us into a traffic amplifier:
/// at most this many control packets read per tick,
const MAX_CONTROL_PACKETS_PER_TICK: usize = 16;
/// at most this many packets retransmitted per tick across all speakers,
const RETRANSMIT_PER_TICK: u32 = 32;
/// and per speaker a token bucket refilled by 2 packets per tick (250/s,
/// about 2 % packet loss) holding at most 128 tokens. Real speakers ask for a
/// few packets at a time.
const RETRANSMIT_REFILL: u32 = 2;
const RETRANSMIT_BURST: u32 = 128;

/// Identifies one speaker's destination within the engine.
pub type TargetId = u64;

/// Time after stream start at which packet `k` is due. Computed in 128-bit
/// arithmetic: `k · 352 · 10⁹` overflows 64 bits after ~4.8 days.
pub fn packet_time(k: u64) -> Duration {
    let nanos = u128::from(k) * u128::from(FRAMES_PER_PACKET) * 1_000_000_000 / u128::from(SAMPLE_RATE);
    Duration::from_nanos(nanos as u64)
}

/// Number of whole packets due in `d` (inverse of [`packet_time`]).
fn packets_in(d: Duration) -> u64 {
    (d.as_nanos() * u128::from(SAMPLE_RATE) / (1_000_000_000 * u128::from(FRAMES_PER_PACKET))) as u64
}

/// Frames in a duration, as an RTP timestamp difference: wraps modulo 2³²
/// like RTP timestamps do (use it only for RTP arithmetic).
pub fn duration_to_rtp_frames(d: Duration) -> u32 {
    (d.as_nanos() * u128::from(SAMPLE_RATE) / 1_000_000_000) as u32
}

/// Converts milliseconds to frames at the stream sample rate.
pub const fn ms_to_frames(ms: u32) -> u32 {
    (ms as u64 * SAMPLE_RATE as u64 / 1000) as u32
}

struct Target {
    data: UdpSocket,
    control_addr: SocketAddr,
    /// True until the first packet (with marker bit and initial sync) went out.
    joining: bool,
    /// Extra delay for this speaker in frames (may be negative), so speakers
    /// with different internal processing delays line up in a group.
    offset_frames: i32,
    /// Retransmit token bucket (see [`RETRANSMIT_BURST`]).
    retransmit_tokens: u32,
}

/// State shared between the engine thread and its handle.
struct Shared {
    targets: Mutex<HashMap<TargetId, Target>>,
    stop: AtomicBool,
    /// RTP timestamp of the next packet to be sent.
    next_rtptime: AtomicU32,
    /// Count of packets that carried audible (non-zero) samples, for idle
    /// detection. Digital silence does not count.
    audible_packets: AtomicU64,
}

impl Shared {
    /// The target list. A panic while holding the lock cannot leave it
    /// inconsistent (every update is a single insert/remove), so poisoning
    /// is ignored rather than propagated.
    fn targets(&self) -> MutexGuard<'_, HashMap<TargetId, Target>> {
        self.targets.lock().unwrap_or_else(|e| e.into_inner())
    }
}

pub struct EngineConfig {
    pub latency_frames: u32,
    pub control_port: u16,
    pub jitter_frames: usize,
}

/// Handle to the running engine. Dropping it stops the thread.
pub struct Engine {
    shared: Arc<Shared>,
    cipher_key: [u8; 32],
    latency_frames: u32,
    thread: Option<JoinHandle<()>>,
    next_id: AtomicU64,
}

impl Engine {
    pub fn start(source: rtrb::Consumer<i16>, clock: Clock, cfg: EngineConfig) -> Result<Self> {
        let control = UdpSocket::bind(("0.0.0.0", cfg.control_port)).with_context(|| {
            format!("binding control port {}/udp (is another airpipe already running?)", cfg.control_port)
        })?;
        control.set_nonblocking(true)?;

        // Random starting points, as RTP recommends, and a fresh audio key.
        let key: [u8; 32] = random::bytes();
        let rtp0 = u32::from_ne_bytes(random::bytes());
        let seq0 = u16::from_ne_bytes(random::bytes());
        let ssrc = u32::from_ne_bytes(random::bytes());

        let shared = Arc::new(Shared {
            targets: Mutex::new(HashMap::new()),
            stop: AtomicBool::new(false),
            next_rtptime: AtomicU32::new(rtp0),
            audible_packets: AtomicU64::new(0),
        });

        let worker = Worker {
            shared: shared.clone(),
            jitter: JitterBuffer::new(source, cfg.jitter_frames),
            cipher: AudioCipher::new(key),
            clock,
            control,
            latency: cfg.latency_frames,
            rtp0,
            seq0,
            ssrc,
            pcm: vec![0; FRAMES_PER_PACKET as usize * 2],
            payload: Vec::with_capacity(PACKET_LEN),
            marked: Vec::with_capacity(PACKET_LEN),
            history: (0..HISTORY).map(|_| (0, Vec::with_capacity(PACKET_LEN))).collect(),
        };
        let thread = std::thread::Builder::new().name("airpipe-audio".into()).spawn(move || worker.run())?;
        Ok(Self {
            shared,
            cipher_key: key,
            latency_frames: cfg.latency_frames,
            thread: Some(thread),
            next_id: AtomicU64::new(1),
        })
    }

    /// Audio key to hand to speakers in the stream SETUP (`shk`).
    pub fn audio_key(&self) -> [u8; 32] {
        self.cipher_key
    }

    /// Starts sending to a speaker whose stream SETUP returned these ports.
    ///
    /// `offset_ms` delays this speaker relative to the group (OwnTone's
    /// convention: positive plays later). devices.toml already limits it to
    /// `LATENCY_OFFSET_MS_RANGE`; the clamp here guarantees a negative offset
    /// never leaves the speaker less than `MIN_SPEAKER_BUFFER_MS` of buffer
    /// (or the latency itself, if lower), whatever the caller passes.
    pub fn add_target(
        &self,
        speaker: std::net::IpAddr,
        data_port: u16,
        control_port: u16,
        offset_ms: i32,
    ) -> Result<TargetId> {
        let min_offset = -(i64::from(self.latency_frames) - i64::from(ms_to_frames(MIN_SPEAKER_BUFFER_MS))).max(0);
        let offset_frames = (i64::from(offset_ms) * i64::from(SAMPLE_RATE) / 1000).max(min_offset) as i32;
        let data = UdpSocket::bind(if speaker.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" })?;
        data.connect(SocketAddr::new(speaker, data_port))?;
        data.set_nonblocking(true)?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let target = Target {
            data,
            control_addr: SocketAddr::new(speaker, control_port),
            joining: true,
            offset_frames,
            retransmit_tokens: RETRANSMIT_BURST,
        };
        self.shared.targets().insert(id, target);
        Ok(id)
    }

    pub fn remove_target(&self, id: TargetId) {
        self.shared.targets().remove(&id);
    }

    /// RTP timestamp currently audible on the speakers.
    pub fn playing_rtptime(&self) -> u32 {
        self.shared.next_rtptime.load(Ordering::Relaxed).wrapping_sub(self.latency_frames)
    }

    /// Count of packets so far that carried audible (non-zero) audio.
    pub fn audible_packets(&self) -> u64 {
        self.shared.audible_packets.load(Ordering::Relaxed)
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Worker {
    shared: Arc<Shared>,
    jitter: JitterBuffer,
    cipher: AudioCipher,
    clock: Clock,
    control: UdpSocket,
    latency: u32,
    rtp0: u32,
    seq0: u16,
    ssrc: u32,
    // Reused buffers (allocated once, in `Engine::start`).
    pcm: Vec<i16>,
    payload: Vec<u8>,
    marked: Vec<u8>,
    /// Encrypted packets by `seq % HISTORY`, for retransmission. An empty
    /// buffer means "nothing stored".
    history: Vec<(u16, Vec<u8>)>,
}

impl Worker {
    fn run(mut self) {
        let t0 = Instant::now();
        let mut k: u64 = 0;
        let mut last_stats = Instant::now();
        let mut was_idle = true;
        info!("audio engine started (latency {} ms)", u64::from(self.latency) * 1000 / u64::from(SAMPLE_RATE));

        while !self.shared.stop.load(Ordering::Relaxed) {
            // Wake for the next packet, or for a batch of them when nobody listens.
            let idle = self.shared.targets().is_empty();
            if was_idle && !idle {
                self.jitter.resync(); // reads were bursty until now
            }
            was_idle = idle;
            let batch = if idle { IDLE_BATCH } else { 1 };
            let deadline = t0 + packet_time(k + batch - 1);
            let now = Instant::now();
            if deadline > now {
                std::thread::sleep(deadline - now);
            } else if now - deadline > packet_time(MAX_LAG_PACKETS) {
                warn!("audio thread fell {} ms behind; skipping ahead", (now - deadline).as_millis());
                // Never move k backwards: it is also the encryption nonce.
                k = k.max(packets_in(now - t0));
                continue;
            }

            self.serve_retransmits(self.seq0.wrapping_add(k as u16));
            for _ in 0..batch {
                self.packet(k, t0 + packet_time(k), !idle);
                k += 1;
            }

            if last_stats.elapsed() > Duration::from_secs(30) {
                last_stats = Instant::now();
                debug!(
                    "jitter: {} dropped / {} repeated frames, {} underruns",
                    self.jitter.dropped_frames, self.jitter.repeated_frames, self.jitter.underruns
                );
            }
        }
        info!("audio engine stopped");
    }

    /// Produces packet `k` (due at `due`) and sends it to every target.
    /// `paced`: packets are produced one per tick (not in an idle burst).
    fn packet(&mut self, k: u64, due: Instant, paced: bool) {
        let rtptime = self.rtp0.wrapping_add((k as u32).wrapping_mul(FRAMES_PER_PACKET));
        let seq = self.seq0.wrapping_add(k as u16);

        // Always drain the source: it keeps the jitter buffer in step and
        // tells the group whether audio is flowing.
        if self.jitter.fill(&mut self.pcm, paced) == Outcome::Audio && self.pcm.iter().any(|&s| s != 0) {
            self.shared.audible_packets.fetch_add(1, Ordering::Relaxed);
        }
        self.shared.next_rtptime.store(rtptime.wrapping_add(FRAMES_PER_PACKET), Ordering::Relaxed);

        let mut targets = self.shared.targets();
        if targets.is_empty() {
            return; // nobody to send to: skip encoding and encryption
        }
        self.payload.clear();
        alac::encode_stereo_s16(&self.pcm, &mut self.payload);

        let any_joining = targets.values().any(|t| t.joining);
        let periodic_sync = k.is_multiple_of(SYNC_EVERY);
        if any_joining || periodic_sync {
            let due_ntp = self.clock.at(due);
            for t in targets.values().filter(|t| t.joining || periodic_sync) {
                let sync = Sync {
                    first: t.joining,
                    playing_rtptime: rtptime.wrapping_sub(self.latency).wrapping_sub(t.offset_frames as u32),
                    now: due_ntp,
                    next_rtptime: rtptime,
                };
                let _ = self.control.send_to(&sync.encode(), t.control_addr);
            }
        }

        // Encrypt into the history slot's buffer, then send it.
        let header = AudioHeader { marker: false, seq, timestamp: rtptime, ssrc: self.ssrc };
        let slot = &mut self.history[seq as usize % HISTORY];
        slot.0 = seq;
        self.cipher.seal_into(&header, &self.payload, k, &mut slot.1);
        if any_joining {
            // The marked copy differs only in its (authenticated) header, but
            // it needs its own nonce: reusing one would reuse the Poly1305 key.
            let marked = AudioHeader { marker: true, ..header };
            self.cipher.seal_into(&marked, &self.payload, k | MARKED_NONCE_BIT, &mut self.marked);
        }
        for t in targets.values_mut() {
            let bytes = if t.joining { &self.marked } else { &slot.1 };
            if let Err(e) = t.data.send(bytes) {
                debug!("send to {} failed: {e}", t.control_addr.ip());
            }
            t.joining = false;
        }
    }

    /// Answers retransmit requests from speakers we are streaming to, within
    /// the limits above. Anything else on the control port is dropped unparsed.
    /// `next_seq` is the sequence number of the next packet to be sent.
    fn serve_retransmits(&mut self, next_seq: u16) {
        let mut targets = self.shared.targets();
        for t in targets.values_mut() {
            t.retransmit_tokens = (t.retransmit_tokens + RETRANSMIT_REFILL).min(RETRANSMIT_BURST);
        }
        let mut budget = RETRANSMIT_PER_TICK;
        let mut buf = [0u8; 64];
        for _ in 0..MAX_CONTROL_PACKETS_PER_TICK {
            let Ok((n, peer)) = self.control.recv_from(&mut buf) else { return };
            let Some(target) = targets.values_mut().find(|t| t.control_addr.ip() == peer.ip()) else {
                continue;
            };
            let Some(req) = RetransmitRequest::parse(&buf[..n]) else { continue };
            // Only packets still in the history window can be resent.
            let age = next_seq.wrapping_sub(req.first_seq);
            if age == 0 || usize::from(age) > HISTORY {
                continue;
            }
            let count = u32::from(req.count).min(u32::from(age)).min(budget).min(target.retransmit_tokens);
            let mut sent = 0;
            for i in 0..count as u16 {
                let seq = req.first_seq.wrapping_add(i);
                let (stored, pkt) = &self.history[seq as usize % HISTORY];
                if *stored == seq && !pkt.is_empty() {
                    let _ = target.data.send(pkt);
                    sent += 1;
                }
            }
            budget -= count;
            target.retransmit_tokens -= count;
            debug!("retransmitted {sent}/{} packets from seq {} to {}", req.count, req.first_seq, peer.ip());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_time_never_overflows() {
        assert_eq!(packet_time(0), Duration::ZERO);
        assert_eq!(packet_time(44_100), Duration::from_secs(352));
        // One year of packets: the old 64-bit formula overflowed after ~4.8 days.
        let year = 365 * 24 * 3600 * u64::from(SAMPLE_RATE) / u64::from(FRAMES_PER_PACKET);
        let t = packet_time(year);
        assert!((t.as_secs_f64() - 365.0 * 24.0 * 3600.0).abs() < 1.0, "{t:?}");
    }

    #[test]
    fn frame_conversions() {
        assert_eq!(ms_to_frames(1000), 44_100);
        assert_eq!(ms_to_frames(250), 11_025);
        assert_eq!(duration_to_rtp_frames(Duration::from_secs(2)), 88_200);
    }

    #[test]
    fn packet_count_survives_long_uptimes() {
        // 2³² frames is ~27 h: a 32-bit frame count would wrap there.
        let month = Duration::from_secs(30 * 24 * 3600);
        let k = packets_in(month);
        assert!(packet_time(k) <= month && month - packet_time(k) < packet_time(1));
    }

    #[test]
    fn packet_buffers_are_sized_for_a_full_packet() {
        let cipher = AudioCipher::new([1; 32]);
        let mut payload = Vec::new();
        alac::encode_stereo_s16(&[0i16; FRAMES_PER_PACKET as usize * 2], &mut payload);
        let mut out = Vec::with_capacity(PACKET_LEN);
        let capacity = out.capacity();
        cipher.seal_into(&AudioHeader { marker: false, seq: 0, timestamp: 0, ssrc: 0 }, &payload, 0, &mut out);
        assert_eq!(out.len(), PACKET_LEN);
        assert_eq!(out.capacity(), capacity, "sealing must not reallocate");
    }
}
