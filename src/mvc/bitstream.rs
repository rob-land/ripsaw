// Bit-level reader for H.264 NAL payloads. Implements raw `u(n)` reads
// and Exp-Golomb coded values (`ue(v)` / `se(v)`) per ITU-T H.264
// § 9.1 ("Parsing process for Exp-Golomb codes"). Big-endian (MSB-first)
// because that's what H.264 uses.

#[derive(Debug, Clone)]
pub struct BitReader<'a> {
    data: &'a [u8],
    byte_pos: usize,
    bit_pos: u8, // 0..8, counted from the MSB within data[byte_pos]
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ReadError {
    #[error("ran off the end of the bitstream")]
    Truncated,
    #[error("exp-Golomb codeword length exceeds 32 bits")]
    ExpGolombOverflow,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, byte_pos: 0, bit_pos: 0 }
    }

    /// Total bits remaining (informational).
    pub fn bits_left(&self) -> u64 {
        if self.byte_pos >= self.data.len() {
            return 0;
        }
        ((self.data.len() - self.byte_pos) as u64) * 8 - self.bit_pos as u64
    }

    pub fn byte_aligned(&self) -> bool {
        self.bit_pos == 0
    }

    /// Absolute bit offset consumed so far (MSB-first). Used to locate the
    /// byte-aligned CABAC slice data that follows a parsed slice header.
    pub fn position_bits(&self) -> usize {
        self.byte_pos * 8 + self.bit_pos as usize
    }

    /// `more_rbsp_data()` per H.264 § 7.2. Returns true if there is RBSP
    /// payload remaining before the `rbsp_stop_one_bit`. The stop bit is
    /// the last set bit in the whole buffer (everything after it is
    /// alignment zero-padding), so we compare the current position to it.
    pub fn more_rbsp_data(&self) -> bool {
        if self.byte_pos >= self.data.len() {
            return false;
        }
        // Absolute bit index (MSB-first) of the last set bit in the buffer.
        let mut last_byte = self.data.len();
        while last_byte > 0 && self.data[last_byte - 1] == 0 {
            last_byte -= 1;
        }
        if last_byte == 0 {
            return false; // no set bits at all — not a valid RBSP tail
        }
        let stop_in_byte = 7 - self.data[last_byte - 1].trailing_zeros() as usize;
        let stop_abs = (last_byte - 1) * 8 + stop_in_byte;
        let cur_abs = self.byte_pos * 8 + self.bit_pos as usize;
        cur_abs < stop_abs
    }

    /// Read a single bit. Returns `Ok(true)` if the bit was 1.
    pub fn read_bit(&mut self) -> Result<bool, ReadError> {
        if self.byte_pos >= self.data.len() {
            return Err(ReadError::Truncated);
        }
        let byte = self.data[self.byte_pos];
        let bit = (byte >> (7 - self.bit_pos)) & 1;
        self.bit_pos += 1;
        if self.bit_pos == 8 {
            self.byte_pos += 1;
            self.bit_pos = 0;
        }
        Ok(bit == 1)
    }

    /// Read `n` bits (1..=32) as an unsigned integer.
    pub fn read_u(&mut self, n: u32) -> Result<u32, ReadError> {
        debug_assert!(n >= 1 && n <= 32);
        let mut acc: u32 = 0;
        for _ in 0..n {
            acc = (acc << 1) | (self.read_bit()? as u32);
        }
        Ok(acc)
    }

    /// Read an unsigned Exp-Golomb code (`ue(v)`).
    /// Algorithm: count leading zeros `k`; the code value is
    /// `(1 << k) - 1 + read_u(k)`. `k == 0` yields 0.
    pub fn read_ue(&mut self) -> Result<u32, ReadError> {
        let mut k = 0u32;
        while !self.read_bit()? {
            k += 1;
            if k > 31 {
                return Err(ReadError::ExpGolombOverflow);
            }
        }
        if k == 0 {
            return Ok(0);
        }
        let tail = self.read_u(k)?;
        Ok((1u32 << k) - 1 + tail)
    }

    /// Read a signed Exp-Golomb code (`se(v)`). Maps via the inverse of
    /// the codeword table in § 9.1.1: 0→0, 1→1, 2→-1, 3→2, 4→-2 ...
    pub fn read_se(&mut self) -> Result<i32, ReadError> {
        let code = self.read_ue()? as i64;
        let value = if code % 2 == 0 {
            -(code / 2)
        } else {
            (code + 1) / 2
        };
        Ok(value as i32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_single_bits() {
        // 0b1010_0110 = 0xA6
        let mut r = BitReader::new(&[0xA6]);
        assert!(r.read_bit().unwrap());
        assert!(!r.read_bit().unwrap());
        assert!(r.read_bit().unwrap());
        assert!(!r.read_bit().unwrap());
        assert!(!r.read_bit().unwrap());
        assert!(r.read_bit().unwrap());
        assert!(r.read_bit().unwrap());
        assert!(!r.read_bit().unwrap());
        assert_eq!(r.read_bit().unwrap_err(), ReadError::Truncated);
    }

    #[test]
    fn read_u_crosses_byte_boundaries() {
        // 0xA5 = 0b1010_0101, 0x5A = 0b0101_1010
        // read_u(12) over [0xA5, 0x5A] should produce 0b1010_0101_0101 = 0xA55 = 2645
        let mut r = BitReader::new(&[0xA5, 0x5A]);
        assert_eq!(r.read_u(12).unwrap(), 0xA55);
        // 4 bits remaining: 0b1010 = 10
        assert_eq!(r.read_u(4).unwrap(), 0b1010);
    }

    #[test]
    fn read_ue_for_known_codewords() {
        // From H.264 § 9.1 Table 9-1:
        //   codeword `1`      → 0
        //   codeword `010`    → 1
        //   codeword `011`    → 2
        //   codeword `00100`  → 3
        //   codeword `00101`  → 4
        //   codeword `00110`  → 5
        //   codeword `00111`  → 6
        // Concatenating the eight codewords above:
        //   1 010 011 00100 00101 00110 00111
        // = 1010 0110 0100 0010 1001 1000 111  (27 bits)
        // Pad with one trailing 0 to make 28 bits, then to 32 with zeros:
        //   1010 0110 0100 0010 1001 1000 1110 0000
        // = 0xA6 0x42 0x98 0xE0
        let mut r = BitReader::new(&[0xA6, 0x42, 0x98, 0xE0]);
        let values: Vec<u32> = (0..7).map(|_| r.read_ue().unwrap()).collect();
        assert_eq!(values, vec![0, 1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn read_se_via_known_mapping() {
        // codeNum to value: 0→0, 1→1, 2→-1, 3→2, 4→-2
        // Codewords:        1, 010, 011, 00100, 00101
        // Concatenated:     1 010 011 00100 00101
        // = 1010 0110 0100 0010 1   = 17 bits, pad to 24
        // = 1010 0110 0100 0010 1000 0000  = 0xA6 0x42 0x80
        let mut r = BitReader::new(&[0xA6, 0x42, 0x80]);
        assert_eq!(r.read_se().unwrap(), 0);
        assert_eq!(r.read_se().unwrap(), 1);
        assert_eq!(r.read_se().unwrap(), -1);
        assert_eq!(r.read_se().unwrap(), 2);
        assert_eq!(r.read_se().unwrap(), -2);
    }

    #[test]
    fn read_ue_at_buffer_edge_returns_truncated() {
        // Single 0 byte: all leading zeros, no terminator.
        let mut r = BitReader::new(&[0x00]);
        assert_eq!(r.read_ue().unwrap_err(), ReadError::Truncated);
    }

    #[test]
    fn read_ue_overflow_after_32_leading_zeros() {
        // 32 leading zeros then a 1 — that's a codeword of length 65 bits
        // which doesn't fit in u32 anyway. Five all-zero bytes is plenty.
        let mut r = BitReader::new(&[0, 0, 0, 0, 0, 0x80]);
        assert_eq!(r.read_ue().unwrap_err(), ReadError::ExpGolombOverflow);
    }

    #[test]
    fn more_rbsp_data_detects_stop_bit() {
        // One byte: 0b1100_1000. The rbsp_stop_one_bit is the lowest set
        // bit (bit index 4, MSB-first), so payload bits are indices 0..4.
        let mut r = BitReader::new(&[0b1100_1000]);
        assert!(r.more_rbsp_data()); // at 0, stop at 4
        r.read_u(3).unwrap(); // -> index 3
        assert!(r.more_rbsp_data()); // 3 < 4
        r.read_bit().unwrap(); // -> index 4 (the stop bit itself)
        assert!(!r.more_rbsp_data()); // at the stop bit: nothing more
    }

    #[test]
    fn more_rbsp_data_false_when_exhausted_or_all_zero() {
        let mut r = BitReader::new(&[0x80]);
        assert!(!r.more_rbsp_data()); // only the stop bit, at index 0
        let r2 = BitReader::new(&[0x00, 0x00]);
        assert!(!r2.more_rbsp_data()); // no set bits anywhere
        let mut r3 = BitReader::new(&[0xFF]);
        r3.read_u(8).unwrap();
        assert!(!r3.more_rbsp_data()); // past the end
    }

    #[test]
    fn byte_aligned_tracks_position() {
        let mut r = BitReader::new(&[0xFF, 0xFF]);
        assert!(r.byte_aligned());
        r.read_bit().unwrap();
        assert!(!r.byte_aligned());
        let _ = r.read_u(7);
        assert!(r.byte_aligned());
    }
}
