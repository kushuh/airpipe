//! The PipeWire output device (`AirPlay: <speakers>`).
//!
//! A PipeWire stream with `media.class = Audio/Sink` is exposed to the session
//! as an ordinary sink: it shows up in desktop volume controls and apps can
//! play to it. Design follows PipeWire's own `module-raop-sink`:
//!
//! * Format is fixed to S16LE / 44.1 kHz / stereo; PipeWire converts for us.
//! * Volume and mute set by the user are *not* applied to the samples
//!   (`softVolumes` forced to 1.0). They are forwarded to the speakers as
//!   AirPlay volume instead, which keeps full resolution on the wire.
//!   (With `forward_volume = false`, PipeWire scales samples as usual.)
//! * The AirPlay latency is reported as `ProcessLatency`, so players that do
//!   A/V sync (video players, browsers) delay video accordingly.
//!
//! Everything PipeWire runs on a dedicated thread; samples cross to the audio
//! engine through a lock-free ring buffer, and control events through a
//! tokio channel.

use std::thread::JoinHandle;

use anyhow::{Result, anyhow};
use pipewire as pw;
use pw::spa;
use pw::spa::pod::{Object, Pod, Property, Value, ValueArray, deserialize::PodDeserializer, serialize::PodSerializer};
use pw::spa::sys as spa_sys;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::proto::rtp::SAMPLE_RATE;

pub use crate::group::volume::SystemVolume;

enum ToSink {
    /// Set the output's volume, as a slider position (0.0..=1.0).
    SetLevel(f32),
    Quit,
}

pub struct Sink {
    tx: pw::channel::Sender<ToSink>,
    thread: Option<JoinHandle<()>>,
}

pub struct SinkConfig {
    pub node_name: String,
    pub description: String,
    /// End-to-end latency to report to PipeWire (AirPlay latency + jitter cushion).
    pub latency: std::time::Duration,
    /// Forward the user's volume to the speakers instead of scaling samples.
    /// When false, PipeWire applies the volume digitally as for any device.
    pub forward_volume: bool,
}

/// Converts PipeWire's linear channel volume to the slider position users see.
///
/// Desktop volume controls are cubic: slider = cbrt(linear). Values above
/// 1.0 (over-amplification) are capped: AirPlay has no headroom above max.
pub fn slider_level(linear: f32) -> f32 {
    linear.max(0.0).cbrt().min(1.0)
}

impl Sink {
    pub fn start(
        cfg: SinkConfig,
        ring: rtrb::Producer<i16>,
        events: mpsc::UnboundedSender<SystemVolume>,
    ) -> Result<Self> {
        let (tx, rx) = pw::channel::channel::<ToSink>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let thread = std::thread::Builder::new().name("airpipe-pipewire".into()).spawn(move || {
            let result = run(cfg, ring, events, rx, &ready_tx);
            if let Err(e) = result {
                let _ = ready_tx.send(Err(format!("{e:#}")));
            }
        })?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self { tx, thread: Some(thread) }),
            Ok(Err(e)) => Err(anyhow!("PipeWire: {e}")),
            Err(_) => Err(anyhow!("PipeWire thread exited during startup")),
        }
    }
}

impl Sink {
    /// Moves the output's volume slider. PipeWire does not report this change
    /// back to us, so the caller should record the level itself.
    pub fn set_level(&self, level: f32) {
        let _ = self.tx.send(ToSink::SetLevel(level.clamp(0.0, 1.0)));
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        let _ = self.tx.send(ToSink::Quit);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn serialize(value: &Value) -> Vec<u8> {
    PodSerializer::serialize(std::io::Cursor::new(Vec::new()), value)
        .expect("serializing to memory cannot fail")
        .0
        .into_inner()
}

fn format_param() -> Vec<u8> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::S16LE);
    info.set_rate(SAMPLE_RATE);
    info.set_channels(2);
    let mut pos = [0u32; spa::param::audio::MAX_CHANNELS];
    pos[0] = spa_sys::SPA_AUDIO_CHANNEL_FL;
    pos[1] = spa_sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(pos);
    serialize(&Value::Object(Object {
        type_: spa_sys::SPA_TYPE_OBJECT_Format,
        id: spa_sys::SPA_PARAM_EnumFormat,
        properties: info.into(),
    }))
}

fn latency_param(ns: i64) -> Vec<u8> {
    serialize(&Value::Object(Object {
        type_: spa_sys::SPA_TYPE_OBJECT_ParamProcessLatency,
        id: spa_sys::SPA_PARAM_ProcessLatency,
        properties: vec![Property::new(spa_sys::SPA_PARAM_PROCESS_LATENCY_ns, Value::Long(ns))],
    }))
}

/// User-visible volume state extracted from a Props param.
#[derive(Debug, Default, PartialEq)]
struct PropsVolume {
    channel_volumes: Option<Vec<f32>>,
    mute: Option<bool>,
    /// True if PipeWire would currently scale samples (soft volume != 1).
    soft_volume_active: bool,
}

fn parse_props(pod: &Pod) -> Option<PropsVolume> {
    let (_, Value::Object(obj)) = PodDeserializer::deserialize_any_from(pod.as_bytes()).ok()? else {
        return None;
    };
    let mut out = PropsVolume::default();
    for p in &obj.properties {
        match (p.key, &p.value) {
            (spa_sys::SPA_PROP_channelVolumes, Value::ValueArray(ValueArray::Float(v))) => {
                out.channel_volumes = Some(v.clone())
            }
            (spa_sys::SPA_PROP_mute, Value::Bool(m)) => out.mute = Some(*m),
            (spa_sys::SPA_PROP_softVolumes, Value::ValueArray(ValueArray::Float(v))) => {
                out.soft_volume_active |= v.iter().any(|&x| (x - 1.0).abs() > f32::EPSILON)
            }
            (spa_sys::SPA_PROP_softMute, Value::Bool(m)) => out.soft_volume_active |= *m,
            _ => {}
        }
    }
    Some(out)
}

/// Props that neutralize PipeWire's software scaling (the volume goes to the
/// speakers instead), optionally also setting the user-visible volume.
fn passthrough_props(channels: usize, channel_volume: Option<f32>) -> Vec<u8> {
    let mut properties = vec![
        Property::new(spa_sys::SPA_PROP_softVolumes, Value::ValueArray(ValueArray::Float(vec![1.0; channels]))),
        Property::new(spa_sys::SPA_PROP_softMute, Value::Bool(false)),
    ];
    if let Some(v) = channel_volume {
        properties.push(Property::new(
            spa_sys::SPA_PROP_channelVolumes,
            Value::ValueArray(ValueArray::Float(vec![v; channels])),
        ));
    }
    serialize(&Value::Object(Object {
        type_: spa_sys::SPA_TYPE_OBJECT_Props,
        id: spa_sys::SPA_PARAM_Props,
        properties,
    }))
}

/// Applies a Props param to the stream.
fn set_props(stream: &pw::stream::Stream, bytes: &[u8]) {
    let pod = Pod::from_bytes(bytes).expect("valid pod");
    // SAFETY: the stream pointer is valid while we hold `stream`, and the pod
    // outlives the call, which copies it.
    let res = unsafe { pw::sys::pw_stream_set_param(stream.as_raw_ptr(), spa_sys::SPA_PARAM_Props, pod.as_raw_ptr()) };
    if res < 0 {
        warn!("could not update the output's volume properties ({res})");
    }
}

struct Callbacks {
    ring: rtrb::Producer<i16>,
    forward_volume: bool,
    events: mpsc::UnboundedSender<SystemVolume>,
    volume: f32,
    mute: bool,
}

fn run(
    cfg: SinkConfig,
    ring: rtrb::Producer<i16>,
    events: mpsc::UnboundedSender<SystemVolume>,
    rx: pw::channel::Receiver<ToSink>,
    ready: &std::sync::mpsc::Sender<Result<(), String>>,
) -> Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;

    let props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Playback",
        *pw::keys::MEDIA_CLASS => "Audio/Sink",
        *pw::keys::NODE_NAME => cfg.node_name.as_str(),
        *pw::keys::NODE_DESCRIPTION => cfg.description.as_str(),
        *pw::keys::AUDIO_CHANNELS => "2",
        "audio.position" => "FL,FR",
        "device.icon-name" => "audio-speakers",
    };
    // Rc variant: the control-message handler below needs its own handle.
    let stream = pw::stream::StreamRc::new(core.clone(), "airpipe", props)?;

    let data = Callbacks { ring, forward_volume: cfg.forward_volume, events, volume: f32::NAN, mute: false };
    let _listener = stream
        .add_local_listener_with_user_data(data)
        .state_changed(|_, _, old, new| match new {
            pw::stream::StreamState::Error(e) => warn!("PipeWire stream error: {e}"),
            _ => debug!("PipeWire stream state: {old:?} -> {new:?}"),
        })
        .param_changed(|stream, d, id, param| {
            let Some(param) = param else { return };
            if id != spa_sys::SPA_PARAM_Props {
                return;
            }
            let Some(props) = parse_props(param) else { return };
            if let Some(m) = props.mute {
                d.mute = m;
            }
            if let Some(vols) = &props.channel_volumes {
                if !vols.is_empty() {
                    d.volume = vols.iter().sum::<f32>() / vols.len() as f32;
                }
                if d.forward_volume && props.soft_volume_active {
                    set_props(stream, &passthrough_props(vols.len().max(1), None));
                }
            }
            if !d.volume.is_nan() {
                let _ = d.events.send(SystemVolume { level: slider_level(d.volume), muted: d.mute });
            }
        })
        .process(|stream, d| {
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else { return };
            let (offset, size) = (data.chunk().offset() as usize, data.chunk().size() as usize);
            let Some(mem) = data.data() else { return };
            let bytes = &mem[offset.min(mem.len())..(offset + size).min(mem.len())];
            let ring = &mut d.ring;
            let samples = bytes.len() / 2;
            let Ok(chunk) = ring.write_chunk_uninit(samples.min(ring.slots())) else { return };
            let n = chunk.len();
            chunk.fill_from_iter(bytes.as_chunks::<2>().0.iter().take(n).map(|b| i16::from_le_bytes([b[0], b[1]])));
        })
        .register()?;

    let format = format_param();
    let latency = latency_param(cfg.latency.as_nanos() as i64);
    let mut params = [Pod::from_bytes(&format).expect("valid pod"), Pod::from_bytes(&latency).expect("valid pod")];
    stream.connect(
        spa::utils::Direction::Input,
        None,
        pw::stream::StreamFlags::AUTOCONNECT
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::RT_PROCESS,
        &mut params,
    )?;

    let _rx = rx.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        let stream = stream.clone();
        let forward_volume = cfg.forward_volume;
        move |msg| match msg {
            ToSink::SetLevel(level) => {
                // Sliders are cubic; PipeWire stores linear amplitude.
                let linear = level * level * level;
                if forward_volume {
                    // Volume and "no software scaling" in one update, or the
                    // samples would be attenuated as well as the speakers
                    // (like module-raop-sink, which always pairs them).
                    set_props(&stream, &passthrough_props(2, Some(linear)));
                } else if let Err(e) = stream.set_control(spa_sys::SPA_PROP_channelVolumes, &[linear, linear]) {
                    warn!("could not set output volume: {e}");
                }
            }
            ToSink::Quit => mainloop.quit(),
        }
    });

    info!("PipeWire output {:?} created", cfg.description);
    let _ = ready.send(Ok(()));
    mainloop.run();
    let _ = stream.disconnect();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slider_level_is_cubic_and_capped() {
        assert_eq!(slider_level(1.0), 1.0);
        assert_eq!(slider_level(0.0), 0.0);
        assert!((slider_level(0.125) - 0.5).abs() < 1e-6);
        assert_eq!(slider_level(8.0), 1.0, "over-amplification is capped");
        assert_eq!(slider_level(-1.0), 0.0);
    }

    #[test]
    fn props_roundtrip() {
        let bytes = serialize(&Value::Object(Object {
            type_: spa_sys::SPA_TYPE_OBJECT_Props,
            id: spa_sys::SPA_PARAM_Props,
            properties: vec![
                Property::new(spa_sys::SPA_PROP_channelVolumes, Value::ValueArray(ValueArray::Float(vec![0.5, 0.25]))),
                Property::new(spa_sys::SPA_PROP_mute, Value::Bool(true)),
                Property::new(spa_sys::SPA_PROP_softVolumes, Value::ValueArray(ValueArray::Float(vec![0.5, 0.25]))),
            ],
        }));
        let parsed = parse_props(Pod::from_bytes(&bytes).unwrap()).unwrap();
        assert_eq!(parsed.channel_volumes, Some(vec![0.5, 0.25]));
        assert_eq!(parsed.mute, Some(true));
        assert!(parsed.soft_volume_active);

        let pass = parse_props(Pod::from_bytes(&passthrough_props(2, None)).unwrap()).unwrap();
        assert!(!pass.soft_volume_active);
        let set = parse_props(Pod::from_bytes(&passthrough_props(2, Some(0.125))).unwrap()).unwrap();
        assert!(!set.soft_volume_active, "setting a volume never enables software scaling");
        assert_eq!(set.channel_volumes, Some(vec![0.125, 0.125]));
    }
}
