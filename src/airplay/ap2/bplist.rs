//! Binary property lists (`bplist00`), as far as AirPlay 2's session
//! messages use them: dictionaries, arrays, strings, integers, booleans,
//! data and reals. Written without sharing equal objects; read with any
//! sharing, and with a limit on depth so a crafted answer cannot recurse
//! without end.

use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Dict(Vec<(String, Value)>),
    Array(Vec<Value>),
    String(String),
    Int(i64),
    Real(f64),
    Bool(bool),
    Data(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlistError(pub String);

impl fmt::Display for PlistError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// How deep containers may nest in what is read.
const DEPTH: usize = 16;

impl Value {
    /// The value under `key` of a dictionary.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Dict(items) => items.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(items) => Some(items),
            _ => None,
        }
    }
}

/// An integer object: one, two, four or eight bytes, big-endian.
fn int(n: i64, out: &mut Vec<u8>) {
    if (0..=0xff).contains(&n) {
        out.extend_from_slice(&[0x10, n as u8]);
    } else if (0..=0xffff).contains(&n) {
        out.push(0x11);
        out.extend_from_slice(&(n as u16).to_be_bytes());
    } else if (0..=0xffff_ffff).contains(&n) {
        out.push(0x12);
        out.extend_from_slice(&(n as u32).to_be_bytes());
    } else {
        out.push(0x13);
        out.extend_from_slice(&n.to_be_bytes());
    }
}

/// A marker with a length: in its low four bits, or after it as an
/// integer from 15 on.
fn marker(kind: u8, len: usize, out: &mut Vec<u8>) {
    if len < 15 {
        out.push(kind | len as u8);
    } else {
        out.push(kind | 0x0f);
        int(len as i64, out);
    }
}

fn width(max: usize) -> usize {
    match max {
        0..=0xff => 1,
        0x100..=0xffff => 2,
        0x1_0000..=0xffff_ffff => 4,
        _ => 8,
    }
}

fn put(n: usize, width: usize, out: &mut Vec<u8>) {
    out.extend_from_slice(&(n as u64).to_be_bytes()[8 - width..]);
}

pub fn encode(value: &Value) -> Vec<u8> {
    // Number the objects: a container first, then (for a dictionary) its
    // keys, then its values, each in full before the next.
    let mut objects: Vec<Object> = Vec::new();
    number(value, &mut objects);
    let refs = width(objects.len());
    let mut out = b"bplist00".to_vec();
    let mut offsets = Vec::with_capacity(objects.len());
    for object in &objects {
        offsets.push(out.len());
        match object {
            Object::Key(k) => string(k, &mut out),
            Object::Value(Value::String(k)) => string(k, &mut out),
            Object::Value(Value::Int(n)) => int(*n, &mut out),
            Object::Value(Value::Real(r)) => {
                out.push(0x23);
                out.extend_from_slice(&r.to_be_bytes());
            }
            Object::Value(Value::Bool(b)) => out.push(if *b { 0x09 } else { 0x08 }),
            Object::Value(Value::Data(d)) => {
                marker(0x40, d.len(), &mut out);
                out.extend_from_slice(d);
            }
            Object::Array(items) => {
                marker(0xa0, items.len(), &mut out);
                for &i in items {
                    put(i, refs, &mut out);
                }
            }
            Object::Dict(keys, values) => {
                marker(0xd0, keys.len(), &mut out);
                for &i in keys.iter().chain(values) {
                    put(i, refs, &mut out);
                }
            }
            Object::Value(_) => unreachable!("containers are numbered as such"),
        }
    }
    let table = out.len();
    let offset_width = width(table);
    for &o in &offsets {
        put(o, offset_width, &mut out);
    }
    out.extend_from_slice(&[0; 6]);
    out.extend_from_slice(&[offset_width as u8, refs as u8]);
    out.extend_from_slice(&(objects.len() as u64).to_be_bytes());
    out.extend_from_slice(&0u64.to_be_bytes());
    out.extend_from_slice(&(table as u64).to_be_bytes());
    out
}

enum Object<'a> {
    Key(&'a str),
    Value(&'a Value),
    Array(Vec<usize>),
    Dict(Vec<usize>, Vec<usize>),
}

/// Numbers `value` and what it holds; gives its number.
fn number<'a>(value: &'a Value, objects: &mut Vec<Object<'a>>) -> usize {
    let at = objects.len();
    match value {
        Value::Array(items) => {
            objects.push(Object::Array(Vec::new()));
            let refs = items.iter().map(|v| number(v, objects)).collect();
            objects[at] = Object::Array(refs);
        }
        Value::Dict(items) => {
            objects.push(Object::Dict(Vec::new(), Vec::new()));
            let keys = items
                .iter()
                .map(|(k, _)| {
                    objects.push(Object::Key(k));
                    objects.len() - 1
                })
                .collect();
            let values = items.iter().map(|(_, v)| number(v, objects)).collect();
            objects[at] = Object::Dict(keys, values);
        }
        other => objects.push(Object::Value(other)),
    }
    at
}

/// ASCII as it is, anything else as UTF-16 big-endian.
fn string(s: &str, out: &mut Vec<u8>) {
    if s.is_ascii() {
        marker(0x50, s.len(), out);
        out.extend_from_slice(s.as_bytes());
    } else {
        let units: Vec<u16> = s.encode_utf16().collect();
        marker(0x60, units.len(), out);
        for u in units {
            out.extend_from_slice(&u.to_be_bytes());
        }
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    offsets: Vec<usize>,
    refs: usize,
    /// Values that may still be read: shared objects are read once per
    /// reference, so a few small arrays could otherwise stand for a tree
    /// of billions.
    budget: std::cell::Cell<usize>,
}

fn broken(why: impl Into<String>) -> PlistError {
    PlistError(why.into())
}

fn uint(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0u64, |n, &b| n << 8 | u64::from(b))
}

pub fn decode(bytes: &[u8]) -> Result<Value, PlistError> {
    if bytes.len() < 8 + 32 || &bytes[..8] != b"bplist00" {
        return Err(broken("not a binary plist"));
    }
    let trailer = &bytes[bytes.len() - 32..];
    let offset_width = usize::from(trailer[6]);
    let refs = usize::from(trailer[7]);
    let count = uint(&trailer[8..16]);
    let top = uint(&trailer[16..24]);
    let table = uint(&trailer[24..32]);
    if !(1..=8).contains(&offset_width) || !(1..=8).contains(&refs) {
        return Err(broken("a plist with odd widths"));
    }
    let end = bytes.len() - 32;
    let table_len = count
        .checked_mul(offset_width as u64)
        .ok_or_else(|| broken("too many objects"))?;
    if table < 8 || table.checked_add(table_len).is_none_or(|e| e > end as u64) {
        return Err(broken("an offset table outside the plist"));
    }
    let table = table as usize;
    let offsets = (0..count as usize)
        .map(|i| uint(&bytes[table + i * offset_width..table + (i + 1) * offset_width]) as usize)
        .collect::<Vec<_>>();
    if offsets.iter().any(|&o| o < 8 || o >= table) {
        return Err(broken("an object outside the plist"));
    }
    let budget = std::cell::Cell::new((offsets.len() * 4).max(4096));
    let reader = Reader {
        bytes: &bytes[..table],
        offsets,
        refs,
        budget,
    };
    reader.object(top, 0)
}

impl Reader<'_> {
    fn take(&self, at: usize, n: usize) -> Result<&[u8], PlistError> {
        self.bytes
            .get(
                at..at
                    .checked_add(n)
                    .ok_or_else(|| broken("a length beyond reason"))?,
            )
            .ok_or_else(|| broken("an object cut short"))
    }

    /// The length in a marker's low bits, or the integer after it; gives
    /// the length and where the content starts.
    fn length(&self, at: usize) -> Result<(usize, usize), PlistError> {
        let low = self.take(at, 1)?[0] & 0x0f;
        if low < 15 {
            return Ok((usize::from(low), at + 1));
        }
        let m = self.take(at + 1, 1)?[0];
        if m & 0xf0 != 0x10 || m & 0x0f > 3 {
            return Err(broken("a length that is not an integer"));
        }
        let n = 1usize << (m & 0x0f);
        Ok((uint(self.take(at + 2, n)?) as usize, at + 2 + n))
    }

    fn refs(&self, at: usize, n: usize) -> Result<Vec<u64>, PlistError> {
        let bytes = self.take(
            at,
            n.checked_mul(self.refs).ok_or_else(|| broken("too many"))?,
        )?;
        Ok(bytes.chunks(self.refs).map(uint).collect())
    }

    fn object(&self, index: u64, depth: usize) -> Result<Value, PlistError> {
        if depth > DEPTH {
            return Err(broken("containers nested too deep"));
        }
        let left = self.budget.get();
        if left == 0 {
            return Err(broken("more values than the plist holds objects for"));
        }
        self.budget.set(left - 1);
        let at = *self
            .offsets
            .get(usize::try_from(index).unwrap_or(usize::MAX))
            .ok_or_else(|| {
                broken(format!(
                    "a reference to object {index} of {}",
                    self.offsets.len()
                ))
            })?;
        let m = self.take(at, 1)?[0];
        match m >> 4 {
            0x0 => match m {
                0x08 => Ok(Value::Bool(false)),
                0x09 => Ok(Value::Bool(true)),
                _ => Err(broken(format!("marker {m:#04x}"))),
            },
            0x1 => {
                let n = 1usize << (m & 0x0f);
                let b = self.take(at + 1, n)?;
                match n {
                    1 | 2 | 4 => Ok(Value::Int(uint(b) as i64)),
                    8 => Ok(Value::Int(uint(b) as i64)),
                    _ => Err(broken(format!("an integer of {n} bytes"))),
                }
            }
            0x2 => match m & 0x0f {
                2 => Ok(Value::Real(f64::from(f32::from_be_bytes(
                    self.take(at + 1, 4)?.try_into().expect("four bytes"),
                )))),
                3 => Ok(Value::Real(f64::from_be_bytes(
                    self.take(at + 1, 8)?.try_into().expect("eight bytes"),
                ))),
                _ => Err(broken(format!("marker {m:#04x}"))),
            },
            0x4 => {
                let (n, start) = self.length(at)?;
                Ok(Value::Data(self.take(start, n)?.to_vec()))
            }
            0x5 => {
                let (n, start) = self.length(at)?;
                let b = self.take(start, n)?;
                Ok(Value::String(String::from_utf8_lossy(b).into_owned()))
            }
            0x6 => {
                let (n, start) = self.length(at)?;
                let b = self.take(start, n.checked_mul(2).ok_or_else(|| broken("too long"))?)?;
                let units: Vec<u16> = b
                    .chunks(2)
                    .map(|c| u16::from_be_bytes([c[0], c[1]]))
                    .collect();
                Ok(Value::String(String::from_utf16_lossy(&units)))
            }
            0xa => {
                let (n, start) = self.length(at)?;
                let items = self
                    .refs(start, n)?
                    .into_iter()
                    .map(|r| self.object(r, depth + 1))
                    .collect::<Result<_, _>>()?;
                Ok(Value::Array(items))
            }
            0xd => {
                let (n, start) = self.length(at)?;
                let refs = self.refs(start, n.checked_mul(2).ok_or_else(|| broken("too many"))?)?;
                let (keys, values) = refs.split_at(n);
                let mut items = Vec::with_capacity(n);
                for (&k, &v) in keys.iter().zip(values) {
                    let Value::String(key) = self.object(k, depth + 1)? else {
                        return Err(broken("a dictionary key that is not a string"));
                    };
                    items.push((key, self.object(v, depth + 1)?));
                }
                Ok(Value::Dict(items))
            }
            _ => Err(broken(format!("marker {m:#04x}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    fn dict(items: &[(&str, Value)]) -> Value {
        Value::Dict(
            items
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        )
    }

    /// Made by Python's `plistlib`, which shares equal objects.
    const PLISTLIB: &str = "62706c6973743030db0102030405060708090a0b0c0d0e18191a1b1c1d1e1f596576656e74506f72745a74696d696e67506f72745773747265616d7353626967536e65675166526f6b526e6f54626c6f62546e616d65546c6f6e6711c3cb1000a10fd410111213141516175864617461506f72745b636f6e74726f6c506f727454747970655f100f617564696f42756666657253697a651117701117711060120080000013000001000000000013ffffffffffffffff233ff8000000000000090842010265004b00fc0063006800655f10147878787878787878787878787878787878787878081f29343c404446494c51565b5e60626b748085979a9d9fa4adb6bfc0c1c4cf00000000000001010000000000000020000000000000000000000000000000e6";

    fn expected() -> Value {
        dict(&[
            ("eventPort", Value::Int(50123)),
            ("timingPort", Value::Int(0)),
            (
                "streams",
                Value::Array(vec![dict(&[
                    ("dataPort", Value::Int(6000)),
                    ("controlPort", Value::Int(6001)),
                    ("type", Value::Int(96)),
                    ("audioBufferSize", Value::Int(8_388_608)),
                ])]),
            ),
            ("big", Value::Int(1 << 40)),
            ("neg", Value::Int(-1)),
            ("f", Value::Real(1.5)),
            ("ok", Value::Bool(true)),
            ("no", Value::Bool(false)),
            ("blob", Value::Data(vec![1, 2])),
            ("name", Value::String("Küche".into())),
            ("long", Value::String("x".repeat(20))),
        ])
    }

    #[test]
    fn what_plistlib_writes_is_read() {
        let v = decode(&unhex(PLISTLIB)).unwrap();
        assert_eq!(v, expected());
        let streams = v.get("streams").and_then(Value::as_array).unwrap();
        assert_eq!(
            streams[0].get("dataPort").and_then(Value::as_int),
            Some(6000)
        );
        assert_eq!(v.get("missing"), None);
    }

    #[test]
    fn what_is_written_reads_back() {
        let v = expected();
        let bytes = encode(&v);
        assert_eq!(&bytes[..8], b"bplist00");
        assert_eq!(decode(&bytes).unwrap(), v);
        // Over 255 objects takes two-byte references; long data and an
        // array beyond 14 items take their length as an integer.
        let many = Value::Array((0..300).map(Value::Int).collect());
        let v = dict(&[("many", many), ("data", Value::Data(vec![7; 1000]))]);
        assert_eq!(decode(&encode(&v)).unwrap(), v);
    }

    #[test]
    fn a_session_setup_is_written_as_plistlib_would() {
        // plistlib.dumps({"timingProtocol": "NTP", "timingPort": 7}, fmt=FMT_BINARY,
        // sort_keys=False): with nothing equal to share, the same bytes.
        let v = dict(&[
            ("timingProtocol", Value::String("NTP".into())),
            ("timingPort", Value::Int(7)),
        ]);
        assert_eq!(
            encode(&v),
            unhex(
                "62706c6973743030d2010203045e74696d696e6750726f746f636f6c5a74696d696e67506f7274534e5450\
                 1007080d1c272b000000000000010100000000000000050000000000000000000000000000002d"
            )
        );
    }

    #[test]
    fn shared_references_cannot_blow_up_into_a_huge_tree() {
        // Three arrays of 1000 references each to the next, then one
        // integer: a few kilobytes that would read as 10^9 values.
        let mut b = b"bplist00".to_vec();
        let mut offsets = Vec::new();
        for next in 1u16..=3 {
            offsets.push(b.len());
            b.extend_from_slice(&[0xaf, 0x11, 0x03, 0xe8]);
            for _ in 0..1000 {
                b.extend_from_slice(&next.to_be_bytes());
            }
        }
        offsets.push(b.len());
        b.extend_from_slice(&[0x10, 0]);
        let table = b.len();
        for o in &offsets {
            b.extend_from_slice(&(*o as u16).to_be_bytes());
        }
        b.extend_from_slice(&[0; 6]);
        b.extend_from_slice(&[2, 2]);
        b.extend_from_slice(&4u64.to_be_bytes());
        b.extend_from_slice(&0u64.to_be_bytes());
        b.extend_from_slice(&(table as u64).to_be_bytes());
        let started = std::time::Instant::now();
        assert!(decode(&b).is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        // One level of it, 1000 values from shared objects, still reads.
        let v = Value::Array(vec![Value::Int(0); 1000]);
        assert_eq!(decode(&encode(&v)).unwrap(), v);
    }

    #[test]
    fn broken_plists_are_errors() {
        let good = encode(&expected());
        assert!(decode(b"bplist0").is_err());
        assert!(decode(&good[1..]).is_err());
        let mut cut = good.clone();
        cut.truncate(good.len() - 1);
        assert!(decode(&cut).is_err());
        // An array that holds itself.
        let mut looped = unhex(
            "62706c6973743030a10008000000000000010100000000000000010000000000000000000000000000000a",
        );
        assert!(decode(&looped).is_err());
        // A reference beyond the objects.
        looped[9] = 5;
        assert!(decode(&looped).is_err());
    }
}
