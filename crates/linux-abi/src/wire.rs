//! A minimal binary encoding for handing process state to a forked child
//! (the same `macwl-run` binary, so no versioning): little-endian integers
//! and length-prefixed byte strings, read back in the order written.

/// Builds a message.
#[derive(Debug, Default)]
pub struct Encoder(pub Vec<u8>);

impl Encoder {
    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn i64(&mut self, v: i64) -> &mut Self {
        self.u64(v as u64)
    }

    pub fn bool(&mut self, v: bool) -> &mut Self {
        self.u64(v as u64)
    }

    pub fn u128(&mut self, v: u128) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.u64(v.len() as u64);
        self.0.extend_from_slice(v);
        self
    }

    pub fn opt_bytes(&mut self, v: Option<&[u8]>) -> &mut Self {
        self.bool(v.is_some());
        if let Some(v) = v {
            self.bytes(v);
        }
        self
    }
}

/// The message was cut short or malformed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Truncated;

impl std::fmt::Display for Truncated {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("truncated state message")
    }
}

impl std::error::Error for Truncated {}

/// Reads a message.
#[derive(Debug)]
pub struct Decoder<'a>(pub &'a [u8]);

impl<'a> Decoder<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Truncated> {
        if self.0.len() < n {
            return Err(Truncated);
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }

    pub fn u64(&mut self) -> Result<u64, Truncated> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn i64(&mut self) -> Result<i64, Truncated> {
        Ok(self.u64()? as i64)
    }

    pub fn bool(&mut self) -> Result<bool, Truncated> {
        Ok(self.u64()? != 0)
    }

    pub fn u128(&mut self) -> Result<u128, Truncated> {
        Ok(u128::from_le_bytes(self.take(16)?.try_into().unwrap()))
    }

    pub fn bytes(&mut self) -> Result<Vec<u8>, Truncated> {
        let n = self.u64()? as usize;
        Ok(self.take(n)?.to_vec())
    }

    pub fn opt_bytes(&mut self) -> Result<Option<Vec<u8>>, Truncated> {
        if self.bool()? { self.bytes().map(Some) } else { Ok(None) }
    }

    /// A count of items still to read, checked against what's left (each
    /// item takes at least `min_size` bytes) so a corrupt count can't make
    /// the reader allocate wildly.
    pub fn count(&mut self, min_size: usize) -> Result<usize, Truncated> {
        let n = self.u64()? as usize;
        if n.checked_mul(min_size.max(1)).is_none_or(|need| need > self.0.len()) {
            return Err(Truncated);
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let mut e = Encoder::default();
        e.u64(7).i64(-3).bool(true).u128(u128::MAX - 1).bytes(b"abc").opt_bytes(None).opt_bytes(Some(b"x")).u64(2);
        let mut d = Decoder(&e.0);
        assert_eq!(d.u64(), Ok(7));
        assert_eq!(d.i64(), Ok(-3));
        assert_eq!(d.bool(), Ok(true));
        assert_eq!(d.u128(), Ok(u128::MAX - 1));
        assert_eq!(d.bytes().unwrap(), b"abc");
        assert_eq!(d.opt_bytes(), Ok(None));
        assert_eq!(d.opt_bytes(), Ok(Some(b"x".to_vec())));
        assert_eq!(d.count(1), Err(Truncated), "2 items can't fit in 0 bytes");
        assert!(d.0.is_empty());
        assert_eq!(Decoder(&[1, 2]).u64(), Err(Truncated));
    }
}
