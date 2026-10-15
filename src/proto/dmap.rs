//! DMAP tagged encoding, used to send "now playing" text to speakers
//! (`SET_PARAMETER` with `Content-Type: application/x-dmap-tagged`).
//!
//! Every item is `tag (4 ASCII bytes) | length (u32 BE) | value`. Track
//! metadata is a `mlit` (listing item) container holding string items.

/// Text metadata for the currently playing track.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrackInfo {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub genre: Option<String>,
}

fn item(out: &mut Vec<u8>, tag: &[u8; 4], value: &[u8]) {
    out.extend_from_slice(tag);
    out.extend_from_slice(&(value.len() as u32).to_be_bytes());
    out.extend_from_slice(value);
}

/// Encodes track text as a DMAP `mlit` container.
pub fn encode_track(info: &TrackInfo) -> Vec<u8> {
    let mut inner = Vec::new();
    let fields: [(&[u8; 4], &Option<String>); 4] =
        [(b"minm", &info.title), (b"asar", &info.artist), (b"asal", &info.album), (b"asgn", &info.genre)];
    for (tag, value) in fields {
        if let Some(v) = value {
            item(&mut inner, tag, v.as_bytes());
        }
    }
    let mut out = Vec::with_capacity(inner.len() + 8);
    item(&mut out, b"mlit", &inner);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_container_with_present_fields_only() {
        let info = TrackInfo { title: Some("Song".into()), artist: Some("Me".into()), ..Default::default() };
        let out = encode_track(&info);
        let mut expected = Vec::new();
        expected.extend_from_slice(b"mlit");
        expected.extend_from_slice(&(8 + 4 + 8 + 2u32).to_be_bytes());
        expected.extend_from_slice(b"minm\0\0\0\x04Song");
        expected.extend_from_slice(b"asar\0\0\0\x02Me");
        assert_eq!(out, expected);
    }

    #[test]
    fn empty_track_is_empty_container() {
        assert_eq!(encode_track(&TrackInfo::default()), b"mlit\0\0\0\0".to_vec());
    }
}
