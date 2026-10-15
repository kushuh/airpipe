//! Wire formats and parsers for untrusted input. Everything here is pure (no
//! I/O, no crypto) and unit tested.

pub mod alac;
pub mod dmap;
pub mod ntp;
pub mod plist;
pub mod rtp;
pub mod rtsp;
pub mod tlv8;

/// Replaces control characters (terminal escapes) with U+FFFD, keeping
/// newlines and tabs. Use it on multi-line text from the network that may be
/// printed to a terminal (e.g. the `/info` dump).
pub fn printable(s: &str) -> String {
    s.chars().map(|c| if c.is_control() && c != '\n' && c != '\t' { '\u{FFFD}' } else { c }).collect()
}

/// Like [`printable`], for single-line fields (speaker names, models): also
/// replaces newlines and bidirectional-override characters, which could
/// fake extra rows or reorder text in `airpipe list`.
pub fn printable_line(s: &str) -> String {
    let deceptive = |c: char| matches!(c, '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}');
    s.chars().map(|c| if c.is_control() || deceptive(c) { '\u{FFFD}' } else { c }).collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn network_text_is_made_printable() {
        assert_eq!(super::printable("Kitchen\u{1b}[2J"), "Kitchen\u{FFFD}[2J");
        assert_eq!(super::printable("a\u{7}b\nc"), "a\u{FFFD}b\nc");
        assert_eq!(super::printable("Salle de bain"), "Salle de bain");
        assert_eq!(super::printable_line("Kitchen\nFake row"), "Kitchen\u{FFFD}Fake row");
        assert_eq!(super::printable_line("abc\u{202E}fed"), "abc\u{FFFD}fed");
        assert_eq!(super::printable_line("Salle de bain"), "Salle de bain");
    }
}
