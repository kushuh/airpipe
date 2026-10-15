//! Device memory: `$XDG_CONFIG_HOME/airpipe/devices.toml`.
//!
//! airpipe records every speaker it uses here, with per-speaker settings the
//! user may edit (volume levels, sync offset) and the keys needed to talk to
//! it. The file is edited in place with `toml_edit`, so comments and manual
//! changes survive airpipe's own updates.
//!
//! ```toml
//! [controller]                  # our pairing identity; keep this file private
//! pairing_id = "..."
//! device_id = "02:..."
//! signing_key = "<hex, secret>"
//!
//! [devices."50:5B:C2:9C:79:E5"] # keyed by the speaker's deviceid
//! name = "Bureau"
//! model = "Devialet Phantom 103 dB"
//! max_volume_db = -20.0
//! default_volume_db = -26.0
//! latency_offset_ms = 0
//!
//! [devices."50:5B:C2:9C:79:E5".pairing]   # only after `airpipe pair`
//! accessory_id = "..."
//! public_key = "<hex>"
//! ```
//!
//! The file is created with mode 0600 because it holds our secret key.

use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use ed25519_dalek::SigningKey;
use toml_edit::{DocumentMut, Item, Table, value};

use crate::crypto::pairing::{DeviceKeys, Identity};
use crate::discovery::Device;
use crate::group::volume::MIN_DB;

const FILE_NAME: &str = "devices.toml";

const HEADER: &str = "\
# airpipe device memory. airpipe adds speakers here when it first uses them.
# One entry per AirPlay target as discovered on the network: a stereo pair or a
# vendor multi-room setup that shows up as a single target is a single entry.
# You may edit per-speaker settings; your edits and comments are kept.
# Volumes are AirPlay dB: -30 is the quietest step, 0 is the speaker's FULL power.
#   max_volume_db      sent when the system volume is at 100% (hard ceiling)
#   default_volume_db  level at start-up, and when not following system volume
#   latency_offset_ms  delay this speaker (-250..1000) to line up multi-room groups
# This file contains a secret key: keep it private (mode 0600).
";

/// Per-speaker settings, with defaults chosen by model.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DeviceSettings {
    /// AirPlay dB sent when the system volume is at 100%. Nothing above is sent.
    pub max_volume_db: f32,
    /// Start-up level, and the fixed level when not following system volume.
    pub default_volume_db: f32,
    /// Extra delay for this speaker, to line up a multi-room group.
    pub latency_offset_ms: i32,
}

/// Speakers known to be very loud get a lower ceiling and starting point.
/// Matched case-insensitively as substrings of "manufacturer model " (note
/// the trailing space, so that "kef " matches a whole word, not "kefir").
const HIGH_OUTPUT: &[&str] = &["devialet", "bang & olufsen", "mcintosh", "klipsch", "kef "];

impl DeviceSettings {
    pub fn defaults_for(manufacturer: &str, model: &str) -> Self {
        let id = format!("{manufacturer} {model} ").to_lowercase();
        if HIGH_OUTPUT.iter().any(|m| id.contains(m)) {
            Self::conservative()
        } else {
            Self { max_volume_db: -10.0, default_volume_db: -20.0, latency_offset_ms: 0 }
        }
    }

    /// Used for high-output speakers, and for speakers not resolved yet.
    pub fn conservative() -> Self {
        Self { max_volume_db: -20.0, default_volume_db: -26.0, latency_offset_ms: 0 }
    }

    /// Brings hand-edited values into range. Non-numbers (`nan`, `inf`) fall
    /// back to the model defaults rather than reaching the volume code.
    fn sanitized(mut self, defaults: Self) -> Self {
        let finite = |v: f32, d: f32| if v.is_finite() { v } else { d };
        self.max_volume_db = finite(self.max_volume_db, defaults.max_volume_db).clamp(MIN_DB, 0.0);
        self.default_volume_db =
            finite(self.default_volume_db, defaults.default_volume_db).clamp(MIN_DB, self.max_volume_db);
        let offsets = crate::config::LATENCY_OFFSET_MS_RANGE;
        self.latency_offset_ms = self.latency_offset_ms.clamp(*offsets.start(), *offsets.end());
        self
    }
}

/// Our identity as a pairing controller.
#[derive(Clone)]
pub struct LocalIdentity {
    pub pairing: Identity,
    /// MAC-style identifier we present as `deviceID` in SETUP.
    pub device_id: String,
}

/// Handle to the device memory file. Cheap to clone: it is just a path.
#[derive(Clone)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// Does no I/O: the file is read on demand and created on first write.
    pub fn open(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join(FILE_NAME)
    }

    fn load(&self) -> Result<DocumentMut> {
        let path = self.path();
        if !path.exists() {
            let mut doc = DocumentMut::new();
            doc.decor_mut().set_prefix(HEADER);
            return Ok(doc);
        }
        fs::read_to_string(&path)?.parse::<DocumentMut>().with_context(|| format!("invalid {}", path.display()))
    }

    fn save(&self, doc: &DocumentMut) -> Result<()> {
        write_private(&self.path(), &doc.to_string())
    }

    /// Our identity, created on first use.
    pub fn identity(&self) -> Result<LocalIdentity> {
        let mut doc = self.load()?;
        if let Some(t) = doc.get("controller").and_then(Item::as_table) {
            let get = |k: &str| t.get(k).and_then(Item::as_str).ok_or_else(|| anyhow!("[controller] {k} missing"));
            let key: [u8; 32] = hex::decode(get("signing_key")?)?
                .try_into()
                .map_err(|_| anyhow!("[controller] signing_key must be 32 bytes"))?;
            return Ok(LocalIdentity {
                pairing: Identity {
                    pairing_id: get("pairing_id")?.to_string(),
                    signing_key: SigningKey::from_bytes(&key),
                },
                device_id: get("device_id")?.to_string(),
            });
        }
        let id = LocalIdentity { pairing: Identity::generate(), device_id: crate::random::mac_address() };
        set_controller(&mut doc, &id);
        self.save(&doc)?;
        Ok(id)
    }

    /// Settings for a speaker. A speaker seen for the first time is recorded
    /// with defaults for its model, so the user can find and edit it.
    pub fn device_settings(&self, device: &Device) -> Result<DeviceSettings> {
        let mut doc = self.load()?;
        let defaults =
            inherit_lower_limits(&doc, &device.name, DeviceSettings::defaults_for(&device.manufacturer, &device.model));
        let entry = device_table(&mut doc, &device.device_id);
        let model = format!("{} {}", device.manufacturer, device.model).trim().to_string();
        let mut changed = false;
        let fresh: [(&str, Item); 4] = [
            ("model", value(model)),
            ("max_volume_db", value(defaults.max_volume_db as f64)),
            ("default_volume_db", value(defaults.default_volume_db as f64)),
            ("latency_offset_ms", value(defaults.latency_offset_ms as i64)),
        ];
        // The display name follows the speaker; the rest is only filled in.
        if entry.get("name").and_then(Item::as_str) != Some(device.name.as_str()) {
            entry.insert("name", value(device.name.as_str()));
            changed = true;
        }
        for (key, item) in fresh {
            if !entry.contains_key(key) {
                entry.insert(key, item);
                changed = true;
            }
        }

        let num = |k: &str, d: f32| {
            entry
                .get(k)
                .and_then(|i| i.as_float().or_else(|| i.as_integer().map(|n| n as f64)))
                .map(|v| v as f32)
                .unwrap_or(d)
        };
        let settings = DeviceSettings {
            max_volume_db: num("max_volume_db", defaults.max_volume_db),
            default_volume_db: num("default_volume_db", defaults.default_volume_db),
            latency_offset_ms: entry
                .get("latency_offset_ms")
                .and_then(Item::as_integer)
                .unwrap_or(defaults.latency_offset_ms as i64) as i32,
        }
        .sanitized(defaults);
        if changed {
            self.save(&doc)?;
        }
        Ok(settings)
    }

    /// Pairing keys for a speaker about to be connected.
    ///
    /// Refuses a speaker that claims the name of a PIN-paired speaker but
    /// presents a different device ID: that is either a replaced device or an
    /// impostor, and silently falling back to transient (unauthenticated)
    /// pairing would hand it our audio.
    pub fn keys_for(&self, device: &Device) -> Result<Option<DeviceKeys>> {
        if let Some(keys) = self.device_keys(&device.device_id)? {
            return Ok(Some(keys));
        }
        let doc = self.load()?;
        // Match on the name recorded at pairing time (`pairing.name`), never
        // on the display `name`, which follows whatever the network advertises.
        let paired_same_name = doc.get("devices").and_then(Item::as_table).is_some_and(|devices| {
            devices.iter().any(|(_, entry)| {
                entry
                    .get("pairing")
                    .and_then(|p| p.get("name"))
                    .and_then(Item::as_str)
                    .is_some_and(|n| n.eq_ignore_ascii_case(&device.name))
            })
        });
        if paired_same_name {
            bail!(
                "{:?} is paired, but this device has a different identity ({}). If you replaced the \
                 speaker, run `airpipe unpair` and `airpipe pair` again; otherwise something on your \
                 network is impersonating it",
                device.name,
                device.device_id
            );
        }
        Ok(None)
    }

    pub fn device_keys(&self, device_id: &str) -> Result<Option<DeviceKeys>> {
        let doc = self.load()?;
        let Some(p) = doc.get("devices").and_then(|d| d.get(device_id)).and_then(|d| d.get("pairing")) else {
            return Ok(None);
        };
        let accessory_id =
            p.get("accessory_id").and_then(Item::as_str).ok_or_else(|| anyhow!("pairing.accessory_id missing"))?;
        let pk: [u8; 32] = hex::decode(p.get("public_key").and_then(Item::as_str).unwrap_or_default())?
            .try_into()
            .map_err(|_| anyhow!("{FILE_NAME}: bad pairing.public_key for {device_id}"))?;
        Ok(Some(DeviceKeys { accessory_id: accessory_id.to_string(), accessory_public_key: pk }))
    }

    pub fn save_device_keys(&self, device_id: &str, name: &str, keys: &DeviceKeys) -> Result<()> {
        let mut doc = self.load()?;
        let entry = device_table(&mut doc, device_id);
        if !entry.contains_key("name") {
            entry.insert("name", value(name));
        }
        let mut pairing = Table::new();
        // Name at pairing time; never updated from the network (see keys_for).
        pairing.insert("name", value(name));
        pairing.insert("accessory_id", value(keys.accessory_id.as_str()));
        pairing.insert("public_key", value(hex::encode(keys.accessory_public_key)));
        entry.insert("pairing", Item::Table(pairing));
        self.save(&doc)
    }

    /// Returns true if a pairing was removed. Per-device settings are kept.
    pub fn remove_device_keys(&self, device_id: &str) -> Result<bool> {
        let mut doc = self.load()?;
        let removed = doc
            .get_mut("devices")
            .and_then(|d| d.get_mut(device_id))
            .and_then(Item::as_table_mut)
            .and_then(|t| t.remove("pairing"))
            .is_some();
        if removed {
            self.save(&doc)?;
        }
        Ok(removed)
    }
}

/// A speaker can reappear under a new device ID (another network interface,
/// a stereo pair being formed). If an existing entry has the same name and
/// lower volume limits, the new entry starts with those: a user's lowered
/// ceiling must not be lost to louder model defaults.
fn inherit_lower_limits(doc: &DocumentMut, name: &str, mut defaults: DeviceSettings) -> DeviceSettings {
    let Some(devices) = doc.get("devices").and_then(Item::as_table) else { return defaults };
    let num = |entry: &Item, key: &str| {
        entry.get(key).and_then(|i| i.as_float().or_else(|| i.as_integer().map(|n| n as f64))).map(|v| v as f32)
    };
    for (_, entry) in devices.iter() {
        if !entry.get("name").and_then(Item::as_str).is_some_and(|n| n.eq_ignore_ascii_case(name)) {
            continue;
        }
        if let Some(max) = num(entry, "max_volume_db").filter(|v| v.is_finite()) {
            defaults.max_volume_db = defaults.max_volume_db.min(max);
        }
        if let Some(default) = num(entry, "default_volume_db").filter(|v| v.is_finite()) {
            defaults.default_volume_db = defaults.default_volume_db.min(default);
        }
    }
    defaults
}

fn set_controller(doc: &mut DocumentMut, id: &LocalIdentity) {
    let mut t = Table::new();
    t.decor_mut().set_prefix("\n# Our pairing identity, generated on first run. signing_key is SECRET.\n");
    t.insert("pairing_id", value(id.pairing.pairing_id.as_str()));
    t.insert("device_id", value(id.device_id.as_str()));
    t.insert("signing_key", value(hex::encode(id.pairing.signing_key.to_bytes())));
    doc.insert("controller", Item::Table(t));
}

/// `[devices."<id>"]`, created if needed (keeping `[devices]` itself implicit).
fn device_table<'a>(doc: &'a mut DocumentMut, device_id: &str) -> &'a mut Table {
    let devices = doc.entry("devices").or_insert_with(|| {
        let mut t = Table::new();
        t.set_implicit(true);
        Item::Table(t)
    });
    let devices = devices.as_table_mut().expect("[devices] is a table");
    devices
        .entry(device_id)
        .or_insert_with(|| {
            let mut t = Table::new();
            t.decor_mut().set_prefix("\n");
            Item::Table(t)
        })
        .as_table_mut()
        .expect("device entry is a table")
}

/// Atomically writes a file readable only by the current user.
fn write_private(path: &Path, contents: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    // A fresh, exclusive temp name: concurrent writers never share a file.
    let tmp = path.with_extension(format!("tmp.{}", hex::encode(crate::random::bytes::<6>())));
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("writing {}", tmp.display()))?;
    f.write_all(contents.as_bytes())?;
    f.sync_all()?;
    fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("airpipe-test-{}", crate::random::uuid()))
    }

    fn device(name: &str, manufacturer: &str, model: &str) -> Device {
        Device {
            name: name.into(),
            address: "127.0.0.1:7000".parse().unwrap(),
            device_id: format!("ID-{name}"),
            model: model.into(),
            manufacturer: manufacturer.into(),
            firmware: String::new(),
            features: 0,
            flags: 0,
        }
    }

    #[test]
    fn identity_is_created_once_and_private() {
        let dir = temp_dir();
        let store = Store::open(dir.clone());
        let a = store.identity().unwrap();
        let b = store.identity().unwrap();
        assert_eq!(a.pairing.pairing_id, b.pairing.pairing_id);
        assert_eq!(a.device_id, b.device_id);
        assert_eq!(a.pairing.signing_key.to_bytes(), b.pairing.signing_key.to_bytes());
        let mode = fs::metadata(store.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn devices_are_recorded_with_model_defaults() {
        let dir = temp_dir();
        let store = Store::open(dir.clone());
        let loud = store.device_settings(&device("Bureau", "Devialet", "Phantom 103 dB")).unwrap();
        assert_eq!(loud.max_volume_db, -20.0);
        assert_eq!(loud.default_volume_db, -26.0);
        let plain = store.device_settings(&device("Kitchen", "Sonos", "One")).unwrap();
        assert_eq!(plain.max_volume_db, -10.0);
        let text = fs::read_to_string(store.path()).unwrap();
        assert!(text.contains("ID-Bureau"), "{text}");
        assert!(text.contains("model = \"Devialet Phantom 103 dB\""), "{text}");
        assert!(text.starts_with("# airpipe device memory"), "{text}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn user_edits_and_comments_survive_updates() {
        let dir = temp_dir();
        let store = Store::open(dir.clone());
        let dev = device("Bureau", "Devialet", "Phantom");
        store.identity().unwrap();
        store.device_settings(&dev).unwrap();

        let original = fs::read_to_string(store.path()).unwrap();
        let edited = original
            .replace("max_volume_db = -20.0", "max_volume_db = -15.0 # louder in the office")
            .replace("latency_offset_ms = 0", "latency_offset_ms = 40");
        assert_ne!(original, edited, "edit applied:\n{original}");
        fs::write(store.path(), edited).unwrap();

        let s = store.device_settings(&dev).unwrap();
        assert_eq!(s.max_volume_db, -15.0);
        assert_eq!(s.latency_offset_ms, 40);

        let keys = DeviceKeys { accessory_id: "acc".into(), accessory_public_key: [3u8; 32] };
        store.save_device_keys(&dev.device_id, &dev.name, &keys).unwrap();
        assert_eq!(store.device_keys(&dev.device_id).unwrap(), Some(keys));
        let text = fs::read_to_string(store.path()).unwrap();
        assert!(text.contains("# louder in the office"), "comment kept:\n{text}");

        assert!(store.remove_device_keys(&dev.device_id).unwrap());
        assert!(!store.remove_device_keys(&dev.device_id).unwrap());
        assert_eq!(store.device_settings(&dev).unwrap().max_volume_db, -15.0, "settings kept after unpair");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn new_device_id_keeps_a_lowered_ceiling() {
        let dir = temp_dir();
        let store = Store::open(dir.clone());
        let old = device("Kitchen", "Sonos", "One");
        store.device_settings(&old).unwrap();
        let lowered =
            fs::read_to_string(store.path()).unwrap().replace("max_volume_db = -10.0", "max_volume_db = -24.0");
        fs::write(store.path(), lowered).unwrap();

        let mut moved = device("Kitchen", "Sonos", "One");
        moved.device_id = "NEW-ID".into();
        let s = store.device_settings(&moved).unwrap();
        assert_eq!(s.max_volume_db, -24.0, "the user's lower ceiling carries over");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn out_of_range_values_are_clamped() {
        let d = DeviceSettings::conservative();
        let s = DeviceSettings { max_volume_db: 6.0, default_volume_db: 3.0, latency_offset_ms: 99_999 }.sanitized(d);
        assert_eq!(s.max_volume_db, 0.0);
        assert_eq!(s.default_volume_db, 0.0);
        assert_eq!(s.latency_offset_ms, 1000);
        let s = DeviceSettings { max_volume_db: -25.0, default_volume_db: -10.0, latency_offset_ms: 0 }.sanitized(d);
        assert_eq!(s.default_volume_db, -25.0, "default never above the ceiling");
    }

    #[test]
    fn non_numbers_fall_back_to_defaults() {
        let d = DeviceSettings::conservative();
        let s = DeviceSettings { max_volume_db: f32::NAN, default_volume_db: f32::INFINITY, latency_offset_ms: 0 }
            .sanitized(d);
        assert_eq!(s.max_volume_db, d.max_volume_db);
        assert_eq!(s.default_volume_db, d.default_volume_db);
    }

    #[test]
    fn impostor_of_a_paired_speaker_is_refused() {
        let dir = temp_dir();
        let store = Store::open(dir.clone());
        let real = device("Bureau", "Devialet", "Phantom");
        let keys = DeviceKeys { accessory_id: "acc".into(), accessory_public_key: [3u8; 32] };
        store.save_device_keys(&real.device_id, &real.name, &keys).unwrap();
        assert_eq!(store.keys_for(&real).unwrap(), Some(keys));

        let mut impostor = device("bureau", "Devialet", "Phantom");
        impostor.device_id = "AA:BB:CC:DD:EE:FF".into();
        assert!(store.keys_for(&impostor).is_err(), "same name, other identity, no keys: refuse");

        let other = device("Kitchen", "Sonos", "One");
        assert_eq!(store.keys_for(&other).unwrap(), None, "unrelated unpaired speakers use transient pairing");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn renaming_the_paired_entry_does_not_disable_the_guard() {
        let dir = temp_dir();
        let store = Store::open(dir.clone());
        let real = device("Bureau", "Devialet", "Phantom");
        let keys = DeviceKeys { accessory_id: "acc".into(), accessory_public_key: [3u8; 32] };
        store.save_device_keys(&real.device_id, &real.name, &keys).unwrap();

        // Step 1: an attacker advertises the paired device ID under another
        // name, which renames the display entry.
        let mut renamed = device("Kitchen", "Devialet", "Phantom");
        renamed.device_id = real.device_id.clone();
        store.device_settings(&renamed).unwrap();

        // Step 2: it now claims "Bureau" with a new identity. Still refused.
        let mut impostor = device("Bureau", "Devialet", "Phantom");
        impostor.device_id = "AA:BB:CC:DD:EE:FF".into();
        assert!(store.keys_for(&impostor).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
