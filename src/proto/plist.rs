//! Safe parsing of plists received from the network.
//!
//! Plists from speakers (or from anything answering at a speaker's address,
//! even before pairing) are untrusted. Two classic attacks apply:
//!
//! * **Deep nesting**: `plist::from_bytes` builds values recursively (and
//!   dropping a value recurses too), so a few hundred KB of
//!   `<array><array>...` overflows the stack and aborts the process.
//! * **Shared references** ("billion laughs"): a binary plist can reference
//!   the same object many times; the parser expands every reference, so a
//!   16 KB reply can become millions of values and gigabytes of memory.
//!
//! [`parse`] therefore walks the document first with the crate's iterative
//! event reader, which expands references exactly as `from_bytes` would but
//! without recursion, and stops as soon as the nesting, the number of
//! values, or the amount of text exceeds small limits. Only input that
//! passes is then turned into a value.

use std::io::Cursor;

use ::plist::Value;
use ::plist::stream::{Event, Reader};
use thiserror::Error;

/// Real replies (`/info`, SETUP, events) are a few KB.
pub const MAX_SIZE: usize = 64 * 1024;
/// Real replies nest three or four levels deep.
pub const MAX_DEPTH: usize = 32;
/// Values after expanding references; real replies have a few hundred.
pub const MAX_VALUES: usize = 20_000;
/// Total bytes of strings and data after expanding references.
pub const MAX_PAYLOAD: usize = 1024 * 1024;

#[derive(Debug, Error)]
pub enum PlistError {
    #[error("plist is larger than {MAX_SIZE} bytes")]
    TooLarge,
    #[error("plist is nested deeper than {MAX_DEPTH} levels")]
    TooDeep,
    #[error("plist expands to too many values or too much data")]
    TooExpanded,
    #[error("invalid plist: {0}")]
    Invalid(#[from] ::plist::Error),
}

/// Parses a binary or XML plist from an untrusted source.
pub fn parse(bytes: &[u8]) -> Result<Value, PlistError> {
    if bytes.len() > MAX_SIZE {
        return Err(PlistError::TooLarge);
    }
    let (mut depth, mut values, mut payload) = (0usize, 0usize, 0usize);
    for event in Reader::new(Cursor::new(bytes)) {
        values += 1;
        match event? {
            Event::StartArray(_) | Event::StartDictionary(_) => {
                depth += 1;
                if depth > MAX_DEPTH {
                    return Err(PlistError::TooDeep);
                }
            }
            Event::EndCollection => depth = depth.saturating_sub(1),
            Event::String(s) => payload += s.len(),
            Event::Data(d) => payload += d.len(),
            _ => {}
        }
        if values > MAX_VALUES || payload > MAX_PAYLOAD {
            return Err(PlistError::TooExpanded);
        }
    }
    // Vetted: shallow and small, so building (and later dropping) is safe.
    Ok(::plist::from_bytes(bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nested_xml(levels: usize) -> String {
        format!(
            "<?xml version=\"1.0\"?><plist version=\"1.0\">{}{}</plist>",
            "<array>".repeat(levels),
            "</array>".repeat(levels)
        )
    }

    /// Binary plist builder for tests: `objects[i]` lists the indexes object
    /// `i` (an array) refers to; an empty list is an empty array. Object 0 is
    /// the root. Offsets and references are 2 bytes.
    fn binary_arrays(objects: &[Vec<u16>]) -> Vec<u8> {
        let mut out = b"bplist00".to_vec();
        let mut offsets = Vec::new();
        for refs in objects {
            offsets.push(out.len() as u16);
            if refs.len() < 15 {
                out.push(0xA0 | refs.len() as u8);
            } else {
                out.push(0xAF); // length follows as an int object
                out.push(0x11); // 2-byte int
                out.extend_from_slice(&(refs.len() as u16).to_be_bytes());
            }
            for r in refs {
                out.extend_from_slice(&r.to_be_bytes());
            }
        }
        let table = out.len() as u64;
        for o in offsets {
            out.extend_from_slice(&o.to_be_bytes());
        }
        out.extend_from_slice(&[0; 6]);
        out.push(2); // offset size
        out.push(2); // object reference size
        out.extend_from_slice(&(objects.len() as u64).to_be_bytes());
        out.extend_from_slice(&0u64.to_be_bytes()); // root object
        out.extend_from_slice(&table.to_be_bytes());
        out
    }

    #[test]
    fn parses_normal_plists() {
        let mut dict = ::plist::Dictionary::new();
        dict.insert("eventPort".into(), 7000u64.into());
        let mut out = Vec::new();
        ::plist::to_writer_binary(&mut out, &Value::Dictionary(dict)).unwrap();
        let value = parse(&out).unwrap();
        assert_eq!(value.as_dictionary().unwrap().get("eventPort").unwrap().as_unsigned_integer(), Some(7000));
        assert!(parse(&binary_arrays(&[vec![1, 1], vec![]])).is_ok(), "the test builder makes valid plists");
    }

    #[test]
    fn deep_nesting_is_rejected_without_crashing() {
        // The deepest XML that fits the size limit (~4 600 levels).
        let levels = (MAX_SIZE - 64) / ("<array>".len() + "</array>".len());
        assert!(matches!(parse(nested_xml(levels).as_bytes()), Err(PlistError::TooDeep)));
        assert!(matches!(parse(nested_xml(MAX_DEPTH + 1).as_bytes()), Err(PlistError::TooDeep)));
        assert!(parse(nested_xml(MAX_DEPTH).as_bytes()).is_ok());
        // The densest binary nesting (~13 000 levels, a chain of one-element arrays).
        let n = (MAX_SIZE - 64) / 5;
        let chain: Vec<Vec<u16>> = (0..n).map(|i| vec![(i + 1) as u16]).chain([vec![]]).collect();
        assert!(matches!(parse(&binary_arrays(&chain)), Err(PlistError::TooDeep)));
    }

    #[test]
    fn shared_references_cannot_amplify() {
        // Root -> 4000 x A, A -> 4000 x B: 16 KB of input, 16 million values
        // once expanded (1.3 GB with plain from_bytes).
        let n = 4000;
        let objects = vec![vec![1u16; n], vec![2u16; n], vec![]];
        let input = binary_arrays(&objects);
        assert!(input.len() < MAX_SIZE);
        let start = std::time::Instant::now();
        assert!(matches!(parse(&input), Err(PlistError::TooExpanded)));
        assert!(start.elapsed() < std::time::Duration::from_secs(1), "rejected quickly");
    }

    #[test]
    fn rejects_oversized_input() {
        assert!(matches!(parse(&vec![b' '; MAX_SIZE + 1]), Err(PlistError::TooLarge)));
        assert!(matches!(parse(nested_xml(20_000).as_bytes()), Err(PlistError::TooLarge)));
    }
}
