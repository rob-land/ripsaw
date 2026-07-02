// Walk a raw H.264 byte stream (Annex B format) and yield individual
// NAL units. The byte stream uses 0x000001 or 0x00000001 as a NAL
// start code; the splitter strips the start code and returns the
// payload bytes belonging to each NAL.
//
// See H.264 § B.1 for the byte stream specification.

/// Iterator that scans a byte stream once and yields each NAL unit as
/// a `&[u8]` slice into the source buffer. Cheap: no allocations.
pub struct NalSplitter<'a> {
    data: &'a [u8],
    cursor: usize,
}

impl<'a> NalSplitter<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, cursor: 0 }
    }
}

impl<'a> Iterator for NalSplitter<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        // Locate the next start code (0x000001 or 0x00000001) at or
        // after `cursor`. The first one identifies where the next NAL
        // payload begins; the one after that marks where it ends.
        let nal_start = find_start_code(self.data, self.cursor)?;
        self.cursor = nal_start;

        // Skip the start code itself.
        let after_start = skip_start_code(self.data, nal_start)?;

        // Find the next start code (the boundary of this NAL).
        let nal_end = find_start_code(self.data, after_start).unwrap_or(self.data.len());
        let payload = &self.data[after_start..nal_end];

        // Strip the optional `cabac_zero_word`-style trailing zeros
        // that the spec allows -- they're not part of the NAL.
        let trimmed_end = trim_trailing_zeros(payload);

        self.cursor = nal_end;
        Some(&payload[..trimmed_end])
    }
}

/// Search `data` starting at `from` for the next NAL start code
/// (`0x000001` or `0x00000001`). Returns the index of the *first*
/// byte of the start code.
fn find_start_code(data: &[u8], from: usize) -> Option<usize> {
    if data.len() < 3 || from >= data.len() {
        return None;
    }
    let mut i = from;
    while i + 2 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 {
            if data[i + 2] == 1 {
                return Some(i);
            }
            if i + 3 < data.len() && data[i + 2] == 0 && data[i + 3] == 1 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// Skip the 3- or 4-byte start code at `from`, returning the index of
/// the first byte after it. Caller guarantees a start code is there.
fn skip_start_code(data: &[u8], from: usize) -> Option<usize> {
    if data.len() <= from + 2 {
        return None;
    }
    if data[from] == 0 && data[from + 1] == 0 && data[from + 2] == 1 {
        return Some(from + 3);
    }
    if data.len() > from + 3
        && data[from] == 0
        && data[from + 1] == 0
        && data[from + 2] == 0
        && data[from + 3] == 1
    {
        return Some(from + 4);
    }
    None
}

fn trim_trailing_zeros(data: &[u8]) -> usize {
    let mut end = data.len();
    while end > 0 && data[end - 1] == 0 {
        end -= 1;
    }
    end
}

/// Streaming counterpart of [`NalSplitter`]: yields each NAL unit as an owned
/// `Vec<u8>` while reading the byte stream incrementally from any `Read`, so the
/// whole elementary stream is never held in memory at once. Only the current
/// NAL (plus a small read-ahead) is buffered — memory stays bounded regardless
/// of stream length, which is what lets the decoder handle a full retail movie.
///
/// Same delimiting as [`NalSplitter`] (3- or 4-byte start codes, trailing zeros
/// trimmed); emulation prevention guarantees `00 00 01` only appears as a start
/// code, so scanning the payload for the next start code is unambiguous.
pub struct NalReader<R: std::io::Read> {
    reader: R,
    /// Unconsumed bytes: after the leading start code is skipped this begins at
    /// the current NAL's payload.
    buf: Vec<u8>,
    /// Resume offset for the next-start-code search (avoids rescanning the
    /// whole growing buffer each refill).
    scan: usize,
    started: bool,
    eof: bool,
}

impl<R: std::io::Read> NalReader<R> {
    pub fn new(reader: R) -> Self {
        Self { reader, buf: Vec::new(), scan: 0, started: false, eof: false }
    }

    /// Read one more chunk into `buf`; sets `eof` at end of stream.
    fn read_more(&mut self) -> std::io::Result<()> {
        let mut chunk = [0u8; 64 * 1024];
        let n = self.reader.read(&mut chunk)?;
        if n == 0 {
            self.eof = true;
        } else {
            self.buf.extend_from_slice(&chunk[..n]);
        }
        Ok(())
    }
}

impl<R: std::io::Read> Iterator for NalReader<R> {
    type Item = std::io::Result<Vec<u8>>;

    fn next(&mut self) -> Option<std::io::Result<Vec<u8>>> {
        // Skip the leading start code once: drop everything up to and including
        // the first start code, leaving `buf` at the first NAL's payload.
        if !self.started {
            loop {
                if let Some(sc) = find_start_code(&self.buf, 0) {
                    let after = skip_start_code(&self.buf, sc).expect("start code just found");
                    self.buf.drain(..after);
                    self.started = true;
                    self.scan = 0;
                    break;
                }
                if self.eof {
                    return None; // no start code anywhere
                }
                if let Err(e) = self.read_more() {
                    return Some(Err(e));
                }
            }
        }
        // Find the next start code = end of the current NAL, refilling as needed.
        loop {
            if let Some(sc) = find_start_code(&self.buf, self.scan) {
                let end = trim_trailing_zeros(&self.buf[..sc]);
                let nal = self.buf[..end].to_vec();
                let after = skip_start_code(&self.buf, sc).expect("start code just found");
                self.buf.drain(..after);
                self.scan = 0;
                return Some(Ok(nal));
            }
            if self.eof {
                if self.buf.is_empty() {
                    return None;
                }
                let end = trim_trailing_zeros(&self.buf);
                let nal = self.buf[..end].to_vec();
                self.buf.clear();
                self.scan = 0;
                if end == 0 {
                    return None; // trailing padding only
                }
                return Some(Ok(nal));
            }
            // Resume the next search 3 bytes back so a start code straddling the
            // read boundary is still found.
            self.scan = self.buf.len().saturating_sub(3);
            if let Err(e) = self.read_more() {
                return Some(Err(e));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_three_byte_start_codes() {
        // 0x00 0x00 0x01 0x67 (SPS) 0x00 0x00 0x01 0x68 (PPS)
        let data = b"\x00\x00\x01\x67\x42\x00\x00\x01\x68\x42";
        let nals: Vec<&[u8]> = NalSplitter::new(data).collect();
        assert_eq!(nals.len(), 2);
        assert_eq!(nals[0], &[0x67, 0x42]);
        assert_eq!(nals[1], &[0x68, 0x42]);
    }

    #[test]
    fn splits_four_byte_start_codes() {
        // 0x00 0x00 0x00 0x01 0x67 ...
        let data = b"\x00\x00\x00\x01\x67\x42\x00\x00\x00\x01\x68\x42";
        let nals: Vec<&[u8]> = NalSplitter::new(data).collect();
        assert_eq!(nals.len(), 2);
        assert_eq!(nals[0], &[0x67, 0x42]);
        assert_eq!(nals[1], &[0x68, 0x42]);
    }

    #[test]
    fn handles_mixed_three_and_four_byte_start_codes() {
        let data = b"\x00\x00\x00\x01\x67\x00\x00\x01\x68\x00\x00\x00\x01\x65";
        let nals: Vec<&[u8]> = NalSplitter::new(data).collect();
        assert_eq!(nals.len(), 3);
        assert_eq!(nals[0], &[0x67]);
        assert_eq!(nals[1], &[0x68]);
        assert_eq!(nals[2], &[0x65]);
    }

    #[test]
    fn empty_input_yields_no_nals() {
        let nals: Vec<&[u8]> = NalSplitter::new(&[]).collect();
        assert!(nals.is_empty());
    }

    #[test]
    fn data_without_start_codes_yields_no_nals() {
        let data = b"\x01\x02\x03\x04";
        let nals: Vec<&[u8]> = NalSplitter::new(data).collect();
        assert!(nals.is_empty());
    }

    #[test]
    fn trims_trailing_zero_padding() {
        // NAL bytes 0x67 0x42, then trailing zero padding before EOF.
        let data = b"\x00\x00\x01\x67\x42\x00\x00\x00";
        let nals: Vec<&[u8]> = NalSplitter::new(data).collect();
        assert_eq!(nals.len(), 1);
        assert_eq!(nals[0], &[0x67, 0x42]);
    }

    /// The streaming `NalReader` must yield exactly the same NALs as the
    /// in-memory `NalSplitter`, for any read-chunking. A tiny `Read` wrapper
    /// that hands out at most `step` bytes per call exercises start codes that
    /// straddle read boundaries.
    struct Chunked<'a> {
        data: &'a [u8],
        pos: usize,
        step: usize,
    }
    impl std::io::Read for Chunked<'_> {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            let n = self.step.min(out.len()).min(self.data.len() - self.pos);
            out[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    fn stream_nals(data: &[u8], step: usize) -> Vec<Vec<u8>> {
        NalReader::new(Chunked { data, pos: 0, step })
            .map(|r| r.unwrap())
            .collect()
    }

    #[test]
    fn nal_reader_matches_splitter_for_all_chunk_sizes() {
        let cases: &[&[u8]] = &[
            b"\x00\x00\x01\x67\x42\x00\x00\x01\x68\x42",
            b"\x00\x00\x00\x01\x67\x42\x00\x00\x00\x01\x68\x42",
            b"\x00\x00\x00\x01\x67\x00\x00\x01\x68\x00\x00\x00\x01\x65",
            b"\x00\x00\x01\x67\x42\x00\x00\x00", // trailing zero padding
            b"", b"\x01\x02\x03\x04", // no start codes
        ];
        for data in cases {
            let want: Vec<Vec<u8>> = NalSplitter::new(data).map(|s| s.to_vec()).collect();
            // Every chunk size from 1 byte up to the whole buffer at once.
            for step in 1..=data.len().max(1) {
                assert_eq!(stream_nals(data, step), want, "case {data:?} step {step}");
            }
        }
    }
}
