//! Every random value airpipe generates comes from here, read directly from
//! the operating system's CSPRNG (`getrandom(2)`).

/// Random bytes for keys, nonces and secrets.
///
/// Panics only if the kernel cannot provide randomness at all, in which
/// case no key could be generated safely anyway.
pub fn bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("the OS random number generator is unavailable");
    b
}

/// A random UUID (version 4), uppercase as Apple devices send them.
pub fn uuid() -> String {
    let mut b: [u8; 16] = bytes();
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // RFC 4122 variant
    let h = hex::encode_upper(b);
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

/// A random MAC-style identifier ("02:1A:..."), locally administered and
/// unicast, so it can never collide with a real network card.
pub fn mac_address() -> String {
    let mut b: [u8; 6] = bytes();
    b[0] = (b[0] & 0xfe) | 0x02;
    b.iter().map(|x| format!("{x:02X}")).collect::<Vec<_>>().join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_format() {
        let u = uuid();
        assert_eq!(u.len(), 36);
        assert_eq!(u.as_bytes()[14], b'4', "version nibble: {u}");
        assert!(matches!(u.as_bytes()[19], b'8' | b'9' | b'A' | b'B'), "variant: {u}");
        assert_ne!(uuid(), u);
    }

    #[test]
    fn mac_is_local_unicast() {
        let m = mac_address();
        assert_eq!(m.len(), 17);
        let first = u8::from_str_radix(&m[..2], 16).unwrap();
        assert_eq!(first & 0x03, 0x02);
    }
}
