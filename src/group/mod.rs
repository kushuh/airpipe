//! The group controller: one PipeWire output feeding one or more speakers.
//!
//! A "speaker" is one discovered AirPlay target. A stereo pair or a vendor
//! multi-room setup that advertises a single target is one speaker.
//!
//! Lifecycle of a speaker ("member"):
//!
//! ```text
//!   idle ──audio starts──▶ connecting ──ok──▶ streaming ──idle timeout──▶ idle
//!                              │                  │  │
//!                              └──error──▶ waiting (retry with backoff)
//!                                                    │
//!         speaker ended the session (another device took over)
//!                                                    ▼
//!                       yielded ──audio stops──▶ idle (reconnects on next sound)
//! ```
//!
//! Speakers are only held while audible audio flows, so they stay free for
//! other senders (phones, other computers) the rest of the time, and airpipe
//! never takes a speaker back from another device mid-playback.
//!
//! [`run`] sets everything up, then `Group` reacts to one event at a time;
//! each `on_*` method handles one event source. Requests from `airpipe
//! status`, `reload` (also sent by `use`, `add`, `remove`)... arrive through
//! the control socket ([`crate::control`]). Volume rules live in [`volume`].

pub mod volume;

use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{debug, info, warn};

use crate::airplay::events::RemoteCommand;
use crate::airplay::session::{self, Command, SessionContext, SessionEvent, SessionHandle, SessionId};
use crate::config::{Config, StartVolume};
use crate::control::{self, Reply, Request};
use crate::discovery::{self, Device, feature};
use crate::mpris::{self, Artwork, MediaEvent};
use crate::proto::dmap::TrackInfo;
use crate::proto::ntp::Clock;
use crate::proto::rtp::SAMPLE_RATE;
use crate::sink::{Sink, SinkConfig};
use crate::store::{DeviceSettings, Store};
use crate::stream::timing::{AllowedPeers, PeerPermit, TimingServer};
use crate::stream::{self, Engine, EngineConfig, TargetId};
use volume::{MIN_DB, MUTED_DB, SystemVolume, VolumeInputs};

/// Seconds to wait before reconnecting, by number of consecutive failures.
const RETRY_BACKOFF: [u64; 5] = [2, 5, 10, 30, 60];
/// How often connection management runs.
const TICK: Duration = Duration::from_millis(250);
/// How often playback progress is sent to speakers.
const PROGRESS_INTERVAL: Duration = Duration::from_secs(5);
/// Positions older than this are not used for progress.
const STALE_POSITION: Duration = Duration::from_secs(3);
/// After start-up the desktop may restore a remembered volume onto our new
/// output, and a volume report from before a speaker change may still be
/// queued. During this window we re-assert the level we set.
const START_VOLUME_WINDOW: Duration = Duration::from_secs(3);
/// Pending remote-control commands from speakers; extra ones are dropped.
const REMOTE_QUEUE: usize = 8;

/// What a group needs to run. The speakers are `config.output.speakers`.
pub struct GroupConfig {
    /// Effective settings (config.toml plus command-line flags).
    pub config: Config,
    /// Where config.toml is, to re-read the speakers on `reload`.
    pub config_path: PathBuf,
    /// Extra cap on top of every speaker's own (`--max-volume`), as a slider position.
    pub volume_cap: Option<f32>,
}

/// Applied to the new speakers on reload; other settings need a restart.
const RELOAD_NOTE: &str = "speakers and devices.toml volumes applied; latency offsets apply on the next \
     connection, other config.toml settings after `systemctl --user restart airpipe`";

/// The parts shared by `airpipe run` and `airpipe tone`: the clock service,
/// the audio engine, and what every session needs to know.
pub struct Pipeline {
    pub engine: Engine,
    pub peers: AllowedPeers,
    pub ctx: SessionContext,
    _timing: TimingServer,
}

impl Pipeline {
    /// Starts the timing service and the audio engine reading from `source`.
    pub fn start(cfg: &Config, store: &Store, source: rtrb::Consumer<i16>) -> Result<Self> {
        let clock = Clock::new();
        let peers = AllowedPeers::default();
        let timing = TimingServer::start(cfg.network.timing_port, clock, peers.clone())?;
        let (latency_ms, jitter_frames) = cfg.latency();
        let latency_frames = stream::ms_to_frames(latency_ms);
        let engine = Engine::start(
            source,
            clock,
            EngineConfig { latency_frames, control_port: cfg.network.control_port, jitter_frames },
        )?;
        let ctx = SessionContext {
            identity: store.identity()?,
            client_name: cfg.network.client_name.clone(),
            timing_port: cfg.network.timing_port,
            control_port: cfg.network.control_port,
            audio_key: engine.audio_key(),
            latency_frames,
            group_uuid: crate::random::uuid(),
        };
        Ok(Self { engine, peers, ctx, _timing: timing })
    }

    /// Connects to a speaker: allows it on the timing port (the speaker asks
    /// for timing during setup), then runs the session setup.
    pub async fn connect(
        &self,
        device: Device,
        keys: Option<crate::crypto::pairing::DeviceKeys>,
        volume_db: f32,
        events: mpsc::Sender<SessionEvent>,
    ) -> Result<(SessionHandle, PeerPermit)> {
        connect(&self.peers, &self.ctx, device, keys, volume_db, events).await
    }
}

async fn connect(
    peers: &AllowedPeers,
    ctx: &SessionContext,
    device: Device,
    keys: Option<crate::crypto::pairing::DeviceKeys>,
    volume_db: f32,
    events: mpsc::Sender<SessionEvent>,
) -> Result<(SessionHandle, PeerPermit)> {
    let permit = peers.allow(device.address.ip());
    let session = session::start(device, ctx.clone(), keys, volume_db, events).await?;
    Ok((session, permit))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemberState {
    Idle,
    Connecting,
    Streaming,
    Waiting {
        until: Instant,
    },
    /// Another device took the speaker over; wait for the next audio onset.
    Yielded,
}

struct Member {
    name: String,
    device: Option<Device>,
    /// From devices.toml; conservative defaults until the speaker is resolved.
    settings: DeviceSettings,
    gain_db: f32,
    state: MemberState,
    session: Option<SessionHandle>,
    /// Lets this speaker use our timing port while connected; revoked on drop.
    permit: Option<PeerPermit>,
    target: Option<TargetId>,
    failures: usize,
    /// The current connection attempt; results of older ones are dropped.
    attempt: u64,
    last_error: Option<String>,
}

impl Member {
    fn new(name: String) -> Self {
        Self {
            name,
            device: None,
            settings: DeviceSettings::conservative(),
            gain_db: 0.0,
            state: MemberState::Idle,
            session: None,
            permit: None,
            target: None,
            failures: 0,
            attempt: 0,
            last_error: None,
        }
    }

    fn has_session(&self, id: SessionId) -> bool {
        self.session.as_ref().is_some_and(|s| s.id == id)
    }
}

/// Result of a background connection attempt.
struct Connected {
    name: String,
    /// Which attempt this is (see `Member::attempt`).
    attempt: u64,
    result: Result<(DeviceSettings, SessionHandle, PeerPermit)>,
}

/// A new speaker list and what discovery found for it (see `Group::on_resolved`).
struct Resolved {
    /// Which reload this answers (see `Group::reloads`).
    reload: u64,
    speakers: Vec<String>,
    found: Vec<Device>,
    reply: Option<oneshot::Sender<Reply>>,
}

/// Looks the speakers up on the network. Missing ones are found later, when
/// connecting.
async fn resolve(reload: u64, speakers: Vec<String>, reply: Option<oneshot::Sender<Reply>>) -> Resolved {
    let mut found = Vec::new();
    if !speakers.is_empty() {
        match discovery::scan(Duration::from_secs(3)).await {
            Ok(scan) => found = scan.devices,
            Err(e) => warn!("{e:#}"),
        }
    }
    Resolved { reload, speakers: unique(speakers), found, reply }
}

/// Each speaker once, even if listed twice (names are case-insensitive).
fn unique(mut speakers: Vec<String>) -> Vec<String> {
    let mut seen = Vec::<String>::new();
    speakers.retain(|name| {
        let new = !seen.iter().any(|s| s.eq_ignore_ascii_case(name));
        seen.push(name.clone());
        new
    });
    speakers
}

/// The output's name in desktop sound settings.
fn output_description(speakers: &[String]) -> String {
    if speakers.is_empty() {
        "AirPlay (no speaker selected)".into()
    } else {
        format!("AirPlay: {}", speakers.join(" + "))
    }
}

#[derive(Default)]
struct NowPlaying {
    info: Option<TrackInfo>,
    artwork: Option<Artwork>,
    length: Option<Duration>,
    position: Option<(Duration, Instant)>,
}

/// Sets up the audio pipeline and runs the group until `airpipe stop`,
/// Ctrl-C or SIGTERM. `control` is bound first by the caller, so a second
/// airpipe never gets this far.
pub async fn run(gc: GroupConfig, store: Store, control: control::Server) -> Result<()> {
    let cfg = &gc.config;
    // Resolve speakers first: their settings decide the start-up volume, and
    // connecting on the first sound is faster when the address is known.
    let resolved = resolve(0, cfg.output.speakers.clone(), None).await;

    // The ring holds up to 2 s; the jitter buffer keeps it near its target.
    let (producer, consumer) = rtrb::RingBuffer::<i16>::new(SAMPLE_RATE as usize * 2 * 2);
    let pipeline = Pipeline::start(cfg, &store, consumer)?;
    let (latency_ms, jitter_frames) = cfg.latency();

    let (volume_tx, mut volume_rx) = mpsc::unbounded_channel();
    let jitter = Duration::from_secs_f64(jitter_frames as f64 / f64::from(SAMPLE_RATE));
    let sink = Sink::start(
        SinkConfig {
            node_name: cfg.output.node_name.clone(),
            description: output_description(&resolved.speakers),
            latency: Duration::from_millis(latency_ms.into()) + jitter,
            forward_volume: cfg.volume.follow_system,
        },
        producer,
        volume_tx,
    )?;

    let (media_tx, mut media_rx) = mpsc::unbounded_channel();
    let (streaming_tx, streaming_rx) = watch::channel(false);
    let mut remote_tx = None; // speaker buttons go to MPRIS, so only with metadata on
    if cfg.metadata.enabled {
        let (tx, remote_rx) = mpsc::channel(REMOTE_QUEUE);
        remote_tx = Some(tx);
        tokio::spawn(async move {
            if let Err(e) = mpris::run(media_tx, remote_rx, streaming_rx).await {
                warn!("now-playing metadata disabled: {e:#}");
            }
        });
    }

    let (session_tx, mut session_rx) = mpsc::channel(64);
    let (connected_tx, mut connected_rx) = mpsc::unbounded_channel();
    let (resolved_tx, mut resolved_rx) = mpsc::unbounded_channel();
    let (mut requests, control_task) = control.spawn();

    let mut group = Group {
        gc,
        store,
        pipeline,
        sink,
        members: Vec::new(),
        session_tx,
        connected_tx,
        resolved_tx,
        remote_tx,
        streaming_tx,
        system: None,
        start_level: None,
        last_audible_packets: 0,
        last_audio_at: None,
        now_playing: NowPlaying::default(),
        last_progress: Instant::now(),
        attempts: 0,
        reloads: 0,
    };
    group.on_resolved(resolved);

    match group.members.len() {
        0 => info!("ready, but no speaker selected: run `airpipe use <speaker>` (see `airpipe list`)"),
        _ => info!("ready: play something to the {:?} output", output_description(&group.speaker_names())),
    }
    // Ctrl-C in a terminal, SIGTERM from systemd: both end sessions cleanly.
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut ticker = tokio::time::interval(TICK);
    let mut stop_reply = None;
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = tokio::signal::ctrl_c() => break,
            _ = sigterm.recv() => break,
            Some(v) = volume_rx.recv() => group.on_volume(v),
            Some(ev) = session_rx.recv() => group.on_session(ev),
            Some(c) = connected_rx.recv() => group.on_connected(c),
            Some(r) = resolved_rx.recv() => group.on_resolved(r),
            Some(ev) = media_rx.recv() => group.on_media(ev),
            Some((request, reply)) = requests.recv() => {
                if request == Request::Stop {
                    stop_reply = Some(reply);
                    break;
                }
                group.on_request(request, reply);
            }
        }
        group.manage_connections();
    }

    info!("shutting down");
    group.release_all().await;
    // Removes the output (giving the default output back) before answering
    // `airpipe stop`, so the sound has moved when the command returns.
    drop(group);
    if let Some(reply) = stop_reply {
        let _ = reply.send(Reply::ok("airpipe stopped"));
    }
    drop(requests);
    let _ = control_task.await;
    Ok(())
}

struct Group {
    gc: GroupConfig,
    store: Store,
    pipeline: Pipeline,
    sink: Sink,
    members: Vec<Member>,
    session_tx: mpsc::Sender<SessionEvent>,
    connected_tx: mpsc::UnboundedSender<Connected>,
    resolved_tx: mpsc::UnboundedSender<Resolved>,
    /// Speaker buttons to forward to MPRIS (None when metadata is disabled).
    remote_tx: Option<mpsc::Sender<RemoteCommand>>,
    /// Whether any speaker is streaming (MPRIS only polls while true).
    streaming_tx: watch::Sender<bool>,
    /// Last known system volume of our output.
    system: Option<SystemVolume>,
    /// Slider level set by `apply_start_volume`, and when; re-asserted
    /// during the start window.
    start_level: Option<(f32, Instant)>,
    last_audible_packets: u64,
    last_audio_at: Option<Instant>,
    now_playing: NowPlaying,
    last_progress: Instant,
    /// Connection attempts started so far.
    attempts: u64,
    /// Reloads requested so far; only the latest one is applied.
    reloads: u64,
}

impl Group {
    fn volume_inputs(&self, m: &Member) -> VolumeInputs {
        VolumeInputs {
            settings: m.settings,
            gain_db: m.gain_db,
            cap: self.gc.volume_cap,
            follow_system: self.gc.config.volume.follow_system,
        }
    }

    fn send_volume(&self, m: &Member) {
        if let Some(s) = &m.session {
            s.set_volume(self.volume_inputs(m).airplay_db(self.system));
        }
    }

    /// Puts the slider at the speakers' default volume (with several
    /// speakers, the lowest): at start-up, and when speakers are added. Once
    /// the desktop has reported a volume, it only ever lowers it, and keeps
    /// mute.
    fn apply_start_volume(&mut self) {
        let v = &self.gc.config.volume;
        if !v.follow_system || v.start != StartVolume::DeviceDefault || self.members.is_empty() {
            return;
        }
        let mut level = self.members.iter().map(|m| self.volume_inputs(m).default_level()).fold(1.0f32, f32::min);
        let mut muted = false;
        if let Some(now) = self.system {
            if now.level <= level {
                return;
            }
            level = level.min(now.level);
            muted = now.muted;
        }
        info!("setting output volume to {:.0}% (the speakers' default volume)", level * 100.0);
        self.sink.set_level(level);
        self.start_level = Some((level, Instant::now()));
        // PipeWire does not report a volume we set ourselves, so record it.
        self.system = Some(SystemVolume { level, muted });
    }

    fn on_volume(&mut self, new: SystemVolume) {
        if let Some((want, at)) = self.start_level
            && at.elapsed() < START_VOLUME_WINDOW
            && (new.level - want).abs() > 0.01
        {
            debug!("output volume restored to {:.0}%, re-applying {:.0}%", new.level * 100.0, want * 100.0);
            self.sink.set_level(want);
            return;
        }
        if self.system != Some(new) {
            debug!("system volume {:.0}%{}", new.level * 100.0, if new.muted { " (muted)" } else { "" });
            self.system = Some(new);
            for m in &self.members {
                self.send_volume(m);
            }
        }
    }

    fn on_session(&mut self, ev: SessionEvent) {
        let session = ev.session();
        // Events from sessions we already let go of are stale: ignore them.
        let Some(i) = self.members.iter().position(|m| m.has_session(session)) else {
            debug!("ignoring event from an old session: {ev:?}");
            return;
        };
        match ev {
            SessionEvent::Remote { command, .. } => {
                if let Some(tx) = &self.remote_tx
                    && tx.try_send(command).is_err()
                {
                    debug!("too many remote commands, dropping one");
                }
            }
            SessionEvent::TakenOver { .. } => {
                self.release(i);
                self.members[i].state = MemberState::Yielded;
            }
            SessionEvent::Failed { error, .. } => {
                self.release(i);
                schedule_retry(&mut self.members[i], error);
            }
        }
    }

    fn on_connected(&mut self, c: Connected) {
        let i = self.member_index(&c.name);
        let current = |m: &Member| m.state == MemberState::Connecting && m.attempt == c.attempt;
        let Some(i) = i.filter(|&i| current(&self.members[i])) else {
            // Removed (perhaps re-added) or reset while connecting: drop the
            // result. Dropping the session ends it; dropping the permit closes
            // the timing port to it.
            return;
        };
        let (settings, session, permit) = match c.result {
            Ok(ok) => ok,
            Err(e) => return schedule_retry(&mut self.members[i], format!("{e:#}")),
        };
        // devices.toml may have been reloaded while connecting: use it as it is now.
        let settings = self.store.device_settings(&session.device).unwrap_or(settings);
        let (ip, p) = (session.device.address.ip(), session.ports);
        let target = match self.pipeline.engine.add_target(ip, p.data, p.control, settings.latency_offset_ms) {
            Ok(t) => t,
            Err(e) => return schedule_retry(&mut self.members[i], format!("{e:#}")),
        };
        info!("{}: streaming", c.name);
        send_now_playing(&session, &self.now_playing, &self.pipeline.engine);
        let m = &mut self.members[i];
        m.device = Some(session.device.clone());
        m.settings = settings;
        m.target = Some(target);
        m.session = Some(session);
        m.permit = Some(permit);
        m.state = MemberState::Streaming;
        m.failures = 0;
        m.last_error = None;
        // The system volume may have changed while connecting.
        self.send_volume(&self.members[i]);
    }

    fn on_media(&mut self, ev: MediaEvent) {
        let sessions = || self.members.iter().filter_map(|m| m.session.as_ref());
        match ev {
            MediaEvent::Track { info, artwork, length } => {
                info!("now playing: {}", describe(&info));
                self.now_playing = NowPlaying { info: Some(info), artwork, length, position: None };
                // Let the next position update through right away.
                self.last_progress = Instant::now() - PROGRESS_INTERVAL;
                for s in sessions() {
                    send_now_playing(s, &self.now_playing, &self.pipeline.engine);
                }
            }
            MediaEvent::Cleared => {
                debug!("now playing: nothing identifiable");
                self.now_playing = NowPlaying::default();
                for s in sessions().filter(|s| s.device.has_feature(feature::METADATA_TEXT)) {
                    s.send(Command::Text {
                        info: TrackInfo::default(),
                        rtptime: self.pipeline.engine.playing_rtptime(),
                    });
                }
            }
            MediaEvent::Position(pos) => {
                self.now_playing.position = Some((pos, Instant::now()));
                if self.last_progress.elapsed() >= PROGRESS_INTERVAL {
                    self.last_progress = Instant::now();
                    for s in sessions() {
                        send_progress(s, &self.now_playing, &self.pipeline.engine);
                    }
                }
            }
        }
    }

    /// Handles a request from the control socket (`Stop` is handled by the
    /// main loop).
    fn on_request(&mut self, request: Request, reply: oneshot::Sender<Reply>) {
        let answer = match request {
            Request::Stop => return,
            Request::Status => Reply::ok(self.status()),
            Request::Gain { speaker, db } => match self.member_index(&speaker) {
                None => {
                    Reply::error(format!("{speaker:?} is not one of the speakers in use ({})", self.speaker_list()))
                }
                Some(i) => {
                    let m = &mut self.members[i];
                    m.gain_db = db.clamp(MIN_DB, 0.0);
                    let text = format!("{}: gain {:.1} dB until airpipe restarts", m.name, m.gain_db);
                    self.send_volume(&self.members[i]);
                    Reply::ok(text)
                }
            },
            Request::Reload => match Config::load(&self.gc.config_path) {
                Err(e) => Reply::error(format!("{e:#}")),
                Ok(cfg) => {
                    // Discovery takes a few seconds: answer when it is done.
                    self.reloads += 1;
                    let (tx, reload) = (self.resolved_tx.clone(), self.reloads);
                    tokio::spawn(async move {
                        let _ = tx.send(resolve(reload, cfg.output.speakers, Some(reply)).await);
                    });
                    return;
                }
            },
        };
        let _ = reply.send(answer);
    }

    /// Switches to a new speaker list: removed speakers are released, new
    /// ones connect on the next sound, and every speaker's devices.toml
    /// settings are re-read.
    fn on_resolved(&mut self, r: Resolved) {
        if r.reload != self.reloads {
            // A newer reload (e.g. a quick `use` then `remove`) is in flight and wins.
            if let Some(reply) = r.reply {
                let _ = reply.send(Reply::ok("replaced by a newer change; `airpipe status` shows the result"));
            }
            return;
        }
        let mut text = String::new();
        let mut i = 0;
        while i < self.members.len() {
            if r.speakers.iter().any(|s| s.eq_ignore_ascii_case(&self.members[i].name)) {
                i += 1;
            } else {
                self.release(i);
                let _ = writeln!(text, "{}: removed", self.members.remove(i).name);
            }
        }
        let mut added = false;
        for name in &r.speakers {
            let i = self.member_index(name).unwrap_or_else(|| {
                added = true;
                self.members.push(Member::new(name.clone()));
                self.members.len() - 1
            });
            let m = &mut self.members[i];
            if m.session.is_none()
                && let Some(d) = r.found.iter().find(|d| d.name.eq_ignore_ascii_case(name))
            {
                m.device = Some(d.clone());
            }
            match &m.device {
                None => {
                    let _ = writeln!(text, "{name}: not on the network right now; airpipe connects when it shows up");
                }
                Some(d) => match self.store.device_settings(d) {
                    Ok(s) => m.settings = s,
                    Err(e) => {
                        let _ = writeln!(text, "{name}: {e:#}");
                    }
                },
            }
        }
        self.sink.set_description(&output_description(&self.speaker_names()));
        if added {
            self.apply_start_volume();
        }
        for m in &self.members {
            self.send_volume(m);
        }
        if let Some(reply) = r.reply {
            match self.members.len() {
                0 => text.push_str("no speaker selected: run `airpipe use <speaker>`\n"),
                _ => {
                    let _ = writeln!(text, "playing to: {}", self.speaker_list());
                }
            }
            text.push_str(RELOAD_NOTE);
            let _ = reply.send(Reply::ok(text));
        }
    }

    /// Connects speakers while audible audio flows, releases them when it stops.
    fn manage_connections(&mut self) {
        let packets = self.pipeline.engine.audible_packets();
        if packets != self.last_audible_packets {
            self.last_audible_packets = packets;
            self.last_audio_at = Some(Instant::now());
        }
        let audio_flowing = self.last_audio_at.is_some_and(|t| t.elapsed() < self.gc.config.idle_timeout());
        let now = Instant::now();

        for i in 0..self.members.len() {
            match self.members[i].state {
                MemberState::Idle if audio_flowing => self.start_connect(i),
                MemberState::Waiting { until } if audio_flowing && now >= until => self.start_connect(i),
                MemberState::Waiting { .. } | MemberState::Yielded if !audio_flowing => {
                    self.members[i].state = MemberState::Idle;
                }
                MemberState::Streaming if !audio_flowing => {
                    let secs = self.gc.config.audio.idle_timeout_secs;
                    info!("{}: idle for {secs}s, releasing speaker", self.members[i].name);
                    self.release(i);
                    self.members[i].state = MemberState::Idle;
                }
                _ => {}
            }
        }
        let streaming = self.members.iter().any(|m| m.state == MemberState::Streaming);
        self.streaming_tx.send_if_modified(|v| std::mem::replace(v, streaming) != streaming);
    }

    /// Starts connecting in the background; the outcome arrives in `on_connected`.
    fn start_connect(&mut self, i: usize) {
        let inputs = self.volume_inputs(&self.members[i]);
        let system = self.system;
        self.attempts += 1;
        let attempt = self.attempts;
        let m = &mut self.members[i];
        m.state = MemberState::Connecting;
        m.attempt = attempt;
        info!("{}: connecting", m.name);
        let name = m.name.clone();
        let known = m.device.clone();
        let (peers, ctx) = (self.pipeline.peers.clone(), self.pipeline.ctx.clone());
        let (store, events, results) = (self.store.clone(), self.session_tx.clone(), self.connected_tx.clone());
        tokio::spawn(async move {
            let result = async {
                let device = match known {
                    Some(d) => d,
                    None => discovery::find(&name, Duration::from_secs(5)).await?,
                };
                // Fresh settings: the user may have edited devices.toml.
                let settings = store.device_settings(&device)?;
                let volume = VolumeInputs { settings, ..inputs }.airplay_db(system);
                let keys = store.keys_for(&device)?;
                let (session, permit) = connect(&peers, &ctx, device, keys, volume, events).await?;
                Ok((settings, session, permit))
            }
            .await;
            let _ = results.send(Connected { name, attempt, result });
        });
    }

    /// Stops streaming to a member at once and ends its session in the
    /// background, so a slow speaker never blocks the event loop.
    fn release(&mut self, i: usize) {
        let m = &mut self.members[i];
        if let Some(t) = m.target.take() {
            self.pipeline.engine.remove_target(t);
        }
        if let Some(s) = m.session.take() {
            tokio::spawn(s.teardown());
        }
        m.permit = None;
    }

    /// Ends every session and waits (briefly) for the speakers to acknowledge.
    async fn release_all(&mut self) {
        let mut teardowns = Vec::new();
        for m in self.members.iter_mut() {
            if let Some(t) = m.target.take() {
                self.pipeline.engine.remove_target(t);
            }
            if let Some(s) = m.session.take() {
                teardowns.push(s.teardown());
            }
            m.permit = None;
        }
        futures_util::future::join_all(teardowns).await;
    }

    fn member_index(&self, name: &str) -> Option<usize> {
        self.members.iter().position(|m| m.name.eq_ignore_ascii_case(name))
    }

    fn speaker_names(&self) -> Vec<String> {
        self.members.iter().map(|m| m.name.clone()).collect()
    }

    fn speaker_list(&self) -> String {
        match self.members.len() {
            0 => "none".into(),
            _ => self.speaker_names().join(", "),
        }
    }

    fn status(&self) -> String {
        let mut out = String::new();
        let audio = match self.last_audio_at {
            Some(t) => format!("last audio {}s ago", t.elapsed().as_secs()),
            None => "no audio yet".into(),
        };
        let vol = match (self.gc.config.volume.follow_system, self.system) {
            (false, _) => "fixed per-speaker volumes (follow_system = false)".to_string(),
            (true, Some(v)) => format!("volume {:.0}%{}", v.level * 100.0, if v.muted { " (muted)" } else { "" }),
            (true, None) => "volume not reported by the desktop yet".to_string(),
        };
        let _ = writeln!(out, "output {:?}: {vol}, {audio}", output_description(&self.speaker_names()));
        if self.members.is_empty() {
            out.push_str("no speaker selected: run `airpipe use <speaker>` (see `airpipe list`)\n");
        }
        for m in &self.members {
            let state = match m.state {
                MemberState::Idle => "idle".to_string(),
                MemberState::Connecting => "connecting".to_string(),
                MemberState::Streaming => "streaming".to_string(),
                MemberState::Yielded => "in use by another device".to_string(),
                MemberState::Waiting { until } => {
                    format!("retry in {}s", until.saturating_duration_since(Instant::now()).as_secs())
                }
            };
            let v = self.volume_inputs(m);
            let db = v.airplay_db(self.system);
            let playing =
                if db == MUTED_DB { "muted".to_string() } else { format!("{:.0}%", volume::db_to_level(db) * 100.0) };
            let addr = m.device.as_ref().map(|d| d.address.to_string()).unwrap_or_default();
            let _ = write!(
                out,
                "  {:<20} {:<24} {playing:>5} (default {:.0}%, max {:.0}%",
                m.name,
                state,
                m.settings.default_volume * 100.0,
                v.ceiling_level() * 100.0
            );
            if m.gain_db != 0.0 {
                let _ = write!(out, ", gain {:.1} dB", m.gain_db);
            }
            let _ = writeln!(out, ")  {addr}");
            if let Some(e) = &m.last_error {
                let _ = writeln!(out, "  {:<20} last error: {e}", "");
            }
        }
        let _ = write!(out, "settings: {} and {}", self.gc.config_path.display(), self.store.path().display());
        out
    }
}

fn schedule_retry(m: &mut Member, error: String) {
    let delay = RETRY_BACKOFF[m.failures.min(RETRY_BACKOFF.len() - 1)];
    warn!("{}: {error}; retrying in {delay}s", m.name);
    m.failures += 1;
    m.last_error = Some(error);
    m.state = MemberState::Waiting { until: Instant::now() + Duration::from_secs(delay) };
    // Rediscover next time: the speaker may have changed address.
    m.device = None;
}

fn describe(info: &TrackInfo) -> String {
    match (&info.artist, &info.title) {
        (Some(a), Some(t)) => format!("{a} - {t}"),
        (None, Some(t)) => t.clone(),
        _ => "(unknown)".into(),
    }
}

/// Sends whatever metadata the speaker advertises support for.
fn send_now_playing(s: &SessionHandle, np: &NowPlaying, engine: &Engine) {
    let Some(info) = &np.info else { return };
    let rtptime = engine.playing_rtptime();
    if s.device.has_feature(feature::METADATA_TEXT) {
        s.send(Command::Text { info: info.clone(), rtptime });
    }
    if let Some(art) = &np.artwork
        && s.device.has_feature(feature::METADATA_ARTWORK)
    {
        s.send(Command::Artwork { mime: art.mime.to_string(), data: art.data.clone(), rtptime });
    }
    send_progress(s, np, engine);
}

/// Progress is expressed as RTP timestamps: track start, now, track end.
fn send_progress(s: &SessionHandle, np: &NowPlaying, engine: &Engine) {
    if !s.device.has_feature(feature::METADATA_PROGRESS) {
        return;
    }
    let (Some(length), Some((pos, at))) = (np.length, np.position) else { return };
    if at.elapsed() > STALE_POSITION {
        return; // the player stopped reporting; do not extrapolate for hours
    }
    let current = engine.playing_rtptime();
    let start = current.wrapping_sub(stream::duration_to_rtp_frames(pos + at.elapsed()));
    s.send(Command::Progress { start, current, end: start.wrapping_add(stream::duration_to_rtp_frames(length)) });
}
