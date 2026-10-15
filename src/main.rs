use std::io::Write;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use tokio::sync::mpsc;

use airpipe::airplay::connection::{Connection, USER_AGENT};
use airpipe::airplay::session::SessionEvent;
use airpipe::config::{self, Config, LatencyProfile};
use airpipe::control::{self, Reply, Request};
use airpipe::discovery::{self, Device, feature};
use airpipe::group::volume::{self, VolumeInputs};
use airpipe::group::{self, GroupConfig, Pipeline};
use airpipe::proto::rtp::SAMPLE_RATE;
use airpipe::proto::rtsp;
use airpipe::store::{DeviceSettings, Store};

#[derive(Parser)]
#[command(
    version,
    about = "Stream Linux system audio from PipeWire to AirPlay 2 speakers (sender only)",
    after_help = "airpipe runs in the background (`systemctl --user enable --now airpipe.service`, or \
                  `airpipe run` in a terminal); status, use, add, remove, gain, reload and stop talk to it.\n\
                  Settings: ~/.config/airpipe/config.toml (global) and devices.toml (per speaker). \
                  Run `airpipe config` to see paths and effective values."
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List AirPlay speakers on the network.
    List,
    /// Show what a speaker reports about itself (GET /info).
    Info { speaker: String },
    /// Pair with a speaker that requires a PIN or password.
    Pair {
        speaker: String,
        /// Device password, for speakers set to "require password". If not
        /// given, the speaker is asked to display a PIN, which you type in.
        #[arg(long)]
        password: Option<String>,
    },
    /// Forget a speaker's pairing keys (its settings are kept).
    Unpair { speaker: String },
    /// Choose the speakers to play to (saved in config.toml). Several
    /// speakers play in sync.
    Use {
        #[arg(required = true)]
        speakers: Vec<String>,
    },
    /// Add a speaker to the ones in use.
    Add { speaker: String },
    /// Stop using a speaker.
    Remove { speaker: String },
    /// Show the speakers in use, their state and volume.
    Status,
    /// Make one speaker of a group quieter, in dB (e.g. -6), until airpipe
    /// restarts. To keep it, lower its default_volume or max_volume in
    /// devices.toml.
    Gain {
        speaker: String,
        #[arg(allow_hyphen_values = true)]
        db: f32,
    },
    /// Apply changes made by hand to config.toml (speakers) and devices.toml.
    Reload,
    /// Stop the running airpipe (sound goes back to your previous output).
    Stop,
    /// Run airpipe: create the "AirPlay" output and play to the speakers in
    /// config.toml. Usually run by systemd (`airpipe.service`); only one
    /// runs at a time.
    Run {
        /// Buffering preset (overrides config.toml).
        #[arg(long, value_enum)]
        latency: Option<LatencyProfile>,
        /// Exact latency in milliseconds (overrides --latency and config.toml).
        #[arg(long, value_parser = parse_latency_ms)]
        latency_ms: Option<u32>,
        /// Release the speakers after this many seconds without audio.
        #[arg(long)]
        idle_timeout: Option<u64>,
        /// PipeWire node name of the output.
        #[arg(long)]
        node_name: Option<String>,
        /// Do not send now-playing metadata or accept speaker remote control.
        #[arg(long)]
        no_metadata: bool,
        /// Never play louder than this percentage of a speaker's full power,
        /// on top of each speaker's max_volume (devices.toml).
        #[arg(long, value_parser = parse_percent)]
        max_volume: Option<f32>,
    },
    /// Play a quiet test tone on a speaker (diagnostics; no PipeWire involved).
    Tone {
        speaker: String,
        #[arg(long, default_value_t = 5)]
        seconds: u64,
        /// Volume in percent of the speaker's full power. Capped by the
        /// speaker's max_volume (devices.toml).
        #[arg(long, default_value = "5", value_parser = parse_percent)]
        volume: f32,
    },
    /// Show where settings live and the effective configuration.
    Config,
}

// A single-threaded runtime is plenty: all async work is light network and
// D-Bus I/O. Real-time audio and PipeWire run on their own threads.
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let result = run_command().await;
    if let Err(e) = &result
        && e.is::<control::AlreadyRunning>()
    {
        eprintln!("Error: {e}");
        std::process::exit(control::ALREADY_RUNNING);
    }
    result
}

async fn run_command() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("airpipe=info")),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let dir = config::config_dir()?;
    let config_path = dir.join("config.toml");
    let store = Store::open(dir);

    match cli.command {
        Cmd::List => list(&store).await,
        Cmd::Info { speaker } => info(&speaker).await,
        Cmd::Pair { speaker, password } => pair(&speaker, password, &store).await,
        Cmd::Unpair { speaker } => unpair(&speaker, &store).await,
        Cmd::Config => show_config(&config_path, &store),
        Cmd::Use { speakers } => set_speakers(&config_path, speakers).await,
        Cmd::Add { speaker } => {
            let mut speakers = Config::load(&config_path)?.output.speakers;
            if speakers.iter().any(|s| s.eq_ignore_ascii_case(&speaker)) {
                bail!("{speaker} is already in use");
            }
            speakers.push(speaker);
            set_speakers(&config_path, speakers).await
        }
        Cmd::Remove { speaker } => {
            let mut speakers = Config::load(&config_path)?.output.speakers;
            let before = speakers.len();
            speakers.retain(|s| !s.eq_ignore_ascii_case(&speaker));
            if speakers.len() == before {
                bail!("{speaker} is not in use (see `airpipe status`)");
            }
            set_speakers(&config_path, speakers).await
        }
        Cmd::Status => request(Request::Status).await,
        Cmd::Gain { speaker, db } => request(Request::Gain { speaker: checked_name(speaker)?, db }).await,
        Cmd::Reload => request(Request::Reload).await,
        Cmd::Stop => request(Request::Stop).await,
        Cmd::Run { latency, latency_ms, idle_timeout, node_name, no_metadata, max_volume } => {
            // First, so a second airpipe stops here before touching anything.
            let control = control::Server::bind()?;
            let mut cfg = Config::load_or_create(&config_path)?;
            // Command-line flags override config.toml.
            if let Some(p) = latency {
                cfg.audio.latency = p;
                cfg.audio.latency_ms = None;
            }
            if latency_ms.is_some() {
                cfg.audio.latency_ms = latency_ms;
            }
            if let Some(t) = idle_timeout {
                cfg.audio.idle_timeout_secs = t;
            }
            if let Some(n) = node_name {
                cfg.output.node_name = n;
            }
            if no_metadata {
                cfg.metadata.enabled = false;
            }
            let group_cfg = GroupConfig { config: cfg, config_path, volume_cap: max_volume };
            group::run(group_cfg, store, control).await
        }
        Cmd::Tone { speaker, seconds, volume } => {
            let cfg = Config::load(&config_path)?;
            tone(&speaker, seconds, volume, &cfg, store).await
        }
    }
}

fn parse_latency_ms(s: &str) -> Result<u32, String> {
    let range = config::LATENCY_MS_RANGE;
    match s.parse::<u32>() {
        Ok(v) if range.contains(&v) => Ok(v),
        _ => Err(format!("must be a number of milliseconds from {} to {}", range.start(), range.end())),
    }
}

/// A percentage of a speaker's range, as a slider position (0.0..=1.0).
fn parse_percent(s: &str) -> Result<f32, String> {
    match s.trim_end_matches('%').parse::<f32>() {
        Ok(v) if (0.0..=100.0).contains(&v) => Ok(v / 100.0),
        _ => Err("must be a percentage from 0 to 100".into()),
    }
}

/// Speaker names travel in the control protocol's tab-separated lines.
fn checked_name(name: String) -> Result<String> {
    if name.trim().is_empty() || name.chars().any(char::is_control) {
        bail!("{name:?} is not a valid speaker name (see `airpipe list`)");
    }
    Ok(name)
}

/// Sends a request to the running airpipe and prints its answer.
async fn request(request: Request) -> Result<()> {
    print_reply(control::send(&request).await?)
}

fn print_reply(reply: Reply) -> Result<()> {
    if !reply.ok {
        bail!("{}", reply.text);
    }
    if !reply.text.is_empty() {
        println!("{}", reply.text);
    }
    Ok(())
}

/// Saves the speaker list, then has the running airpipe switch to it.
async fn set_speakers(config_path: &Path, speakers: Vec<String>) -> Result<()> {
    let speakers = speakers.into_iter().map(checked_name).collect::<Result<Vec<_>>>()?;
    config::save_speakers(config_path, &speakers)?;
    let list = if speakers.is_empty() { "none".to_string() } else { speakers.join(", ") };
    println!("Speakers: {list} (saved in {})", config_path.display());
    match control::send(&Request::Reload).await {
        Ok(reply) => print_reply(reply),
        Err(e) if e.is::<control::NotRunning>() => {
            println!("airpipe is not running; it plays to them once started. {}", control::HOW_TO_START);
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// Read-only: shows settings without creating any file.
fn show_config(path: &Path, store: &Store) -> Result<()> {
    let cfg = Config::load(path)?;
    let exists = |p: &Path| if p.exists() { "" } else { " (not created yet)" };
    println!("Global settings: {}{}", path.display(), exists(path));
    println!("Device memory:   {}{}", store.path().display(), exists(&store.path()));
    println!();
    let speakers = if cfg.output.speakers.is_empty() {
        "(none: choose with `airpipe use <speaker>`)".to_string()
    } else {
        cfg.output.speakers.join(", ")
    };
    let (latency_ms, _) = cfg.latency();
    let latency_source = match cfg.audio.latency_ms {
        Some(_) => "latency_ms".to_string(),
        None => format!("{:?} profile", cfg.audio.latency),
    };
    println!("speakers         {speakers}");
    println!("latency          {latency_ms} ms ({latency_source})");
    println!("idle timeout     {} s", cfg.audio.idle_timeout_secs);
    println!("follow system    {}", cfg.volume.follow_system);
    println!("start volume     {:?}", cfg.volume.start);
    println!("output name      {} (shown on speakers as {:?})", cfg.output.node_name, cfg.network.client_name);
    println!("ports            timing {}/udp, control {}/udp", cfg.network.timing_port, cfg.network.control_port);
    println!("metadata         {}", cfg.metadata.enabled);
    Ok(())
}

async fn list(store: &Store) -> Result<()> {
    let scan = discovery::scan(Duration::from_secs(3)).await?;
    for name in &scan.unresponsive {
        println!("{name:<24} (advertised, but did not answer; it may be waking up, try again)");
    }
    let devices = scan.devices;
    if devices.is_empty() && scan.unresponsive.is_empty() {
        println!("No AirPlay speakers found. Check that UDP 5353 (mDNS) is allowed from your LAN (see README).");
        return Ok(());
    }
    println!("{:<24} {:<22} {:<28} NOTES", "NAME", "ADDRESS", "MODEL");
    for d in devices {
        let mut notes = Vec::new();
        notes.push(if d.is_airplay2() { "AirPlay 2" } else { "AirPlay 1 only (unsupported)" });
        if d.needs_pairing() {
            notes.push(if store.device_keys(&d.device_id)?.is_some() { "paired" } else { "needs `airpipe pair`" });
        }
        if d.needs_password() {
            notes.push("password protected");
        }
        let model = format!("{} {}", d.manufacturer, d.model);
        println!("{:<24} {:<22} {:<28} {}", d.name, d.address, model.trim(), notes.join(", "));
    }
    Ok(())
}

async fn find(name: &str) -> Result<Device> {
    discovery::find(name, Duration::from_secs(5)).await
}

async fn info(name: &str) -> Result<()> {
    let device = find(name).await?;
    let mut conn = Connection::connect(device.address, vec![("User-Agent".into(), USER_AGENT.into())]).await?;
    let resp = conn.request_ok(rtsp::Request::new("GET", "/info")).await?;
    let value = airpipe::proto::plist::parse(&resp.body).context("parsing /info")?;
    println!("{} ({})", device.name, device.address);
    println!("  model:    {} {} (firmware {})", device.manufacturer, device.model, device.firmware);
    println!("  features: {:#x}", device.features);
    let named = [
        (feature::METADATA_ARTWORK, "artwork"),
        (feature::METADATA_PROGRESS, "progress"),
        (feature::METADATA_TEXT, "text metadata"),
        (feature::BUFFERED_AUDIO, "buffered audio"),
        (feature::PTP_CLOCK, "PTP clock"),
        (feature::HOMEKIT_PAIRING, "HomeKit pairing"),
        (feature::COREUTILS_PAIRING, "CoreUtils pairing"),
    ];
    let present: Vec<_> = named.iter().filter(|(b, _)| device.has_feature(*b)).map(|(_, n)| *n).collect();
    println!("  supports: {}", present.join(", "));
    let mut out = Vec::new();
    plist::to_writer_xml(&mut out, &value)?;
    // Speaker-supplied text: strip terminal control sequences before printing.
    println!("{}", airpipe::proto::printable(&String::from_utf8_lossy(&out)));
    Ok(())
}

async fn pair(name: &str, password: Option<String>, store: &Store) -> Result<()> {
    let device = find(name).await?;
    let identity = store.identity()?;
    let ask = || -> Result<String> {
        print!("Enter the PIN shown by {}: ", device.name);
        std::io::stdout().flush()?;
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        Ok(line.trim().to_string())
    };
    let keys = airpipe::airplay::pair::with_pin(&device, &identity, ask, password).await?;
    store.save_device_keys(&device.device_id, &device.name, &keys)?;
    println!("Paired with {}. Keys saved in {}.", device.name, store.path().display());
    Ok(())
}

async fn unpair(name: &str, store: &Store) -> Result<()> {
    let device = find(name).await?;
    if store.remove_device_keys(&device.device_id)? {
        println!("Removed pairing for {}.", device.name);
    } else {
        println!("{} was not paired.", device.name);
    }
    Ok(())
}

async fn tone(name: &str, seconds: u64, volume: f32, cfg: &Config, store: Store) -> Result<()> {
    let device = find(name).await?;
    let settings = store.device_settings(&device)?;
    // Same rules as streaming: never above the speaker's max_volume.
    let inputs = VolumeInputs {
        settings: DeviceSettings { default_volume: volume, ..settings },
        gain_db: 0.0,
        cap: None,
        follow_system: false,
    };
    let volume = inputs.airplay_db(None);

    // Same pipeline as `airpipe run`, fed by a tone generator instead of PipeWire.
    let (mut producer, consumer) = rtrb::RingBuffer::<i16>::new(SAMPLE_RATE as usize * 2);
    let pipeline = Pipeline::start(cfg, &store, consumer)?;
    let (latency_ms, jitter_frames) = cfg.latency();

    // Soft 440 Hz sine (about -12 dBFS), generated in real time and kept two
    // jitter cushions ahead so the engine's buffer never runs dry. Runs until
    // the session is over.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_generator = stop.clone();
    let generator = std::thread::spawn(move || {
        let rate = f64::from(SAMPLE_RATE);
        let start = std::time::Instant::now();
        let mut n: u64 = 0;
        while !stop_generator.load(std::sync::atomic::Ordering::Relaxed) {
            let due = (start.elapsed().as_secs_f64() * rate) as u64 + 2 * jitter_frames as u64;
            while n < due {
                let v = ((n as f64 * 440.0 * std::f64::consts::TAU / rate).sin() * 8000.0) as i16;
                if producer.push(v).is_err() || producer.push(v).is_err() {
                    break;
                }
                n += 1;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    });

    let (events, mut events_rx) = mpsc::channel(16);
    let keys = store.keys_for(&device)?;
    let (session, _permit) = pipeline.connect(device, keys, volume, events).await?;
    let ip = session.device.address.ip();
    let target = pipeline.engine.add_target(ip, session.ports.data, session.ports.control, 0)?;
    // The first sound comes out after the configured latency.
    let latency = Duration::from_millis(latency_ms.into());
    println!(
        "Playing a 440 Hz tone at {:.0}% ({volume:.1} dB) for {seconds}s (after {latency_ms} ms of buffering)...",
        volume::db_to_level(volume) * 100.0
    );
    let end = tokio::time::sleep(latency + Duration::from_secs(seconds));
    tokio::pin!(end);
    loop {
        tokio::select! {
            _ = &mut end => break,
            ev = events_rx.recv() => match ev {
                // Speaker buttons do not stop the tone; only a failure or takeover does.
                Some(SessionEvent::Remote { .. }) => {}
                other => {
                    println!("session ended early: {other:?}");
                    break;
                }
            },
        }
    }
    pipeline.engine.remove_target(target);
    session.teardown().await;
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = generator.join();
    Ok(())
}
