// Minimal EBML / Matroska element walker. Just enough to find a
// specific element (and its children) inside an MKV. Not a full MKV
// parser -- we don't model cluster blocks, attachments, etc. The only
// consumer is `mvcc::find_mvcc_bytes` which walks
// Segment / Tracks / TrackEntry / BlockAdditionMapping looking for the
// BlockAddIDExtraData of a BlockAddIDType == "mvcC" entry.

use std::io::{self, Read, Seek, SeekFrom};

#[derive(Debug, thiserror::Error)]
pub enum EbmlError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("invalid VINT (length byte is 0)")]
    InvalidVint,
    #[error("VINT length {0} > 8 unsupported")]
    OversizedVint(u8),
    #[error("element of size {0} is too large to load into memory")]
    ElementTooLarge(u64),
}

// Canonical EBML IDs we care about. These are the on-wire bytes
// including the leading-1 length marker bit.
pub mod id {
    pub const SEGMENT: u32 = 0x1853_8067;
    pub const TRACKS: u32 = 0x1654_AE6B;
    pub const TRACK_ENTRY: u32 = 0xAE;
    pub const BLOCK_ADDITION_MAPPING: u32 = 0x41E4;
    pub const BLOCK_ADD_ID_TYPE: u32 = 0x41E7;
    pub const BLOCK_ADD_ID_EXTRA_DATA: u32 = 0x41ED;
}

pub struct EbmlReader<R: Read + Seek> {
    inner: R,
}

impl<R: Read + Seek> EbmlReader<R> {
    pub fn new(inner: R) -> Self {
        Self { inner }
    }

    pub fn into_inner(self) -> R {
        self.inner
    }

    pub fn position(&mut self) -> io::Result<u64> {
        self.inner.stream_position()
    }

    pub fn seek(&mut self, pos: u64) -> io::Result<u64> {
        self.inner.seek(SeekFrom::Start(pos))
    }

    /// Read a VINT, returning the full canonical ID (the leading-1 marker
    /// bit is kept). For a 1-byte ID like 0xAE the function returns 0xAE
    /// as a u32; for a 4-byte ID like 0x1654AE6B it returns the same.
    pub fn read_vint_id(&mut self) -> Result<u32, EbmlError> {
        let (raw, len) = self.read_vint_raw()?;
        if len > 4 {
            return Err(EbmlError::OversizedVint(len));
        }
        Ok(raw as u32)
    }

    /// Read a VINT and strip the leading-1 marker bit. This is the form
    /// used for element sizes.
    pub fn read_vint_size(&mut self) -> Result<u64, EbmlError> {
        let (raw, len) = self.read_vint_raw()?;
        Ok(raw & !(1u64 << (7 * len)))
    }

    /// Returns (raw VINT bits, total length in bytes).
    fn read_vint_raw(&mut self) -> Result<(u64, u8), EbmlError> {
        let mut first = [0u8; 1];
        self.inner.read_exact(&mut first)?;
        if first[0] == 0 {
            return Err(EbmlError::InvalidVint);
        }
        let len = (first[0].leading_zeros() + 1) as u8;
        if len > 8 {
            return Err(EbmlError::OversizedVint(len));
        }
        let mut raw = first[0] as u64;
        if len > 1 {
            let mut tail = [0u8; 7];
            let n = (len - 1) as usize;
            self.inner.read_exact(&mut tail[..n])?;
            for &b in &tail[..n] {
                raw = (raw << 8) | (b as u64);
            }
        }
        Ok((raw, len))
    }

    pub fn read_bytes(&mut self, n: usize) -> io::Result<Vec<u8>> {
        let mut out = vec![0u8; n];
        self.inner.read_exact(&mut out)?;
        Ok(out)
    }

    pub fn read_uint(&mut self, n: usize) -> io::Result<u64> {
        let mut acc = 0u64;
        for &b in &self.read_bytes(n)? {
            acc = (acc << 8) | (b as u64);
        }
        Ok(acc)
    }

    pub fn skip(&mut self, n: u64) -> io::Result<()> {
        self.inner.seek(SeekFrom::Current(n as i64))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn one_byte_id_parses() {
        let mut r = EbmlReader::new(Cursor::new(&[0xAE][..]));
        assert_eq!(r.read_vint_id().unwrap(), 0xAE);
    }

    #[test]
    fn four_byte_id_parses() {
        // Segment = 0x18 53 80 67
        let mut r = EbmlReader::new(Cursor::new(&[0x18, 0x53, 0x80, 0x67][..]));
        assert_eq!(r.read_vint_id().unwrap(), 0x1853_8067);
    }

    #[test]
    fn vint_size_strips_marker_bit() {
        // 1-byte size encoding: 0xA5 -> binary 1010_0101 -> length 1, value 0x25
        let mut r = EbmlReader::new(Cursor::new(&[0xA5][..]));
        assert_eq!(r.read_vint_size().unwrap(), 0x25);

        // 2-byte size: 0x40_42 -> length 2 (leading 0100_0000), value 0x0042 = 66
        let mut r = EbmlReader::new(Cursor::new(&[0x40, 0x42][..]));
        assert_eq!(r.read_vint_size().unwrap(), 66);
    }

    #[test]
    fn invalid_zero_byte_rejected() {
        let mut r = EbmlReader::new(Cursor::new(&[0x00][..]));
        assert!(matches!(r.read_vint_id(), Err(EbmlError::InvalidVint)));
    }
}
