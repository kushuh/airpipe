//! HomeKit-style pairing as used by AirPlay 2 (`/pair-setup`, `/pair-verify`).
//!
//! Three flows exist, all as pure state machines: each step consumes the
//! device's TLV8 response and produces the next request body. The RTSP layer
//! (`airplay::pair`) just POSTs them.
//!
//! * **Transient** (`X-Apple-HKP: 4`): SRP with the fixed password `3939`,
//!   no long-term keys. Used by speakers that allow anyone on the network
//!   to play (the common case). Result: the 64-byte SRP session key.
//! * **Setup** (`X-Apple-HKP: 3`): SRP with a PIN shown by the device (after
//!   `/pair-pin-start`) or the device password, followed by an exchange of
//!   Ed25519 long-term public keys, which we persist.
//! * **Verify**: proves possession of the long-term keys from a previous
//!   setup on every new connection. Result: an X25519 shared secret.
//!
//! The shared secret seeds the control and event channel ciphers.

use chacha20poly1305::{AeadInOut, ChaCha20Poly1305, KeyInit, Tag};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use thiserror::Error;
use x25519_dalek::{PublicKey as XPublic, StaticSecret};

use super::channel::derive_key;
use super::srp;
use crate::proto::tlv8::{self, Message, Type, Writer};

const METHOD_PAIR_SETUP: u8 = 0;
const FLAG_TRANSIENT: u8 = 0x10;
/// Password used for transient pairing by every AirPlay 2 sender.
const TRANSIENT_PASSWORD: &[u8] = b"3939";

#[derive(Debug, Error)]
pub enum PairingError {
    #[error("device rejected pairing: {0}")]
    Device(#[from] tlv8::DeviceError),
    #[error("malformed response from device: {0}")]
    Malformed(&'static str),
    #[error(transparent)]
    Srp(#[from] srp::SrpError),
    #[error("could not decrypt the device's pairing message")]
    Decrypt,
    #[error("device signature is invalid (not the device we paired with?)")]
    BadSignature,
}

type Result<T> = std::result::Result<T, PairingError>;

fn parse(body: &[u8], expected_state: u8) -> Result<Message> {
    let msg = Message::decode(body).map_err(|_| PairingError::Malformed("invalid TLV8"))?;
    if let Some(err) = msg.error() {
        return Err(err.into());
    }
    match msg.get_u8(Type::State) {
        Some(s) if s == expected_state => Ok(msg),
        _ => Err(PairingError::Malformed("unexpected pairing state")),
    }
}

/// ChaCha20-Poly1305 with an 8-byte ASCII label as nonce, as used inside
/// pairing messages ("PS-Msg05", "PV-Msg02", ...). Tag is appended.
fn seal_labeled(key: &[u8; 32], label: &[u8; 8], plain: &[u8]) -> Vec<u8> {
    let mut nonce = [0u8; 12];
    nonce[4..].copy_from_slice(label);
    let mut buf = plain.to_vec();
    let tag = ChaCha20Poly1305::new(&(*key).into())
        .encrypt_inout_detached(&nonce.into(), &[], buf.as_mut_slice().into())
        .expect("pairing messages are small");
    buf.extend_from_slice(&tag);
    buf
}

fn open_labeled(key: &[u8; 32], label: &[u8; 8], sealed: &[u8]) -> Result<Vec<u8>> {
    if sealed.len() < 16 {
        return Err(PairingError::Decrypt);
    }
    let (body, tag) = sealed.split_at(sealed.len() - 16);
    let mut nonce = [0u8; 12];
    nonce[4..].copy_from_slice(label);
    let mut buf = body.to_vec();
    let tag = Tag::try_from(tag).map_err(|_| PairingError::Decrypt)?;
    ChaCha20Poly1305::new(&(*key).into())
        .decrypt_inout_detached(&nonce.into(), &[], buf.as_mut_slice().into(), &tag)
        .map_err(|_| PairingError::Decrypt)?;
    Ok(buf)
}

/// Our long-term identity as a pairing controller.
#[derive(Clone)]
pub struct Identity {
    /// Identifier sent as `Identifier` TLV; stable across runs.
    pub pairing_id: String,
    pub signing_key: SigningKey,
}

impl Identity {
    pub fn generate() -> Self {
        Self { pairing_id: crate::random::uuid(), signing_key: SigningKey::from_bytes(&crate::random::bytes()) }
    }
}

/// What a successful setup teaches us about the device; persisted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceKeys {
    pub accessory_id: String,
    pub accessory_public_key: [u8; 32],
}

// ---------------------------------------------------------------------------
// pair-setup (transient and PIN/password)
// ---------------------------------------------------------------------------

/// pair-setup client. Transient pairing stops after M4.
pub struct Setup {
    password: Vec<u8>,
    transient: bool,
    srp: Option<srp::Client>,
    session_key: Option<[u8; 64]>,
}

impl Setup {
    /// Transient pairing: no PIN, no long-term keys.
    pub fn transient() -> Self {
        Self { password: TRANSIENT_PASSWORD.to_vec(), transient: true, srp: None, session_key: None }
    }

    /// Full pairing with a PIN or device password.
    pub fn with_password(password: &str) -> Self {
        Self { password: password.as_bytes().to_vec(), transient: false, srp: None, session_key: None }
    }

    /// Value of the `X-Apple-HKP` header that must accompany every request.
    pub fn hkp_header(&self) -> &'static str {
        if self.transient { "4" } else { "3" }
    }

    /// M1: start pairing.
    pub fn m1(&self) -> Vec<u8> {
        let w = Writer::new().add_u8(Type::State, 1).add_u8(Type::Method, METHOD_PAIR_SETUP);
        if self.transient { w.add_u8(Type::Flags, FLAG_TRANSIENT) } else { w }.finish()
    }

    /// Consumes M2 (salt + server key), returns M3 (our key + proof).
    pub fn m3(&mut self, m2: &[u8]) -> Result<Vec<u8>> {
        let msg = parse(m2, 2)?;
        let salt = msg.get(Type::Salt).ok_or(PairingError::Malformed("M2 without salt"))?;
        let server_pk = msg.get(Type::PublicKey).ok_or(PairingError::Malformed("M2 without public key"))?;
        let client = srp::Client::new(&self.password, salt, server_pk, &crate::random::bytes())?;
        let body = Writer::new()
            .add_u8(Type::State, 3)
            .add(Type::PublicKey, &client.public_key)
            .add(Type::Proof, &client.proof)
            .finish();
        self.srp = Some(client);
        Ok(body)
    }

    /// Consumes M4 (server proof). For transient pairing this completes the
    /// exchange and returns the shared secret.
    pub fn m4(&mut self, m4: &[u8]) -> Result<Option<Vec<u8>>> {
        let msg = parse(m4, 4)?;
        let proof = msg.get(Type::Proof).ok_or(PairingError::Malformed("M4 without proof"))?;
        let client = self.srp.take().ok_or(PairingError::Malformed("M4 before M3"))?;
        let key = client.verify(proof)?;
        self.session_key = Some(key);
        Ok(self.transient.then(|| key.to_vec()))
    }

    /// M5: send our long-term public key, signed (non-transient only).
    pub fn m5(&self, identity: &Identity) -> Result<Vec<u8>> {
        let k = self.session_key.ok_or(PairingError::Malformed("M5 before M4"))?;
        let controller_x = derive_key(&k, "Pair-Setup-Controller-Sign-Salt", "Pair-Setup-Controller-Sign-Info");
        let ltpk = identity.signing_key.verifying_key().to_bytes();
        let mut info = controller_x.to_vec();
        info.extend_from_slice(identity.pairing_id.as_bytes());
        info.extend_from_slice(&ltpk);
        let signature = identity.signing_key.sign(&info).to_bytes();

        let sub = Writer::new()
            .add(Type::Identifier, identity.pairing_id.as_bytes())
            .add(Type::PublicKey, &ltpk)
            .add(Type::Signature, &signature)
            .finish();
        let key = derive_key(&k, "Pair-Setup-Encrypt-Salt", "Pair-Setup-Encrypt-Info");
        let sealed = seal_labeled(&key, b"PS-Msg05", &sub);
        Ok(Writer::new().add_u8(Type::State, 5).add(Type::EncryptedData, &sealed).finish())
    }

    /// Consumes M6: the device's long-term public key, signed.
    pub fn m6(&self, m6: &[u8]) -> Result<DeviceKeys> {
        let k = self.session_key.ok_or(PairingError::Malformed("M6 before M4"))?;
        let msg = parse(m6, 6)?;
        let sealed = msg.get(Type::EncryptedData).ok_or(PairingError::Malformed("M6 without data"))?;
        let key = derive_key(&k, "Pair-Setup-Encrypt-Salt", "Pair-Setup-Encrypt-Info");
        let sub = Message::decode(&open_labeled(&key, b"PS-Msg06", sealed)?)
            .map_err(|_| PairingError::Malformed("invalid TLV8 in M6"))?;

        let id = sub.get(Type::Identifier).ok_or(PairingError::Malformed("M6 without identifier"))?;
        let pk: [u8; 32] =
            sub.get(Type::PublicKey).and_then(|v| v.try_into().ok()).ok_or(PairingError::Malformed("M6 public key"))?;
        let sig: [u8; 64] =
            sub.get(Type::Signature).and_then(|v| v.try_into().ok()).ok_or(PairingError::Malformed("M6 signature"))?;

        let accessory_x = derive_key(&k, "Pair-Setup-Accessory-Sign-Salt", "Pair-Setup-Accessory-Sign-Info");
        let mut info = accessory_x.to_vec();
        info.extend_from_slice(id);
        info.extend_from_slice(&pk);
        let vk = VerifyingKey::from_bytes(&pk).map_err(|_| PairingError::BadSignature)?;
        vk.verify(&info, &Signature::from_bytes(&sig)).map_err(|_| PairingError::BadSignature)?;

        Ok(DeviceKeys { accessory_id: String::from_utf8_lossy(id).into_owned(), accessory_public_key: pk })
    }
}

// ---------------------------------------------------------------------------
// pair-verify
// ---------------------------------------------------------------------------

/// pair-verify client for a device we have long-term keys for.
pub struct Verify {
    secret: StaticSecret,
    public: [u8; 32],
    shared: Option<[u8; 32]>,
}

impl Verify {
    pub fn new() -> Self {
        let secret = StaticSecret::from(crate::random::bytes::<32>());
        let public = XPublic::from(&secret).to_bytes();
        Self { secret, public, shared: None }
    }

    pub fn m1(&self) -> Vec<u8> {
        Writer::new().add_u8(Type::State, 1).add(Type::PublicKey, &self.public).finish()
    }

    /// Consumes M2 (device ephemeral key + signed proof), returns M3.
    pub fn m3(&mut self, m2: &[u8], identity: &Identity, device: &DeviceKeys) -> Result<Vec<u8>> {
        let msg = parse(m2, 2)?;
        let device_public: [u8; 32] = msg
            .get(Type::PublicKey)
            .and_then(|v| v.try_into().ok())
            .ok_or(PairingError::Malformed("verify M2 public key"))?;
        let sealed = msg.get(Type::EncryptedData).ok_or(PairingError::Malformed("verify M2 without data"))?;

        let shared = self.secret.diffie_hellman(&XPublic::from(device_public)).to_bytes();
        let key = derive_key(&shared, "Pair-Verify-Encrypt-Salt", "Pair-Verify-Encrypt-Info");
        let sub = Message::decode(&open_labeled(&key, b"PV-Msg02", sealed)?)
            .map_err(|_| PairingError::Malformed("invalid TLV8 in verify M2"))?;

        let id = sub.get(Type::Identifier).ok_or(PairingError::Malformed("verify M2 identifier"))?;
        let sig: [u8; 64] = sub
            .get(Type::Signature)
            .and_then(|v| v.try_into().ok())
            .ok_or(PairingError::Malformed("verify M2 signature"))?;
        if id != device.accessory_id.as_bytes() {
            return Err(PairingError::BadSignature);
        }
        let mut info = device_public.to_vec();
        info.extend_from_slice(id);
        info.extend_from_slice(&self.public);
        VerifyingKey::from_bytes(&device.accessory_public_key)
            .map_err(|_| PairingError::BadSignature)?
            .verify(&info, &Signature::from_bytes(&sig))
            .map_err(|_| PairingError::BadSignature)?;

        let mut info = self.public.to_vec();
        info.extend_from_slice(identity.pairing_id.as_bytes());
        info.extend_from_slice(&device_public);
        let signature = identity.signing_key.sign(&info).to_bytes();
        let sub = Writer::new()
            .add(Type::Identifier, identity.pairing_id.as_bytes())
            .add(Type::Signature, &signature)
            .finish();
        let sealed = seal_labeled(&key, b"PV-Msg03", &sub);

        self.shared = Some(shared);
        Ok(Writer::new().add_u8(Type::State, 3).add(Type::EncryptedData, &sealed).finish())
    }

    /// Consumes M4 and returns the shared secret for the channel ciphers.
    pub fn m4(&self, m4: &[u8]) -> Result<Vec<u8>> {
        parse(m4, 4)?;
        self.shared.map(|s| s.to_vec()).ok_or(PairingError::Malformed("verify M4 before M3"))
    }
}

impl Default for Verify {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_m1_requests_transient_flag() {
        let s = Setup::transient();
        let m1 = Message::decode(&s.m1()).unwrap();
        assert_eq!(m1.get_u8(Type::State), Some(1));
        assert_eq!(m1.get_u8(Type::Method), Some(0));
        assert_eq!(m1.get_u8(Type::Flags), Some(0x10));
        assert_eq!(s.hkp_header(), "4");
        assert!(Message::decode(&Setup::with_password("1234").m1()).unwrap().get(Type::Flags).is_none());
    }

    #[test]
    fn device_errors_surface() {
        let mut s = Setup::transient();
        let err = s.m3(&Writer::new().add_u8(Type::State, 2).add_u8(Type::Error, 2).finish()).unwrap_err();
        assert!(matches!(err, PairingError::Device(tlv8::DeviceError::Authentication)));
    }

    #[test]
    fn labeled_seal_roundtrip() {
        let key = [9u8; 32];
        let sealed = seal_labeled(&key, b"PS-Msg05", b"inner tlv");
        assert_eq!(open_labeled(&key, b"PS-Msg05", &sealed).unwrap(), b"inner tlv");
        assert!(open_labeled(&key, b"PS-Msg06", &sealed).is_err());
    }

    /// Simulates the accessory side of pair-verify and checks both signatures.
    #[test]
    fn verify_against_simulated_accessory() {
        let identity = Identity::generate();
        let accessory_ltsk = SigningKey::from_bytes(&[7u8; 32]);
        let device = DeviceKeys {
            accessory_id: "50:5B:C2:9C:79:E5".into(),
            accessory_public_key: accessory_ltsk.verifying_key().to_bytes(),
        };

        let mut client = Verify::new();
        let m1 = Message::decode(&client.m1()).unwrap();
        let client_pub: [u8; 32] = m1.get(Type::PublicKey).unwrap().try_into().unwrap();

        // Accessory: ephemeral key, shared secret, signed M2.
        let acc_secret = StaticSecret::from([3u8; 32]);
        let acc_pub = XPublic::from(&acc_secret).to_bytes();
        let shared = acc_secret.diffie_hellman(&XPublic::from(client_pub)).to_bytes();
        let key = derive_key(&shared, "Pair-Verify-Encrypt-Salt", "Pair-Verify-Encrypt-Info");
        let mut info = acc_pub.to_vec();
        info.extend_from_slice(device.accessory_id.as_bytes());
        info.extend_from_slice(&client_pub);
        let sub = Writer::new()
            .add(Type::Identifier, device.accessory_id.as_bytes())
            .add(Type::Signature, &accessory_ltsk.sign(&info).to_bytes())
            .finish();
        let m2 = Writer::new()
            .add_u8(Type::State, 2)
            .add(Type::PublicKey, &acc_pub)
            .add(Type::EncryptedData, &seal_labeled(&key, b"PV-Msg02", &sub))
            .finish();

        let m3 = client.m3(&m2, &identity, &device).unwrap();

        // Accessory: check the controller's signature in M3.
        let m3 = Message::decode(&m3).unwrap();
        let inner =
            Message::decode(&open_labeled(&key, b"PV-Msg03", m3.get(Type::EncryptedData).unwrap()).unwrap()).unwrap();
        assert_eq!(inner.get(Type::Identifier).unwrap(), identity.pairing_id.as_bytes());
        let mut info = client_pub.to_vec();
        info.extend_from_slice(identity.pairing_id.as_bytes());
        info.extend_from_slice(&acc_pub);
        let sig: [u8; 64] = inner.get(Type::Signature).unwrap().try_into().unwrap();
        identity.signing_key.verifying_key().verify(&info, &Signature::from_bytes(&sig)).unwrap();

        let secret = client.m4(&Writer::new().add_u8(Type::State, 4).finish()).unwrap();
        assert_eq!(secret, shared);
    }

    #[test]
    fn verify_rejects_wrong_accessory_key() {
        let identity = Identity::generate();
        let device = DeviceKeys {
            accessory_id: "dev".into(),
            accessory_public_key: SigningKey::from_bytes(&[1u8; 32]).verifying_key().to_bytes(),
        };
        let impostor = SigningKey::from_bytes(&[2u8; 32]);
        let mut client = Verify::new();
        let client_pub: [u8; 32] =
            Message::decode(&client.m1()).unwrap().get(Type::PublicKey).unwrap().try_into().unwrap();
        let acc_secret = StaticSecret::from([4u8; 32]);
        let acc_pub = XPublic::from(&acc_secret).to_bytes();
        let shared = acc_secret.diffie_hellman(&XPublic::from(client_pub)).to_bytes();
        let key = derive_key(&shared, "Pair-Verify-Encrypt-Salt", "Pair-Verify-Encrypt-Info");
        let mut info = acc_pub.to_vec();
        info.extend_from_slice(b"dev");
        info.extend_from_slice(&client_pub);
        let sub =
            Writer::new().add(Type::Identifier, b"dev").add(Type::Signature, &impostor.sign(&info).to_bytes()).finish();
        let m2 = Writer::new()
            .add_u8(Type::State, 2)
            .add(Type::PublicKey, &acc_pub)
            .add(Type::EncryptedData, &seal_labeled(&key, b"PV-Msg02", &sub))
            .finish();
        assert!(matches!(client.m3(&m2, &identity, &device), Err(PairingError::BadSignature)));
    }
}
