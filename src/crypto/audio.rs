//! Encryption of realtime audio packets.
//!
//! The sender picks a random 32-byte key and hands it to each speaker in the
//! stream `SETUP` (`shk`). Each packet is
//!
//! ```text
//! RTP header (12, clear) | ChaCha20-Poly1305(payload) | tag (16) | nonce tail (8)
//! ```
//!
//! with the RTP timestamp and SSRC (header bytes 4..12) as AAD, and the nonce
//! `[0; 4] || tail`. The receiver reads the nonce from the packet, so any
//! unique value works; we use a packet counter. Because the key is ours, one
//! key is shared by every speaker in a group and each packet is encrypted once.

use chacha20poly1305::{AeadInOut, ChaCha20Poly1305, KeyInit};

use crate::proto::rtp::{AUDIO_HEADER_LEN, AudioHeader};

pub struct AudioCipher {
    cipher: ChaCha20Poly1305,
}

impl AudioCipher {
    pub fn new(key: [u8; 32]) -> Self {
        Self { cipher: ChaCha20Poly1305::new(&key.into()) }
    }

    /// Writes a complete encrypted packet into `out`, reusing its allocation
    /// so the real-time thread does not allocate per packet.
    pub fn seal_into(&self, header: &AudioHeader, payload: &[u8], nonce_tail: u64, out: &mut Vec<u8>) {
        let head = header.encode();
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(&nonce_tail.to_le_bytes());

        out.clear();
        out.reserve(AUDIO_HEADER_LEN + payload.len() + 16 + 8);
        out.extend_from_slice(&head);
        out.extend_from_slice(payload);
        let tag = self
            .cipher
            .encrypt_inout_detached(&nonce.into(), &head[4..12], out[AUDIO_HEADER_LEN..].as_mut().into())
            .expect("audio payloads are far below the AEAD size limit");
        out.extend_from_slice(&tag);
        out.extend_from_slice(&nonce[4..]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chacha20poly1305::Tag;

    #[test]
    fn packet_layout_and_decryption() {
        let c = AudioCipher::new([5u8; 32]);
        let header = AudioHeader { marker: false, seq: 10, timestamp: 1234, ssrc: 99 };
        let mut pkt = Vec::new();
        c.seal_into(&header, b"payload!", 77, &mut pkt);
        assert_eq!(pkt.len(), 12 + 8 + 16 + 8);
        assert_eq!(&pkt[..12], &header.encode());
        assert_ne!(&pkt[12..20], b"payload!");
        assert_eq!(u64::from_le_bytes(pkt[36..44].try_into().unwrap()), 77);

        // Decrypt the way a receiver does.
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(&pkt[36..44]);
        let mut body = pkt[12..20].to_vec();
        let tag = Tag::try_from(&pkt[20..36]).unwrap();
        ChaCha20Poly1305::new(&[5u8; 32].into())
            .decrypt_inout_detached(&nonce.into(), &pkt[4..12], body.as_mut_slice().into(), &tag)
            .unwrap();
        assert_eq!(body, b"payload!");
    }
}
