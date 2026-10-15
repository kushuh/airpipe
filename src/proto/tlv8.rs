//! TLV8 encoding used by HomeKit-style pairing (`/pair-setup`, `/pair-verify`).
//!
//! Each item is `type (1 byte) | length (1 byte) | value (length bytes)`.
//! Values longer than 255 bytes are split into consecutive fragments of the
//! same type; a decoder concatenates adjacent items that share a type.

use std::collections::BTreeMap;

use thiserror::Error;

/// TLV types defined by the HomeKit Accessory Protocol, plus `Flags` which
/// AirPlay uses to request transient pairing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Type {
    Method = 0x00,
    Identifier = 0x01,
    Salt = 0x02,
    PublicKey = 0x03,
    Proof = 0x04,
    EncryptedData = 0x05,
    State = 0x06,
    Error = 0x07,
    RetryDelay = 0x08,
    Certificate = 0x09,
    Signature = 0x0a,
    Permissions = 0x0b,
    FragmentData = 0x0d,
    FragmentLast = 0x0e,
    Flags = 0x13,
    Separator = 0xff,
}

/// Error codes a device may return in a `Type::Error` item.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum DeviceError {
    #[error("unknown error")]
    Unknown,
    #[error("authentication failed (wrong PIN or password?)")]
    Authentication,
    #[error("device asks to back off, retry later")]
    Backoff,
    #[error("device has reached its maximum number of pairings")]
    MaxPeers,
    #[error("too many failed attempts")]
    MaxTries,
    #[error("device is not accepting pairing now")]
    Unavailable,
    #[error("device is busy")]
    Busy,
    #[error("unrecognized error code {0}")]
    Other(u8),
}

impl From<u8> for DeviceError {
    fn from(code: u8) -> Self {
        match code {
            1 => Self::Unknown,
            2 => Self::Authentication,
            3 => Self::Backoff,
            4 => Self::MaxPeers,
            5 => Self::MaxTries,
            6 => Self::Unavailable,
            7 => Self::Busy,
            other => Self::Other(other),
        }
    }
}

#[derive(Debug, Error)]
pub enum DecodeError {
    #[error("TLV8 item at offset {0} is truncated")]
    Truncated(usize),
}

/// Ordered builder for a TLV8 message.
#[derive(Debug, Default, Clone)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(mut self, ty: Type, value: &[u8]) -> Self {
        let ty = ty as u8;
        if value.is_empty() {
            self.buf.extend_from_slice(&[ty, 0]);
            return self;
        }
        for chunk in value.chunks(255) {
            self.buf.push(ty);
            self.buf.push(chunk.len() as u8);
            self.buf.extend_from_slice(chunk);
        }
        self
    }

    pub fn add_u8(self, ty: Type, value: u8) -> Self {
        self.add(ty, &[value])
    }

    pub fn finish(self) -> Vec<u8> {
        self.buf
    }
}

/// A decoded TLV8 message. Fragments of the same type are merged.
#[derive(Debug, Default, Clone)]
pub struct Message {
    items: BTreeMap<u8, Vec<u8>>,
}

impl Message {
    pub fn decode(data: &[u8]) -> Result<Self, DecodeError> {
        let mut items: BTreeMap<u8, Vec<u8>> = BTreeMap::new();
        let mut last_type: Option<u8> = None;
        let mut i = 0;
        while i < data.len() {
            if i + 2 > data.len() {
                return Err(DecodeError::Truncated(i));
            }
            let ty = data[i];
            let len = data[i + 1] as usize;
            let start = i + 2;
            let end = start + len;
            if end > data.len() {
                return Err(DecodeError::Truncated(i));
            }
            let value = &data[start..end];
            // Adjacent items of the same type are fragments of one value.
            if last_type == Some(ty) {
                items.entry(ty).or_default().extend_from_slice(value);
            } else {
                items.insert(ty, value.to_vec());
            }
            last_type = Some(ty);
            i = end;
        }
        Ok(Self { items })
    }

    pub fn get(&self, ty: Type) -> Option<&[u8]> {
        self.items.get(&(ty as u8)).map(Vec::as_slice)
    }

    pub fn get_u8(&self, ty: Type) -> Option<u8> {
        self.get(ty).and_then(|v| v.first().copied())
    }

    /// Returns the device-reported error, if the message carries one.
    pub fn error(&self) -> Option<DeviceError> {
        self.get_u8(Type::Error).map(DeviceError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_short_values() {
        let data =
            Writer::new().add_u8(Type::State, 1).add_u8(Type::Method, 0).add(Type::Identifier, b"airpipe").finish();
        assert_eq!(&data[..6], &[0x06, 1, 1, 0x00, 1, 0]);
        let msg = Message::decode(&data).unwrap();
        assert_eq!(msg.get_u8(Type::State), Some(1));
        assert_eq!(msg.get(Type::Identifier), Some(&b"airpipe"[..]));
        assert_eq!(msg.error(), None);
    }

    #[test]
    fn long_values_are_fragmented_and_merged() {
        let key: Vec<u8> = (0..384u32).map(|i| i as u8).collect();
        let data = Writer::new().add(Type::PublicKey, &key).add_u8(Type::State, 3).finish();
        // 255-byte fragment + 129-byte fragment + state item
        assert_eq!(data.len(), 2 + 255 + 2 + 129 + 3);
        assert_eq!(data[1], 255);
        assert_eq!(data[2 + 255 + 1], 129);
        let msg = Message::decode(&data).unwrap();
        assert_eq!(msg.get(Type::PublicKey), Some(&key[..]));
        assert_eq!(msg.get_u8(Type::State), Some(3));
    }

    #[test]
    fn device_error_is_reported() {
        let msg = Message::decode(&[0x06, 1, 4, 0x07, 1, 2]).unwrap();
        assert_eq!(msg.error(), Some(DeviceError::Authentication));
    }

    #[test]
    fn truncated_input_is_rejected() {
        assert!(Message::decode(&[0x06, 2, 1]).is_err());
        assert!(Message::decode(&[0x06]).is_err());
    }
}
