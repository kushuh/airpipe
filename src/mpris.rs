//! Now-playing metadata and remote control through MPRIS (session D-Bus).
//!
//! PipeWire cannot tell us *what* is playing, but desktop media players
//! publish it over MPRIS. We follow the player that is currently playing
//! (there is no way to know which output a player uses, so the most recently
//! playing one wins), forward its metadata to the speakers, and route
//! speaker buttons (play/pause/next/previous) back to it.
//!
//! Polling once a second, and only while a speaker is streaming, is
//! deliberate: it costs a few D-Bus calls, is robust against players that
//! emit incomplete `PropertiesChanged` signals, and needs no state machine
//! for players appearing and disappearing. Proxies are created uncached, so
//! each poll is exactly one `Get` per property (no subscriptions).

use std::collections::HashMap;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::{mpsc, watch};
use tracing::{debug, info};
use zbus::zvariant::OwnedValue;

use crate::airplay::events::RemoteCommand;
use crate::proto::dmap::TrackInfo;

const POLL: Duration = Duration::from_secs(1);
/// Upper bound for one poll (all D-Bus calls plus reading artwork).
const POLL_TIMEOUT: Duration = Duration::from_secs(3);
/// Larger covers are skipped: the upload shares the speaker connection with
/// volume changes, so it must stay quick even on weak Wi-Fi.
const MAX_ARTWORK_BYTES: u64 = 1024 * 1024;
const IFACE_PLAYER: &str = "org.mpris.MediaPlayer2.Player";
const PATH: &str = "/org/mpris/MediaPlayer2";

#[derive(Debug, Clone, PartialEq)]
pub struct Artwork {
    pub mime: &'static str,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MediaEvent {
    /// A new track (or new metadata for the current one).
    Track { info: TrackInfo, artwork: Option<Artwork>, length: Option<Duration> },
    /// Playback position of the current track.
    Position(Duration),
    /// Nothing identifiable is playing any more.
    Cleared,
}

#[derive(Debug, Default, Clone, PartialEq)]
struct Snapshot {
    info: TrackInfo,
    art_url: Option<String>,
    length: Option<Duration>,
}

/// A player proxy without property caching: one D-Bus call per read.
async fn player(conn: &zbus::Connection, name: &str) -> zbus::Result<zbus::Proxy<'static>> {
    zbus::proxy::Builder::<zbus::Proxy>::new(conn)
        .destination(name.to_owned())?
        .path(PATH)?
        .interface(IFACE_PLAYER)?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
}

/// Runs until the session bus goes away. Errors (no session bus) are logged
/// by the caller; airpipe works without metadata.
///
/// `streaming` tells whether any speaker is connected; nothing is polled
/// while it is false.
pub async fn run(
    events: mpsc::UnboundedSender<MediaEvent>,
    mut commands: mpsc::Receiver<RemoteCommand>,
    mut streaming: watch::Receiver<bool>,
) -> Result<()> {
    let conn = zbus::Connection::session().await?;
    let dbus = zbus::fdo::DBusProxy::new(&conn).await?;
    let mut current_player: Option<String> = None;
    let mut last: Option<Snapshot> = None;
    let mut ticker = tokio::time::interval(POLL);

    loop {
        tokio::select! {
            changed = streaming.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                // Nothing to reset: the group sends the current track to
                // every new connection itself.
                continue;
            }
            _ = ticker.tick() => {
                if !*streaming.borrow() {
                    continue;
                }
            }
            cmd = commands.recv() => {
                let Some(cmd) = cmd else { return Ok(()) };
                if let Some(name) = &current_player {
                    let method = match cmd {
                        // Speakers send "play" and "paus" as a toggle in practice.
                        RemoteCommand::Play | RemoteCommand::Pause => "PlayPause",
                        RemoteCommand::Next => "Next",
                        RemoteCommand::Previous => "Previous",
                    };
                    info!("remote control: {method} -> {name}");
                    if let Ok(proxy) = player(&conn, name).await
                        && let Err(e) = proxy.call_method(method, &()).await {
                            debug!("{name}: {method} failed: {e}");
                        }
                }
                continue;
            }
        }

        // A player that hangs must not stall metadata or remote control.
        match tokio::time::timeout(POLL_TIMEOUT, poll(&conn, &dbus, &mut current_player, &mut last, &events)).await {
            Ok(result) => result?,
            Err(_) => debug!("MPRIS poll timed out"),
        }
    }
}

/// One poll: follows the playing player and reports changes.
async fn poll(
    conn: &zbus::Connection,
    dbus: &zbus::fdo::DBusProxy<'_>,
    current_player: &mut Option<String>,
    last: &mut Option<Snapshot>,
    events: &mpsc::UnboundedSender<MediaEvent>,
) -> Result<()> {
    let names = dbus.list_names().await?;
    let mut playing = None;
    for name in names.iter().map(|n| n.as_str()).filter(|n| n.starts_with("org.mpris.MediaPlayer2.")) {
        let Ok(proxy) = player(conn, name).await else { continue };
        if proxy.get_property::<String>("PlaybackStatus").await.ok().as_deref() == Some("Playing") {
            playing = Some((name.to_string(), proxy));
            break;
        }
    }
    let Some((name, proxy)) = playing else {
        // Audio is streaming but no player claims it (a game, system sounds,
        // a paused player): do not leave a stale title on the speakers.
        if last.take().is_some() {
            let _ = events.send(MediaEvent::Cleared);
        }
        return Ok(());
    };
    if current_player.as_deref() != Some(name.as_str()) {
        debug!("following MPRIS player {name}");
        *current_player = Some(name);
    }

    let Ok(metadata) = proxy.get_property::<HashMap<String, OwnedValue>>("Metadata").await else { return Ok(()) };
    let snapshot = snapshot(&metadata);
    if last.as_ref() != Some(&snapshot) {
        let artwork = match &snapshot.art_url {
            Some(url) => load_artwork(url).await,
            None => None,
        };
        let _ = events.send(MediaEvent::Track { info: snapshot.info.clone(), artwork, length: snapshot.length });
        *last = Some(snapshot);
    }
    if let Ok(pos) = proxy.get_property::<i64>("Position").await {
        let _ = events.send(MediaEvent::Position(Duration::from_micros(pos.max(0) as u64)));
    }
    Ok(())
}

fn string(v: &OwnedValue) -> Option<String> {
    String::try_from(v.try_clone().ok()?).ok().filter(|s| !s.is_empty())
}

fn snapshot(md: &HashMap<String, OwnedValue>) -> Snapshot {
    let artist = md
        .get("xesam:artist")
        .and_then(|v| Vec::<String>::try_from(v.try_clone().ok()?).ok())
        .map(|a| a.join(", "))
        .filter(|s| !s.is_empty());
    let genre = md
        .get("xesam:genre")
        .and_then(|v| Vec::<String>::try_from(v.try_clone().ok()?).ok())
        .and_then(|g| g.into_iter().next());
    // mpris:length is specified as x (i64) but some players send t (u64).
    let length = md.get("mpris:length").and_then(|v| {
        let v = v.try_clone().ok()?;
        i64::try_from(v.try_clone().ok()?)
            .ok()
            .map(|x| x.max(0) as u64)
            .or_else(|| u64::try_from(v).ok())
            .filter(|&us| us > 0)
            .map(Duration::from_micros)
    });
    Snapshot {
        info: TrackInfo {
            title: md.get("xesam:title").and_then(string),
            artist,
            album: md.get("xesam:album").and_then(string),
            genre,
        },
        art_url: md.get("mpris:artUrl").and_then(string),
        length,
    }
}

/// Loads artwork from a `file://` URL. Remote URLs are not fetched: airpipe
/// makes no outbound connections except to speakers.
///
/// The path comes from another program, so: only regular files (no FIFOs or
/// devices like `/dev/zero`, which would block or never end), and never more
/// than `MAX_ARTWORK_BYTES` read, even if the file grows meanwhile.
async fn load_artwork(url: &str) -> Option<Artwork> {
    use tokio::io::AsyncReadExt;
    let path = percent_decode(url.strip_prefix("file://")?);
    if !tokio::fs::metadata(&path).await.ok()?.is_file() {
        return None;
    }
    // O_NONBLOCK: even if the path is swapped for a FIFO after the check
    // above, opening it cannot block.
    let file = tokio::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(&path).await.ok()?;
    let meta = file.metadata().await.ok()?;
    if !meta.is_file() || meta.len() > MAX_ARTWORK_BYTES {
        debug!("artwork {path} skipped (not a regular file or larger than {MAX_ARTWORK_BYTES} bytes)");
        return None;
    }
    let mut data = Vec::with_capacity(meta.len() as usize);
    file.take(MAX_ARTWORK_BYTES).read_to_end(&mut data).await.ok()?;
    let mime = sniff_image(&data)?;
    Some(Artwork { mime, data })
}

fn sniff_image(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else {
        None
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let (Some(&h), Some(&l)) = (bytes.get(i + 1), bytes.get(i + 2))
            && let (Some(h), Some(l)) = (hex(h), hex(l))
        {
            out.push(h << 4 | l);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::Value;

    fn ov(v: Value<'static>) -> OwnedValue {
        OwnedValue::try_from(v).unwrap()
    }

    #[test]
    fn metadata_snapshot() {
        let mut md = HashMap::new();
        md.insert("xesam:title".to_string(), ov(Value::from("Song")));
        md.insert("xesam:artist".to_string(), ov(Value::from(vec!["A".to_string(), "B".to_string()])));
        md.insert("xesam:album".to_string(), ov(Value::from("")));
        md.insert("mpris:length".to_string(), ov(Value::from(180_000_000i64)));
        md.insert("mpris:artUrl".to_string(), ov(Value::from("file:///tmp/a%20b.jpg")));
        let s = snapshot(&md);
        assert_eq!(s.info.title.as_deref(), Some("Song"));
        assert_eq!(s.info.artist.as_deref(), Some("A, B"));
        assert_eq!(s.info.album, None, "empty strings are dropped");
        assert_eq!(s.length, Some(Duration::from_secs(180)));
        assert_eq!(s.art_url.as_deref(), Some("file:///tmp/a%20b.jpg"));
    }

    #[test]
    fn unsigned_length_is_accepted() {
        let mut md = HashMap::new();
        md.insert("mpris:length".to_string(), ov(Value::from(5_000_000u64)));
        assert_eq!(snapshot(&md).length, Some(Duration::from_secs(5)));
    }

    #[tokio::test]
    async fn artwork_rejects_devices_and_non_images() {
        assert!(load_artwork("file:///dev/zero").await.is_none(), "devices are not regular files");
        assert!(load_artwork("file:///etc/hostname").await.is_none(), "not an image");
        assert!(load_artwork("https://example.com/a.jpg").await.is_none(), "remote URLs are never fetched");
    }

    #[test]
    fn image_sniffing_and_decoding() {
        assert_eq!(sniff_image(&[0xff, 0xd8, 0xff, 0xe0]), Some("image/jpeg"));
        assert_eq!(sniff_image(b"\x89PNG\r\n\x1a\nxxxx"), Some("image/png"));
        assert_eq!(sniff_image(b"GIF89a"), None);
        assert_eq!(percent_decode("/tmp/a%20b%C3%A9.jpg"), "/tmp/a bé.jpg");
        // Malformed escapes are kept literally, never panic.
        assert_eq!(percent_decode("/tmp/%aé.jpg"), "/tmp/%aé.jpg");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }
}
