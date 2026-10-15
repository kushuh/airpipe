//! Encrypted framing for the RTSP control channel and the event channel.
//!
//! After pairing, both TCP connections switch to HomeKit-style framing:
//!
//! ```text
//! length (u16 LE, ≤ 1024) | ciphertext (length bytes) | Poly1305 tag (16 bytes)
//! ```
//!
//! The length field is the AAD, and the nonce is `[0; 4] || counter (u64 LE)`,
//! with an independent counter per direction starting at 0.
//!
//! Keys are derived with HKDF-SHA512 from the pairing shared secret. The event
//! channel is a reverse connection, so the client's *write* key there is the
//! one labelled "Read" (as observed by OwnTone's pair_ap).

use chacha20poly1305::{AeadInOut, ChaCha20Poly1305, KeyInit, Nonce, Tag};
use hkdf::Hkdf;
use sha2::Sha512;
use thiserror::Error;

const MAX_BLOCK: usize = 1024;
const TAG_LEN: usize = 16;

#[derive(Debug, Error)]
pub enum ChannelError {
    #[error("failed to authenticate an encrypted block (wrong keys or corrupted stream)")]
    Decrypt,
}

/// HKDF-SHA512 extract-and-expand into a 32-byte key.
pub fn derive_key(ikm: &[u8], salt: &str, info: &str) -> [u8; 32] {
    let hk = Hkdf::<Sha512>::new(Some(salt.as_bytes()), ikm);
    let mut okm = [0u8; 32];
    hk.expand(info.as_bytes(), &mut okm).expect("32 bytes is a valid HKDF-SHA512 output length");
    okm
}

/// Which connection a cipher is for.
#[derive(Debug, Clone, Copy)]
pub enum Channel {
    Control,
    Events,
}

fn nonce(counter: u64) -> Nonce {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&counter.to_le_bytes());
    n.into()
}

/// Bidirectional cipher state for one connection, from the client's side.
pub struct ChannelCipher {
    write: ChaCha20Poly1305,
    read: ChaCha20Poly1305,
    write_counter: u64,
    read_counter: u64,
}

impl ChannelCipher {
    pub fn new(shared_secret: &[u8], channel: Channel) -> Self {
        let (salt, write_info, read_info) = match channel {
            Channel::Control => ("Control-Salt", "Control-Write-Encryption-Key", "Control-Read-Encryption-Key"),
            Channel::Events => ("Events-Salt", "Events-Read-Encryption-Key", "Events-Write-Encryption-Key"),
        };
        let write_key = derive_key(shared_secret, salt, write_info);
        let read_key = derive_key(shared_secret, salt, read_info);
        Self::from_keys(write_key, read_key)
    }

    pub fn from_keys(write_key: [u8; 32], read_key: [u8; 32]) -> Self {
        Self {
            write: ChaCha20Poly1305::new(&write_key.into()),
            read: ChaCha20Poly1305::new(&read_key.into()),
            write_counter: 0,
            read_counter: 0,
        }
    }

    /// Encrypts `plain` into one or more framed blocks.
    pub fn encrypt(&mut self, plain: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(plain.len() + (plain.len() / MAX_BLOCK + 1) * (2 + TAG_LEN));
        for chunk in plain.chunks(MAX_BLOCK) {
            let len = (chunk.len() as u16).to_le_bytes();
            let mut block = chunk.to_vec();
            let tag = self
                .write
                .encrypt_inout_detached(&nonce(self.write_counter), &len, block.as_mut_slice().into())
                .expect("encryption of a ≤1024 byte block cannot fail");
            self.write_counter += 1;
            out.extend_from_slice(&len);
            out.extend_from_slice(&block);
            out.extend_from_slice(&tag);
        }
        out
    }

    /// Decrypts every complete block at the front of `buf`, removing them.
    ///
    /// A partial trailing block is left in `buf` for the next call.
    pub fn decrypt(&mut self, buf: &mut Vec<u8>) -> Result<Vec<u8>, ChannelError> {
        let mut plain = Vec::new();
        let mut pos = 0;
        while buf.len() - pos >= 2 {
            let len = u16::from_le_bytes([buf[pos], buf[pos + 1]]) as usize;
            let end = pos + 2 + len + TAG_LEN;
            if buf.len() < end {
                break;
            }
            let aad = [buf[pos], buf[pos + 1]];
            let mut block = buf[pos + 2..pos + 2 + len].to_vec();
            let tag = Tag::try_from(&buf[end - TAG_LEN..end]).expect("slice is exactly 16 bytes");
            self.read
                .decrypt_inout_detached(&nonce(self.read_counter), &aad, block.as_mut_slice().into(), &tag)
                .map_err(|_| ChannelError::Decrypt)?;
            self.read_counter += 1;
            plain.extend_from_slice(&block);
            pos = end;
        }
        buf.drain(..pos);
        Ok(plain)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A peer that mirrors our keys, as the speaker would.
    fn peer_of(secret: &[u8], channel: Channel) -> ChannelCipher {
        // Swap roles: what we write, the peer reads.
        let (salt, w, r) = match channel {
            Channel::Control => ("Control-Salt", "Control-Write-Encryption-Key", "Control-Read-Encryption-Key"),
            Channel::Events => ("Events-Salt", "Events-Read-Encryption-Key", "Events-Write-Encryption-Key"),
        };
        ChannelCipher::from_keys(derive_key(secret, salt, r), derive_key(secret, salt, w))
    }

    #[test]
    fn roundtrip_across_blocks_and_partial_reads() {
        let secret = [42u8; 64];
        let mut me = ChannelCipher::new(&secret, Channel::Control);
        let mut peer = peer_of(&secret, Channel::Control);

        let msg: Vec<u8> = (0..2500u32).map(|i| i as u8).collect();
        let wire = me.encrypt(&msg);
        assert_eq!(wire.len(), msg.len() + 3 * (2 + 16), "three blocks");

        // Feed the peer in two pieces, splitting a block.
        let mut buf = wire[..1500].to_vec();
        let mut got = peer.decrypt(&mut buf).unwrap();
        assert_eq!(got.len(), 1024);
        buf.extend_from_slice(&wire[1500..]);
        got.extend(peer.decrypt(&mut buf).unwrap());
        assert!(buf.is_empty());
        assert_eq!(got, msg);

        // Counters advance: a second message still decrypts.
        let mut wire2 = me.encrypt(b"again");
        assert_eq!(peer.decrypt(&mut wire2).unwrap(), b"again");
    }

    #[test]
    fn tampering_is_detected() {
        let secret = [1u8; 32];
        let mut me = ChannelCipher::new(&secret, Channel::Events);
        let mut peer = peer_of(&secret, Channel::Events);
        let mut wire = me.encrypt(b"hello");
        wire[3] ^= 1;
        assert!(peer.decrypt(&mut wire).is_err());
    }

    #[test]
    fn hkdf_known_answer() {
        // RFC 5869 has no SHA-512 vectors. Expected value computed independently
        // with Python's hmac/hashlib (HKDF-SHA512, ikm = 32 zero bytes).
        let key = derive_key(&[0u8; 32], "Control-Salt", "Control-Write-Encryption-Key");
        assert_eq!(hex::encode(key), KNOWN_CONTROL_WRITE_KEY);
    }

    const KNOWN_CONTROL_WRITE_KEY: &str = include_str!("testdata/hkdf_control_write.hex");
}
