use std::io::{IsTerminal, Write};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc;

use airpipe::airplay::connection::{Connection, USER_AGENT};
use airpipe::airplay::session::SessionEvent;
use airpipe::config::{self, Config, LatencyProfile};
use airpipe::discovery::{self, Device, feature};
use airpipe::group::volume::{self, VolumeInputs};
use airpipe::group::{self, ConsoleCommand, GroupConfig, Pipeline};
use airpipe::proto::rtp::SAMPLE_RATE;
use airpipe::proto::rtsp::Request;
use airpipe::store::{DeviceSettings, Store};

#[derive(Parser)]
#[command(
    version,
    about = "Stream Linux system audio from PipeWire to AirPlay 2 speakers (sender only)",
    after_help = "Settings: ~/.config/airpipe/config.toml (global) and devices.toml (per speaker). \
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
    /// Create a PipeWire output that plays on the given speakers, in sync.
    /// Only one `airpipe run` can be active at a time.
    Run {
        /// Speaker names as shown by `airpipe list`. Defaults to
        /// `output.speakers` in config.toml.
        speakers: Vec<String>,
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
        /// Extra volume ceiling in AirPlay dB for this run, on top of each
        /// speaker's max_volume_db (-30 quietest .. 0 = speaker maximum).
        #[arg(long, allow_hyphen_values = true, value_parser = parse_volume)]
        max_volume: Option<f32>,
    },
    /// Play a quiet test tone on a speaker (diagnostics; no PipeWire involved).
    Tone {
        speaker: String,
        #[arg(long, default_value_t = 5)]
        seconds: u64,
        /// AirPlay volume in dB, -30 (quietest) to 0 (speaker maximum).
        /// Capped by the speaker's max_volume_db.
        #[arg(long, default_value_t = -28.0, allow_hyphen_values = true, value_parser = parse_volume)]
        volume: f32,
    },
    /// Show where settings live and the effective configuration.
    Config,
}

// A single-threaded runtime is plenty: all async work is light network and
// D-Bus I/O. Real-time audio and PipeWire run on their own threads.
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
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
        Cmd::Run { speakers, latency, latency_ms, idle_timeout, node_name, no_metadata, max_volume } => {
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
            let speakers = if speakers.is_empty() { cfg.output.speakers.clone() } else { speakers };
            if speakers.is_empty() {
                bail!(
                    "no speakers given: pass names (see `airpipe list`) or set output.speakers in {}",
                    config_path.display()
                );
            }
            let group_cfg = GroupConfig { config: cfg, speakers, volume_cap_db: max_volume };
            let (tx, rx) = mpsc::unbounded_channel();
            if std::io::stdin().is_terminal() {
                tokio::spawn(console(tx));
            }
            group::run(group_cfg, store, rx).await
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

fn parse_volume(s: &str) -> Result<f32, String> {
    let v: f32 = s.parse().map_err(|_| format!("{s:?} is not a number"))?;
    if (volume::MIN_DB..=0.0).contains(&v) {
        Ok(v)
    } else {
        Err("must be between -30 (quietest) and 0 (speaker maximum) dB".into())
    }
}

/// Read-only: shows settings without creating any file.
fn show_config(path: &std::path::Path, store: &Store) -> Result<()> {
    let cfg = Config::load(path)?;
    let exists = |p: &std::path::Path| if p.exists() { "" } else { " (not created yet)" };
    println!("Global settings: {}{}", path.display(), exists(path));
    println!("Device memory:   {}{}", store.path().display(), exists(&store.path()));
    println!();
    let speakers = if cfg.output.speakers.is_empty() {
        "(none: pass names to `airpipe run`)".to_string()
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
    let resp = conn.request_ok(Request::new("GET", "/info")).await?;
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

const CONSOLE_HELP: &str = "commands: status | gain <speaker> <dB> | reload | add <speaker> | remove <speaker> | quit";

/// Reads console commands from stdin while `run` is active.
async fn console(tx: mpsc::UnboundedSender<ConsoleCommand>) {
    println!("Console: {CONSOLE_HELP}");
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let words: Vec<&str> = line.split_whitespace().collect();
        let cmd = match words.as_slice() {
            [] => continue,
            ["status" | "s"] => ConsoleCommand::Status,
            ["reload" | "r"] => ConsoleCommand::Reload,
            ["quit" | "q" | "exit"] => ConsoleCommand::Quit,
            ["gain", rest @ ..] if rest.len() >= 2 => match rest[rest.len() - 1].parse::<f32>() {
                Ok(db) => ConsoleCommand::Gain { speaker: rest[..rest.len() - 1].join(" "), db },
                Err(_) => {
                    println!("usage: gain <speaker> <dB, e.g. -6>");
                    continue;
                }
            },
            ["add", rest @ ..] if !rest.is_empty() => ConsoleCommand::Add(rest.join(" ")),
            ["remove" | "rm", rest @ ..] if !rest.is_empty() => ConsoleCommand::Remove(rest.join(" ")),
            _ => {
                println!("{CONSOLE_HELP}");
                continue;
            }
        };
        let quit = matches!(cmd, ConsoleCommand::Quit);
        if tx.send(cmd).is_err() || quit {
            return;
        }
    }
    // stdin closed (Ctrl-D): treat as quit.
    let _ = tx.send(ConsoleCommand::Quit);
}

async fn tone(name: &str, seconds: u64, volume: f32, cfg: &Config, store: Store) -> Result<()> {
    let device = find(name).await?;
    let settings = store.device_settings(&device)?;
    // Same rules as streaming: never above the speaker's max_volume_db.
    let volume = VolumeInputs {
        settings: DeviceSettings { default_volume_db: volume, ..settings },
        gain_db: 0.0,
        cap_db: None,
        follow_system: false,
    }
    .airplay_db(None);

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
    println!("Playing a 440 Hz tone at {volume:.1} dB for {seconds}s (after {latency_ms} ms of buffering)...");
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
