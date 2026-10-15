//! One speaker's AirPlay 2 session.
//!
//! Startup sequence (realtime audio, NTP timing):
//!
//! 1. `GET /info`: status flags (PIN/password required?).
//! 2. Pairing: pair-verify with stored keys, else transient pair-setup.
//! 3. Switch the RTSP connection to encrypted framing.
//! 4. `SETUP` (session): announce our timing port, receive the event port.
//! 5. Connect the event channel (speaker → us: remote control commands).
//! 6. `RECORD`.
//! 7. `SETUP` (stream): announce format, audio key and our control port;
//!    receive the speaker's data and control ports.
//! 8. `SET_PARAMETER volume` (some devices need it last; OwnTone notes Sonos).
//!
//! After that the session is an actor: it serves volume/metadata commands,
//! keeps the connection alive with `POST /feedback`, and tears down on request.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use plist::{Dictionary, Value};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{debug, info, warn};

use super::connection::{ClosedBySpeaker, Connection, USER_AGENT};
use super::events::{self, RemoteCommand};
use super::pair;
use crate::config::MIN_SPEAKER_BUFFER_MS;
use crate::crypto::channel::{Channel, ChannelCipher};
use crate::crypto::pairing::DeviceKeys;
use crate::discovery::{Device, status};
use crate::group::volume::{MUTED_DB, nudged_below};
use crate::proto::dmap::{self, TrackInfo};
use crate::proto::rtp::{FRAMES_PER_PACKET, SAMPLE_RATE};
use crate::proto::rtsp::{Message, Request};
use crate::store::LocalIdentity;
use crate::stream::ms_to_frames;

/// Aborts a background task when dropped. Holds the event-channel task, so
/// that it stops whenever the session does, including when setup fails
/// after the event channel was opened.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Interval of `POST /feedback` keepalives (what macOS uses).
const FEEDBACK_INTERVAL: Duration = Duration::from_secs(2);
/// When to re-send the volume, counted from the first sound at the speaker.
const REASSERT_AFTER: [Duration; 2] = [Duration::from_millis(500), Duration::from_millis(2500)];

/// Everything a session needs that is shared by the whole group.
#[derive(Clone)]
pub struct SessionContext {
    pub identity: LocalIdentity,
    pub client_name: String,
    pub timing_port: u16,
    pub control_port: u16,
    pub audio_key: [u8; 32],
    pub latency_frames: u32,
    pub group_uuid: String,
}

/// Metadata commands accepted by a running session. They may be dropped
/// when the speaker is slow; volume is never dropped (see
/// [`SessionHandle::set_volume`]).
#[derive(Debug)]
pub enum Command {
    Text {
        info: TrackInfo,
        rtptime: u32,
    },
    Artwork {
        mime: String,
        data: Vec<u8>,
        rtptime: u32,
    },
    /// RTP timestamps of track start, current position and end.
    Progress {
        start: u32,
        current: u32,
        end: u32,
    },
}

/// Identifies one session, so events from an old session of the same
/// speaker are never mistaken for the current one.
pub type SessionId = u64;

/// Notifications from sessions to whoever runs the group. A session that
/// ends because its handle was dropped sends nothing: its owner knows.
#[derive(Debug)]
pub enum SessionEvent {
    /// The speaker closed the session, usually because another device took
    /// it over. Not an error: do not reconnect right away.
    TakenOver {
        session: SessionId,
    },
    Failed {
        session: SessionId,
        error: String,
    },
    Remote {
        session: SessionId,
        command: RemoteCommand,
    },
}

impl SessionEvent {
    /// The session the event comes from.
    pub fn session(&self) -> SessionId {
        match self {
            Self::TakenOver { session } | Self::Failed { session, .. } | Self::Remote { session, .. } => *session,
        }
    }
}

/// Ports the speaker gave us in the stream SETUP (on `device.address.ip()`).
#[derive(Debug, Clone, Copy)]
pub struct StreamPorts {
    pub data: u16,
    pub control: u16,
}

/// A running session. Dropping it ends the session (TEARDOWN is sent in the
/// background); [`SessionHandle::teardown`] also waits for the speaker.
pub struct SessionHandle {
    pub id: SessionId,
    pub device: Device,
    pub ports: StreamPorts,
    tx: mpsc::Sender<Command>,
    volume: watch::Sender<f32>,
    /// Completes when the session actor has finished (after TEARDOWN).
    finished: oneshot::Receiver<()>,
}

impl SessionHandle {
    /// Queues a metadata command; dropped (harmlessly) if the speaker is busy.
    pub fn send(&self, cmd: Command) {
        if self.tx.try_send(cmd).is_err() {
            debug!("{}: session busy or gone, dropping metadata", self.device.name);
        }
    }

    /// Sets the AirPlay volume in dB. Only the latest value is kept and it is
    /// always delivered, ahead of any queued metadata.
    pub fn set_volume(&self, db: f32) {
        self.volume.send_replace(db);
    }

    /// Ends the session, waiting at most 3 s for the speaker to acknowledge.
    pub async fn teardown(self) {
        let Self { tx, volume, finished, .. } = self;
        drop((tx, volume)); // closing both channels tells the actor to stop
        let _ = tokio::time::timeout(Duration::from_secs(3), finished).await;
    }
}

pub(super) fn plist_bytes(dict: Dictionary) -> Vec<u8> {
    let mut out = Vec::new();
    plist::to_writer_binary(&mut out, &Value::Dictionary(dict)).expect("writing to a Vec cannot fail");
    out
}

fn plist_dict(msg: &Message) -> Result<Dictionary> {
    let value = crate::proto::plist::parse(&msg.body).context("response is not a valid plist")?;
    value.into_dictionary().ok_or_else(|| anyhow!("response plist is not a dictionary"))
}

fn uint(dict: &Dictionary, key: &str) -> Option<u64> {
    dict.get(key).and_then(Value::as_unsigned_integer)
}

/// Formats an AirPlay volume without locale surprises ("-12.500000").
///
/// Last line of defence: whatever the caller computed, nothing louder than
/// 0 dB (the speaker's maximum) is ever put on the wire.
pub fn volume_body(db: f32) -> Vec<u8> {
    let db = if db.is_nan() { MUTED_DB } else { db.clamp(MUTED_DB, 0.0) };
    format!("volume: {db:.6}\r\n").into_bytes()
}

fn volume_request(url: &str, db: f32) -> Request {
    Request::new("SET_PARAMETER", url).body("text/parameters", volume_body(db))
}

/// The `SET_PARAMETER` request carrying a metadata command.
fn command_request(url: &str, cmd: Command) -> Request {
    let req = Request::new("SET_PARAMETER", url);
    match cmd {
        Command::Text { info, rtptime } => req
            .header("RTP-Info", format!("rtptime={rtptime}"))
            .body("application/x-dmap-tagged", dmap::encode_track(&info)),
        Command::Artwork { mime, data, rtptime } => {
            req.header("RTP-Info", format!("rtptime={rtptime}")).body(&mime, data)
        }
        Command::Progress { start, current, end } => req
            .header("RTP-Info", format!("rtptime={start}"))
            .body("text/parameters", format!("progress: {start}/{current}/{end}\r\n").into_bytes()),
    }
}

/// Connects, pairs and sets up streaming. Returns once the speaker is ready
/// to receive audio packets.
pub async fn start(
    device: Device,
    ctx: SessionContext,
    keys: Option<DeviceKeys>,
    initial_volume: f32,
    events: mpsc::Sender<SessionEvent>,
) -> Result<SessionHandle> {
    static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dacp_id = hex::encode_upper(crate::random::bytes::<8>());
    let active_remote = u32::from_ne_bytes(crate::random::bytes()).to_string();
    let session_id = u32::from_ne_bytes(crate::random::bytes());
    let headers = vec![
        ("User-Agent".to_string(), USER_AGENT.to_string()),
        ("X-Apple-Client-Name".to_string(), ctx.client_name.clone()),
        ("DACP-ID".to_string(), dacp_id.clone()),
        ("Client-Instance".to_string(), dacp_id),
        ("Active-Remote".to_string(), active_remote),
    ];

    let mut conn = Connection::connect(device.address, headers).await?;

    // 1. Status flags.
    let info = conn.request_ok(Request::new("GET", "/info")).await?;
    let flags = plist_dict(&info).ok().and_then(|d| uint(&d, "statusFlags")).unwrap_or(device.flags);
    if flags & status::RECEIVER_BUSY != 0 {
        warn!("{}: reports busy; it may refuse the stream", device.name);
    }

    // 2. Pairing.
    let secret = match &keys {
        Some(keys) => pair::verify(&mut conn, &ctx.identity, keys).await.with_context(|| {
            format!("pair-verify with {} failed; re-pair with `airpipe pair \"{}\"`", device.name, device.name)
        })?,
        None => {
            if flags & (status::PIN_REQUIRED | status::ONE_TIME_PAIRING_REQUIRED | status::PASSWORD_REQUIRED) != 0 {
                bail!("{} requires pairing first: run `airpipe pair \"{}\"`", device.name, device.name);
            }
            pair::transient(&mut conn).await.with_context(|| format!("pairing with {}", device.name))?
        }
    };

    // 3. Encrypted from here on.
    conn.enable_encryption(ChannelCipher::new(&secret, Channel::Control));

    let local_ip = conn.local_addr()?.ip();
    // IPv4 only: discovery resolves IPv4 addresses (see discovery.rs).
    let url = format!("rtsp://{local_ip}/{session_id}");

    // 4. Session SETUP.
    let mut setup = Dictionary::new();
    setup.insert("deviceID".into(), ctx.identity.device_id.clone().into());
    setup.insert("sessionUUID".into(), crate::random::uuid().into());
    setup.insert("timingPort".into(), (ctx.timing_port as u64).into());
    setup.insert("timingProtocol".into(), "NTP".into());
    setup.insert("groupUUID".into(), ctx.group_uuid.clone().into());
    setup.insert("groupContainsGroupLeader".into(), false.into());
    setup.insert("name".into(), ctx.client_name.clone().into());
    let resp = conn
        .request_ok(Request::new("SETUP", &url).body("application/x-apple-binary-plist", plist_bytes(setup)))
        .await?;
    let event_port = uint(&plist_dict(&resp)?, "eventPort").ok_or_else(|| anyhow!("SETUP reply has no eventPort"))?;

    // 5. Event channel (speaker → us). Failure here is not fatal.
    let speaker_ip = device.address.ip();
    let connect = TcpStream::connect((speaker_ip, event_port as u16));
    let event_task = match tokio::time::timeout(Duration::from_secs(5), connect)
        .await
        .unwrap_or_else(|_| Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out")))
    {
        Ok(stream) => {
            let cipher = ChannelCipher::new(&secret, Channel::Events);
            Some(AbortOnDrop(tokio::spawn(events::run(stream, cipher, id, events.clone()))))
        }
        Err(e) => {
            warn!("{}: no event channel ({e}); speaker remote control disabled", device.name);
            None
        }
    };

    // 6. RECORD.
    conn.request_ok(Request::new("RECORD", &url)).await?;

    // 7. Stream SETUP.
    let mut stream = Dictionary::new();
    stream.insert("audioFormat".into(), 0x40000u64.into()); // ALAC/44100/16/2
    stream.insert("audioMode".into(), "default".into());
    stream.insert("controlPort".into(), (ctx.control_port as u64).into());
    stream.insert("ct".into(), 2u64.into()); // compression type: ALAC
    stream.insert("isMedia".into(), true.into());
    // latencyMax: OwnTone sends 2 s; ours may be longer with a long latency.
    let latency_max = ctx.latency_frames.max(ms_to_frames(2000));
    stream.insert("latencyMax".into(), u64::from(latency_max).into());
    let latency_min = ms_to_frames(MIN_SPEAKER_BUFFER_MS).min(ctx.latency_frames);
    stream.insert("latencyMin".into(), u64::from(latency_min).into());
    stream.insert("shk".into(), Value::Data(ctx.audio_key.to_vec()));
    stream.insert("spf".into(), (FRAMES_PER_PACKET as u64).into());
    stream.insert("sr".into(), (SAMPLE_RATE as u64).into());
    stream.insert("type".into(), 96u64.into()); // realtime
    stream.insert("supportsDynamicStreamID".into(), false.into());
    stream.insert("streamConnectionID".into(), (session_id as u64).into());
    let mut root = Dictionary::new();
    root.insert("streams".into(), Value::Array(vec![Value::Dictionary(stream)]));
    let resp = conn
        .request_ok(Request::new("SETUP", &url).body("application/x-apple-binary-plist", plist_bytes(root)))
        .await?;
    let reply = plist_dict(&resp)?;
    let stream = reply
        .get("streams")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_dictionary)
        .ok_or_else(|| anyhow!("stream SETUP reply has no streams"))?;
    let ports = StreamPorts {
        data: uint(stream, "dataPort").ok_or_else(|| anyhow!("no dataPort"))? as u16,
        control: uint(stream, "controlPort").ok_or_else(|| anyhow!("no controlPort"))? as u16,
    };
    debug!("{}: data port {}, control port {}, event port {event_port}", device.name, ports.data, ports.control);

    // 8. Volume.
    // A speaker that refuses our volume would play at whatever level it was
    // left at: do not stream to it.
    conn.request_ok(volume_request(&url, initial_volume)).await.context("speaker refused the initial volume")?;

    info!("{}: ready", device.name);
    let (tx, rx) = mpsc::channel(32);
    let (volume, volume_rx) = watch::channel(initial_volume);
    let (done, finished) = oneshot::channel();
    tokio::spawn(async move {
        let first_sound = Duration::from_secs_f64(f64::from(ctx.latency_frames) / f64::from(SAMPLE_RATE));
        actor(conn, url, id, rx, volume_rx, events, first_sound).await;
        drop(event_task); // the session is over; stop listening to the speaker
        let _ = done.send(());
    });
    Ok(SessionHandle { id, device, ports, tx, volume, finished })
}

/// Serves one session until its handle is dropped (then sends TEARDOWN) or
/// the connection fails. Volume changes come first: the latest value is
/// sent before any queued metadata.
async fn actor(
    mut conn: Connection,
    url: String,
    id: SessionId,
    mut rx: mpsc::Receiver<Command>,
    mut volume: watch::Receiver<f32>,
    events: mpsc::Sender<SessionEvent>,
    first_sound: Duration,
) {
    // Some speakers (seen on Devialet after standby) play at their own level
    // when audio starts, while still reporting ours, until the volume
    // *changes*. So once sound has started, and again a little later, we
    // send a hair lower value and then ours: two real changes, inaudible.
    let started = tokio::time::Instant::now();
    let mut reasserts = REASSERT_AFTER.iter().map(|d| started + first_sound + *d);
    let mut next_reassert = reasserts.next();
    let mut keepalive = tokio::time::interval(FEEDBACK_INTERVAL);
    // After a slow request, send one keepalive, not a burst of missed ones.
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    keepalive.tick().await;
    let result: Result<()> = async {
        loop {
            tokio::select! {
                biased;
                changed = volume.changed() => {
                    if changed.is_err() {
                        return Ok(()); // handle dropped
                    }
                    let db = *volume.borrow_and_update();
                    let resp = conn.request(volume_request(&url, db)).await?;
                    if resp.status() != Some(200) {
                        warn!("speaker rejected volume {db:.1} dB: {} {}", resp.status().unwrap_or(0), resp.reason());
                    }
                }
                _ = tokio::time::sleep_until(next_reassert.unwrap_or(started)), if next_reassert.is_some() => {
                    next_reassert = reasserts.next();
                    let db = *volume.borrow();
                    if db > MUTED_DB {
                        debug!("session {id}: re-asserting volume {db:.1} dB");
                        if let Some(nudged) = nudged_below(db) {
                            conn.request(volume_request(&url, nudged)).await?;
                        }
                        conn.request(volume_request(&url, db)).await?;
                    }
                }
                cmd = rx.recv() => match cmd {
                    None => return Ok(()), // handle dropped
                    Some(cmd) => {
                        conn.request(command_request(&url, cmd)).await?;
                    }
                },
                _ = keepalive.tick() => {
                    conn.request(Request::new("POST", "/feedback")).await?;
                }
            }
        }
    }
    .await;
    let event = match result {
        Ok(()) => {
            // Our side ended the session. An empty plist ends the whole
            // session (shairport-sync, OwnTone).
            let teardown =
                Request::new("TEARDOWN", &url).body("application/x-apple-binary-plist", plist_bytes(Dictionary::new()));
            let _ = conn.request(teardown).await;
            debug!("session {id}: torn down");
            return;
        }
        Err(e) if e.downcast_ref::<ClosedBySpeaker>().is_some() => {
            info!("session {id}: the speaker ended it (probably another device took over)");
            SessionEvent::TakenOver { session: id }
        }
        Err(e) => {
            warn!("session {id} failed: {e:#}");
            SessionEvent::Failed { session: id, error: format!("{e:#}") }
        }
    };
    let _ = events.send(event).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_is_locale_independent() {
        assert_eq!(volume_body(-12.5), b"volume: -12.500000\r\n");
        assert_eq!(volume_body(-144.0), b"volume: -144.000000\r\n");
    }

    #[test]
    fn volume_never_exceeds_speaker_maximum() {
        assert_eq!(volume_body(6.0), b"volume: 0.000000\r\n");
        assert_eq!(volume_body(f32::NAN), b"volume: -144.000000\r\n");
        assert_eq!(volume_body(f32::INFINITY), b"volume: 0.000000\r\n");
    }

    #[test]
    fn commands_become_set_parameter_requests() {
        let url = "rtsp://10.0.0.2/1";
        let req = command_request(url, Command::Progress { start: 1, current: 2, end: 3 });
        let text = String::from_utf8(req.encode(1)).unwrap();
        assert!(text.starts_with("SET_PARAMETER rtsp://10.0.0.2/1 RTSP/1.0\r\n"));
        assert!(text.contains("RTP-Info: rtptime=1\r\n"));
        assert!(text.ends_with("progress: 1/2/3\r\n"));
    }
}
