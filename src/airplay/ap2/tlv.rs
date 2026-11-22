//! TLV8, the format of the pairing messages: a type byte, a length byte and
//! up to 255 bytes of value. A longer value goes as several items of the
//! same type, each full one but the last, and is joined again when read.

use std::fmt;

/// The item types of the pairing messages.
pub const METHOD: u8 = 0;
pub const IDENTIFIER: u8 = 1;
pub const SALT: u8 = 2;
pub const PUBLIC_KEY: u8 = 3;
pub const PROOF: u8 = 4;
pub const ENCRYPTED_DATA: u8 = 5;
pub const STATE: u8 = 6;
pub const ERROR: u8 = 7;
pub const SIGNATURE: u8 = 10;
pub const FLAGS: u8 = 19;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlvError(pub String);

impl fmt::Display for TlvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

pub fn encode(items: &[(u8, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for &(kind, value) in items {
        if value.is_empty() {
            out.extend_from_slice(&[kind, 0]);
        }
        for part in value.chunks(255) {
            out.extend_from_slice(&[kind, part.len() as u8]);
            out.extend_from_slice(part);
        }
    }
    out
}

/// The items of `bytes`, an item that follows a full one of the same type
/// joined to it.
pub fn decode(bytes: &[u8]) -> Result<Vec<(u8, Vec<u8>)>, TlvError> {
    let mut items: Vec<(u8, Vec<u8>)> = Vec::new();
    let mut rest = bytes;
    let mut last_full = false;
    while !rest.is_empty() {
        let [kind, len, ..] = *rest else {
            return Err(TlvError("an item cut after its type".into()));
        };
        let len = usize::from(len);
        let value = rest
            .get(2..2 + len)
            .ok_or_else(|| TlvError(format!("item {kind} cut short of {len} bytes")))?;
        match items.last_mut() {
            Some((k, v)) if last_full && *k == kind => v.extend_from_slice(value),
            _ => items.push((kind, value.to_vec())),
        }
        last_full = len == 255;
        rest = &rest[2 + len..];
    }
    Ok(items)
}

/// The value of the first item of type `kind`.
pub fn find(items: &[(u8, Vec<u8>)], kind: u8) -> Option<&[u8]> {
    items
        .iter()
        .find(|(t, _)| *t == kind)
        .map(|(_, v)| v.as_slice())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_items_are_type_length_value() {
        assert_eq!(
            encode(&[(STATE, &[1]), (METHOD, &[0]), (FLAGS, &[0x10])]),
            [6, 1, 1, 0, 1, 0, 19, 1, 0x10]
        );
        assert_eq!(encode(&[(SALT, &[])]), [2, 0]);
    }

    #[test]
    fn values_over_255_are_split_and_joined_again() {
        let long: Vec<u8> = (0..600u32).map(|i| i as u8).collect();
        let bytes = encode(&[(PUBLIC_KEY, &long), (PROOF, &[9])]);
        // 255 + 255 + 90, each with its two bytes, then the proof.
        assert_eq!(bytes.len(), 600 + 3 * 2 + 3);
        assert_eq!(&bytes[..2], &[3, 255]);
        assert_eq!(&bytes[257..259], &[3, 255]);
        assert_eq!(&bytes[514..516], &[3, 90]);
        let items = decode(&bytes).unwrap();
        assert_eq!(items, vec![(PUBLIC_KEY, long.clone()), (PROOF, vec![9])]);
        // A value of exactly 255 bytes is one item, and the next item of the
        // same type after it is not joined to it when a different one sits
        // between them.
        let full = vec![7u8; 255];
        let items = decode(&encode(&[(SALT, &full), (STATE, &[2]), (SALT, &[1])])).unwrap();
        assert_eq!(items, vec![(SALT, full), (STATE, vec![2]), (SALT, vec![1])]);
        // Two short items of a type in a row stay two.
        assert_eq!(
            decode(&[6, 1, 1, 6, 1, 2]).unwrap(),
            vec![(STATE, vec![1]), (STATE, vec![2])]
        );
    }

    #[test]
    fn a_cut_item_is_an_error() {
        assert!(decode(&[6, 2, 1]).is_err());
        assert!(decode(&[6]).is_err());
        assert_eq!(decode(&[]).unwrap(), vec![]);
    }

    #[test]
    fn find_gives_the_first_of_a_type() {
        let items = decode(&[6, 1, 2, 7, 1, 2]).unwrap();
        assert_eq!(find(&items, ERROR), Some(&[2u8][..]));
        assert_eq!(find(&items, PROOF), None);
    }
}
