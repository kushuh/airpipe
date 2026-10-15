//! The RTSP side of pairing: POSTs the messages built by
//! [`crate::crypto::pairing`] to `/pair-setup` and `/pair-verify`.

use anyhow::{Result, anyhow, bail};

use super::connection::{Connection, USER_AGENT};
use crate::crypto::pairing::{DeviceKeys, Setup, Verify};
use crate::discovery::Device;
use crate::proto::rtsp::Request;
use crate::store::LocalIdentity;

/// RTSP status for "pairing required" (Connection Authorization Required).
const STATUS_AUTH_REQUIRED: u16 = 470;
/// `X-Apple-HKP` value for pairing with long-term keys (PIN or password).
/// Transient pairing uses [`Setup::hkp_header`] ("4").
const HKP_NORMAL: &str = "3";

/// Transient pairing (no PIN). Returns the shared secret.
pub(super) async fn transient(conn: &mut Connection) -> Result<Vec<u8>> {
    let mut setup = Setup::transient();
    let post = |body: Vec<u8>, hkp: &str| {
        Request::new("POST", "/pair-setup").header("X-Apple-HKP", hkp).body("application/octet-stream", body)
    };
    let m2 = conn.request(post(setup.m1(), setup.hkp_header())).await?;
    if m2.status() == Some(STATUS_AUTH_REQUIRED) {
        bail!("speaker requires PIN pairing: run `airpipe pair`");
    }
    if m2.status() != Some(200) {
        bail!("pair-setup M1 rejected: {} {}", m2.status().unwrap_or(0), m2.reason());
    }
    let m3 = setup.m3(&m2.body)?;
    let m4 = conn.request_ok(post(m3, setup.hkp_header())).await?;
    setup.m4(&m4.body)?.ok_or_else(|| anyhow!("transient pairing did not yield a key"))
}

/// pair-verify with keys from an earlier `airpipe pair`. Returns the shared secret.
pub(super) async fn verify(conn: &mut Connection, identity: &LocalIdentity, keys: &DeviceKeys) -> Result<Vec<u8>> {
    let mut verify = Verify::new();
    let post = |body: Vec<u8>| {
        Request::new("POST", "/pair-verify").header("X-Apple-HKP", HKP_NORMAL).body("application/octet-stream", body)
    };
    let m2 = conn.request_ok(post(verify.m1())).await?;
    let m3 = verify.m3(&m2.body, &identity.pairing, keys)?;
    let m4 = conn.request_ok(post(m3)).await?;
    Ok(verify.m4(&m4.body)?)
}

/// Full PIN/password pairing (`airpipe pair`). Returns the device keys to store.
pub async fn with_pin(
    device: &Device,
    identity: &LocalIdentity,
    ask_pin: impl FnOnce() -> Result<String>,
    password: Option<String>,
) -> Result<DeviceKeys> {
    let headers = vec![("User-Agent".to_string(), USER_AGENT.to_string())];
    let mut conn = Connection::connect(device.address, headers).await?;

    let secret = match password {
        Some(p) => p,
        None => {
            // Ask the device to display a PIN, then ask the user for it.
            conn.request_ok(Request::new("POST", "/pair-pin-start").header("X-Apple-HKP", HKP_NORMAL)).await?;
            ask_pin()?
        }
    };
    let mut setup = Setup::with_password(secret.trim());
    let post = |body: Vec<u8>| {
        Request::new("POST", "/pair-setup").header("X-Apple-HKP", HKP_NORMAL).body("application/octet-stream", body)
    };
    let m2 = conn.request_ok(post(setup.m1())).await?;
    let m3 = setup.m3(&m2.body)?;
    let m4 = conn.request_ok(post(m3)).await?;
    setup.m4(&m4.body)?;
    let m5 = setup.m5(&identity.pairing)?;
    let m6 = conn.request_ok(post(m5)).await?;
    Ok(setup.m6(&m6.body)?)
}
