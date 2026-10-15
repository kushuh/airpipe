//! Speaker discovery: browses `_airplay._tcp` through the Avahi daemon over
//! the system D-Bus.
//!
//! Avahi is the standard mDNS service on Linux desktops (PipeWire's own
//! AirPlay module depends on it too). Its warm cache makes discovery instant
//! and reliable, unlike a fresh mDNS client that must wait for answers.
//!
//! airpipe only *browses*. It never registers a service, so it never shows up
//! as an AirPlay receiver on the network.

use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use tokio::time::Instant;

use crate::proto::printable_line;
use tracing::{debug, warn};

const SERVICE: &str = "_airplay._tcp";
const AVAHI: &str = "org.freedesktop.Avahi";
/// IPv4 only: some receivers misbehave on IPv6 (OwnTone notes the same).
const PROTO_INET: i32 = 0;

/// Feature bits advertised in the `features` TXT key.
/// Sources: <https://emanuelecozzi.net/docs/airplay2/features/> and OwnTone's
/// `features_map` (outputs/airplay.c).
pub mod feature {
    pub const METADATA_ARTWORK: u32 = 15;
    pub const METADATA_PROGRESS: u32 = 16;
    pub const METADATA_TEXT: u32 = 17;
    pub const UNIFIED_MEDIA_CONTROL: u32 = 38;
    pub const BUFFERED_AUDIO: u32 = 40;
    pub const PTP_CLOCK: u32 = 41;
    pub const SYSTEM_PAIRING: u32 = 43;
    pub const HOMEKIT_PAIRING: u32 = 46;
    pub const COREUTILS_PAIRING: u32 = 48;
}

/// Status flag bits (`flags` TXT key and `/info` statusFlags).
pub mod status {
    pub const PIN_REQUIRED: u64 = 1 << 3;
    pub const PASSWORD_REQUIRED: u64 = 1 << 7;
    pub const ONE_TIME_PAIRING_REQUIRED: u64 = 1 << 9;
    pub const RECEIVER_BUSY: u64 = 1 << 17;
}

/// An AirPlay receiver found on the network.
#[derive(Debug, Clone)]
pub struct Device {
    pub name: String,
    pub address: SocketAddr,
    /// `deviceid` TXT key (MAC-style); our stable key for a speaker.
    pub device_id: String,
    pub model: String,
    pub manufacturer: String,
    pub firmware: String,
    pub features: u64,
    pub flags: u64,
}

impl Device {
    pub fn has_feature(&self, bit: u32) -> bool {
        self.features & (1u64 << bit) != 0
    }

    pub fn is_airplay2(&self) -> bool {
        // Any of these implies HomeKit-style pairing and encryption.
        [feature::UNIFIED_MEDIA_CONTROL, feature::SYSTEM_PAIRING, feature::HOMEKIT_PAIRING, feature::COREUTILS_PAIRING]
            .iter()
            .any(|&bit| self.has_feature(bit))
    }

    pub fn needs_pairing(&self) -> bool {
        self.flags & (status::PIN_REQUIRED | status::ONE_TIME_PAIRING_REQUIRED) != 0
    }

    pub fn needs_password(&self) -> bool {
        self.flags & status::PASSWORD_REQUIRED != 0
    }

    /// Builds a device from advertised data. Text fields come from the
    /// network, so control characters are replaced: they are printed to the
    /// terminal and written to devices.toml.
    fn from_txt(name: String, address: SocketAddr, txt: &HashMap<String, String>) -> Self {
        let get = |k: &str| printable_line(txt.get(k).map(String::as_str).unwrap_or_default());
        Self {
            name: printable_line(&name),
            address,
            device_id: get("deviceid"),
            model: get("model"),
            manufacturer: get("manufacturer"),
            firmware: get("fv"),
            features: parse_features(&get("features")),
            flags: parse_hex(&get("flags")),
        }
    }
}

fn parse_hex(s: &str) -> u64 {
    u64::from_str_radix(s.trim_start_matches("0x").trim_start_matches("0X"), 16).unwrap_or(0)
}

/// Parses `features=0xLOW,0xHIGH` into one 64-bit value.
pub fn parse_features(s: &str) -> u64 {
    let mut parts = s.split(',');
    let low = parts.next().map(parse_hex).unwrap_or(0);
    let high = parts.next().map(parse_hex).unwrap_or(0);
    (high << 32) | (low & 0xffff_ffff)
}

/// Result of a scan: resolved speakers, plus speakers that are advertised
/// but did not answer the address lookup (often: waking up from standby).
pub struct Scan {
    pub devices: Vec<Device>,
    pub unresponsive: Vec<String>,
}

/// Lists every speaker Avahi knows about, sorted by name.
///
/// Browsing returns as soon as Avahi has reported everything it has cached
/// (or after `timeout`); the speakers are then resolved in parallel.
pub async fn scan(timeout: Duration) -> Result<Scan> {
    browse(timeout).await.context(
        "could not query Avahi for speakers; is avahi-daemon running? \
         (sudo systemctl enable --now avahi-daemon)",
    )
}

/// Finds a speaker by name (case-insensitive).
pub async fn find(name: &str, timeout: Duration) -> Result<Device> {
    let scan = scan(timeout).await?;
    if let Some(d) = scan.devices.into_iter().find(|d| d.name.eq_ignore_ascii_case(name)) {
        return Ok(d);
    }
    if scan.unresponsive.iter().any(|n| n.eq_ignore_ascii_case(name)) {
        bail!("{name:?} is on the network but did not answer (it may be waking up)");
    }
    bail!("no AirPlay speaker named {name:?} found (see `airpipe list`)")
}

/// What `ItemNew` tells us about an advertised service.
struct Advertised {
    iface: i32,
    proto: i32,
    name: String,
    ty: String,
    domain: String,
}

async fn browse(timeout: Duration) -> Result<Scan> {
    const IF_UNSPEC: i32 = -1;

    let conn = zbus::Connection::system().await?;
    let server = zbus::Proxy::new(&conn, AVAHI, "/", "org.freedesktop.Avahi.Server").await?;
    // Prepare + Start (Server2, Avahi >= 0.8) avoids missing signals emitted
    // before we subscribe.
    let server2 = zbus::Proxy::new(&conn, AVAHI, "/", "org.freedesktop.Avahi.Server2").await?;
    let path: zbus::zvariant::OwnedObjectPath =
        server2.call("ServiceBrowserPrepare", &(IF_UNSPEC, PROTO_INET, SERVICE, "", 0u32)).await?;
    let browser = zbus::Proxy::new(&conn, AVAHI, path, "org.freedesktop.Avahi.ServiceBrowser").await?;
    let mut items = browser.receive_signal("ItemNew").await?;
    let mut all_for_now = browser.receive_signal("AllForNow").await?;
    browser.call_method("Start", &()).await?;

    // Phase 1: collect advertised services (instant: Avahi answers from cache).
    let deadline = Instant::now() + timeout;
    let mut advertised = Vec::new();
    loop {
        tokio::select! {
            Some(msg) = items.next() => {
                let (iface, proto, name, ty, domain, _flags): (i32, i32, String, String, String, u32) =
                    msg.body().deserialize()?;
                advertised.push(Advertised { iface, proto, name, ty, domain });
            }
            Some(_) = all_for_now.next() => break,
            _ = tokio::time::sleep_until(deadline) => break,
        }
    }
    let _ = browser.call_method("Free", &()).await;

    // Phase 2: resolve addresses in parallel, so one slow speaker does not
    // hold up the others.
    let results = futures_util::future::join_all(advertised.iter().map(|a| resolve(&server, a))).await;
    let mut devices = BTreeMap::new();
    let mut unresponsive = Vec::new();
    for (a, result) in advertised.iter().zip(results) {
        match result {
            Some(d) => {
                if let Some(old) = devices.insert(d.name.clone(), d) {
                    // Speakers are addressed by name, so only one can be used.
                    warn!("two speakers are named {:?} ({} and another); rename one", old.name, old.address);
                }
            }
            None => unresponsive.push(printable_line(&a.name)),
        }
    }
    Ok(Scan { devices: devices.into_values().collect(), unresponsive })
}

/// Resolves one service to an address and TXT record. Avahi gives up after
/// about 5 s; one retry covers speakers that are slow to wake up.
async fn resolve(server: &zbus::Proxy<'_>, a: &Advertised) -> Option<Device> {
    // ResolveService returns
    // (iface, proto, name, type, domain, host, aproto, address, port, txt, flags).
    type Resolved = (i32, i32, String, String, String, String, i32, String, u16, Vec<Vec<u8>>, u32);
    for attempt in 1..=2 {
        let args = (a.iface, a.proto, a.name.as_str(), a.ty.as_str(), a.domain.as_str(), PROTO_INET, 0u32);
        match server.call::<_, _, Resolved>("ResolveService", &args).await {
            Ok((_, _, name, _, _, _, _, address, port, txt, _)) => {
                let ip = address.parse::<IpAddr>().ok()?;
                let device = Device::from_txt(name, SocketAddr::new(ip, port), &parse_txt(&txt));
                // The device ID is how pairings and settings are keyed.
                return (!device.device_id.is_empty()).then_some(device);
            }
            Err(e) => debug!("resolving {:?} (attempt {attempt}): {e}", a.name),
        }
    }
    None
}

/// TXT records arrive as raw `key=value` byte strings.
fn parse_txt(entries: &[Vec<u8>]) -> HashMap<String, String> {
    entries
        .iter()
        .filter_map(|entry| {
            let entry = String::from_utf8_lossy(entry);
            let (k, v) = entry.split_once('=')?;
            Some((k.to_string(), v.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn features_combine_both_words() {
        // Devialet Phantom: features=0x445F8A00,0x1C340
        let f = parse_features("0x445F8A00,0x1C340");
        assert_eq!(f, 496155769145856, "matches the value reported by /info");
        let dev = Device {
            name: "x".into(),
            address: "127.0.0.1:7000".parse().unwrap(),
            device_id: String::new(),
            model: String::new(),
            manufacturer: String::new(),
            firmware: String::new(),
            features: f,
            flags: 0x4,
        };
        assert!(dev.is_airplay2());
        assert!(dev.has_feature(feature::PTP_CLOCK));
        assert!(dev.has_feature(feature::COREUTILS_PAIRING));
        assert!(dev.has_feature(feature::METADATA_TEXT));
        assert!(!dev.needs_pairing());
    }

    #[test]
    fn txt_records_are_parsed() {
        let txt = parse_txt(&[b"deviceid=50:5B".to_vec(), b"features=0x1,0x2".to_vec(), b"novalue".to_vec()]);
        assert_eq!(txt.get("deviceid").map(String::as_str), Some("50:5B"));
        assert_eq!(txt.get("features").map(String::as_str), Some("0x1,0x2"));
        assert!(!txt.contains_key("novalue"));
    }

    #[test]
    fn single_word_features() {
        assert_eq!(parse_features("0x444F8A00"), 0x444F8A00);
        assert_eq!(parse_features(""), 0);
    }
}
