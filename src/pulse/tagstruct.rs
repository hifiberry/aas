//! PulseAudio's tagstruct: the typed payload inside a pstream packet.

pub const STRING: u8 = b't';
pub const STRING_NULL: u8 = b'N';
pub const U32: u8 = b'L';
pub const U8: u8 = b'B';
pub const U64: u8 = b'R';
pub const S64: u8 = b'r';
pub const SAMPLE_SPEC: u8 = b'a';
pub const ARBITRARY: u8 = b'x';
pub const BOOLEAN_TRUE: u8 = b'1';
pub const BOOLEAN_FALSE: u8 = b'0';
pub const TIMEVAL: u8 = b'T';
pub const USEC: u8 = b'U';
pub const CHANNEL_MAP: u8 = b'm';
pub const CVOLUME: u8 = b'v';
pub const VOLUME: u8 = b'V';
pub const PROPLIST: u8 = b'P';
pub const FORMAT_INFO: u8 = b'f';

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SampleSpec {
    pub format: u8,
    pub channels: u8,
    pub rate: u32,
}

#[derive(Default)]
pub struct Builder {
    out: Vec<u8>,
}

impl Builder {
    pub fn new() -> Builder {
        Builder::default()
    }

    pub fn u8(&mut self, value: u8) {
        self.out.extend_from_slice(&[U8, value]);
    }

    pub fn u32(&mut self, value: u32) {
        self.out.push(U32);
        self.out.extend_from_slice(&value.to_be_bytes());
    }

    pub fn u64(&mut self, value: u64) {
        self.out.push(U64);
        self.out.extend_from_slice(&value.to_be_bytes());
    }

    pub fn i64(&mut self, value: i64) {
        self.out.push(S64);
        self.out.extend_from_slice(&value.to_be_bytes());
    }

    pub fn boolean(&mut self, value: bool) {
        self.out
            .push(if value { BOOLEAN_TRUE } else { BOOLEAN_FALSE });
    }

    pub fn string(&mut self, value: &str) {
        self.out.push(STRING);
        self.out.extend_from_slice(value.as_bytes());
        self.out.push(0);
    }

    pub fn null_string(&mut self) {
        self.out.push(STRING_NULL);
    }

    pub fn arbitrary(&mut self, data: &[u8]) {
        self.out.push(ARBITRARY);
        self.out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        self.out.extend_from_slice(data);
    }

    pub fn sample_spec(&mut self, spec: SampleSpec) {
        self.out.push(SAMPLE_SPEC);
        self.out.push(spec.format);
        self.out.push(spec.channels);
        self.out.extend_from_slice(&spec.rate.to_be_bytes());
    }

    pub fn channel_map(&mut self, positions: &[u8]) {
        self.out.push(CHANNEL_MAP);
        self.out.push(positions.len() as u8);
        self.out.extend_from_slice(positions);
    }

    pub fn cvolume(&mut self, values: &[u32]) {
        self.out.push(CVOLUME);
        self.out.push(values.len() as u8);
        for value in values {
            self.out.extend_from_slice(&value.to_be_bytes());
        }
    }

    pub fn volume(&mut self, value: u32) {
        self.out.push(VOLUME);
        self.out.extend_from_slice(&value.to_be_bytes());
    }

    pub fn proplist(&mut self, props: &[(String, String)]) {
        self.out.push(PROPLIST);
        for (key, value) in props {
            self.string(key);
            self.u32(value.len() as u32);
            self.arbitrary(value.as_bytes());
        }
        self.null_string();
    }

    pub fn format_info(&mut self, encoding: u8, props: &[(String, String)]) {
        self.out.push(FORMAT_INFO);
        self.u8(encoding);
        self.proplist(props);
    }

    pub fn usec(&mut self, value: u64) {
        self.out.push(USEC);
        self.out.extend_from_slice(&value.to_be_bytes());
    }

    pub fn timeval(&mut self, sec: u32, usec: u32) {
        self.out.push(TIMEVAL);
        self.out.extend_from_slice(&sec.to_be_bytes());
        self.out.extend_from_slice(&usec.to_be_bytes());
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.out
    }
}

pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Reader<'a> {
        Reader { data, pos: 0 }
    }

    pub fn eof(&self) -> bool {
        self.pos >= self.data.len()
    }

    /// How far the reader has read, for a body that follows a header.
    pub fn position(&self) -> usize {
        self.pos
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let out = self.data.get(self.pos..end)?;
        self.pos = end;
        Some(out)
    }

    fn expect(&mut self, tag: u8) -> Option<()> {
        if *self.data.get(self.pos)? != tag {
            return None;
        }
        self.pos += 1;
        Some(())
    }

    pub fn u8(&mut self) -> Option<u8> {
        self.expect(U8)?;
        Some(self.take(1)?[0])
    }

    pub fn u32(&mut self) -> Option<u32> {
        self.expect(U32)?;
        let bytes = self.take(4)?;
        Some(u32::from_be_bytes(bytes.try_into().ok()?))
    }

    pub fn u64(&mut self) -> Option<u64> {
        self.expect(U64)?;
        let bytes = self.take(8)?;
        Some(u64::from_be_bytes(bytes.try_into().ok()?))
    }

    pub fn i64(&mut self) -> Option<i64> {
        self.expect(S64)?;
        let bytes = self.take(8)?;
        Some(i64::from_be_bytes(bytes.try_into().ok()?))
    }

    pub fn boolean(&mut self) -> Option<bool> {
        match *self.data.get(self.pos)? {
            BOOLEAN_TRUE => {
                self.pos += 1;
                Some(true)
            }
            BOOLEAN_FALSE => {
                self.pos += 1;
                Some(false)
            }
            _ => None,
        }
    }

    fn tagged_string(&mut self) -> Option<Option<&'a str>> {
        match *self.data.get(self.pos)? {
            STRING_NULL => {
                self.pos += 1;
                Some(None)
            }
            STRING => {
                self.pos += 1;
                let start = self.pos;
                while *self.data.get(self.pos)? != 0 {
                    self.pos += 1;
                }
                let text = std::str::from_utf8(&self.data[start..self.pos]).ok()?;
                self.pos += 1;
                Some(Some(text))
            }
            _ => None,
        }
    }

    /// A string. A null string reads as the empty one; `proplist` tells
    /// the terminator apart.
    pub fn string(&mut self) -> Option<&'a str> {
        self.tagged_string().flatten()
    }

    pub fn arbitrary(&mut self) -> Option<&'a [u8]> {
        self.expect(ARBITRARY)?;
        let length = {
            let bytes = self.take(4)?;
            u32::from_be_bytes(bytes.try_into().ok()?) as usize
        };
        self.take(length)
    }

    pub fn sample_spec(&mut self) -> Option<SampleSpec> {
        self.expect(SAMPLE_SPEC)?;
        let format = self.take(1)?[0];
        let channels = self.take(1)?[0];
        let rate = u32::from_be_bytes(self.take(4)?.try_into().ok()?);
        Some(SampleSpec {
            format,
            channels,
            rate,
        })
    }

    pub fn channel_map(&mut self) -> Option<Vec<u8>> {
        self.expect(CHANNEL_MAP)?;
        let channels = self.take(1)?[0] as usize;
        Some(self.take(channels)?.to_vec())
    }

    pub fn cvolume(&mut self) -> Option<Vec<u32>> {
        self.expect(CVOLUME)?;
        let channels = self.take(1)?[0] as usize;
        let mut values = Vec::with_capacity(channels);
        for _ in 0..channels {
            values.push(u32::from_be_bytes(self.take(4)?.try_into().ok()?));
        }
        Some(values)
    }

    pub fn proplist(&mut self) -> Option<Vec<(String, String)>> {
        self.expect(PROPLIST)?;
        let mut props = Vec::new();
        loop {
            let Some(key) = self.tagged_string()? else {
                return Some(props);
            };
            let length = self.u32()? as usize;
            let value = self.arbitrary()?;
            if value.len() != length {
                return None;
            }
            props.push((
                key.to_string(),
                String::from_utf8_lossy(value).into_owned(),
            ));
        }
    }

    pub fn format_info(&mut self) -> Option<(u8, Vec<(String, String)>)> {
        self.expect(FORMAT_INFO)?;
        let encoding = self.u8()?;
        let props = self.proplist()?;
        Some((encoding, props))
    }

    pub fn usec(&mut self) -> Option<u64> {
        self.expect(USEC)?;
        Some(u64::from_be_bytes(self.take(8)?.try_into().ok()?))
    }

    pub fn timeval(&mut self) -> Option<(u32, u32)> {
        self.expect(TIMEVAL)?;
        let sec = u32::from_be_bytes(self.take(4)?.try_into().ok()?);
        let usec = u32::from_be_bytes(self.take(4)?.try_into().ok()?);
        Some((sec, usec))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tag_round_trips() {
        let mut b = Builder::new();
        b.string("aas");
        b.u8(3);
        b.u32(48_000);
        b.u64(9);
        b.i64(-2);
        b.boolean(true);
        b.sample_spec(SampleSpec {
            format: 3,
            channels: 2,
            rate: 48_000,
        });
        b.channel_map(&[1, 2]);
        b.cvolume(&[65_536, 32_768]);
        b.proplist(&[("application.name".into(), "mpv".into())]);
        b.usec(12_345);
        b.timeval(7, 8);
        let bytes = b.into_vec();
        let mut r = Reader::new(&bytes);
        assert_eq!(r.string(), Some("aas"));
        assert_eq!(r.u8(), Some(3));
        assert_eq!(r.u32(), Some(48_000));
        assert_eq!(r.u64(), Some(9));
        assert_eq!(r.i64(), Some(-2));
        assert_eq!(r.boolean(), Some(true));
        assert_eq!(
            r.sample_spec(),
            Some(SampleSpec {
                format: 3,
                channels: 2,
                rate: 48_000
            })
        );
        assert_eq!(r.channel_map(), Some(vec![1, 2]));
        assert_eq!(r.cvolume(), Some(vec![65_536, 32_768]));
        assert_eq!(
            r.proplist(),
            Some(vec![("application.name".into(), "mpv".into())])
        );
        assert_eq!(r.usec(), Some(12_345));
        assert_eq!(r.timeval(), Some((7, 8)));
        assert!(r.eof());
    }

    #[test]
    fn a_proplist_pair_carries_its_length_twice() {
        let mut b = Builder::new();
        b.proplist(&[("k".into(), "v".into())]);
        assert_eq!(
            b.into_vec(),
            vec![b'P', b't', b'k', 0, b'L', 0, 0, 0, 1, b'x', 0, 0, 0, 1, b'v', b'N']
        );
    }

    #[test]
    fn a_format_info_encoding_is_tagged() {
        let mut b = Builder::new();
        b.format_info(0, &[]);
        assert_eq!(b.into_vec(), vec![b'f', b'B', 0, b'P', b'N']);
    }

    #[test]
    fn reading_past_the_end_is_none() {
        let mut r = Reader::new(&[b'L', 0, 0]);
        assert_eq!(r.u32(), None);
    }

    #[test]
    fn an_unexpected_tag_is_none() {
        let mut r = Reader::new(&[b'L', 0, 0, 0, 1]);
        assert_eq!(r.u8(), None);
    }
}
