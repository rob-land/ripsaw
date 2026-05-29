// Stream-extract a combined Annex B byte stream from an MKV whose
// dependent MVC view lives in BlockAdditions. The flow per frame:
//
//   Block / SimpleBlock           -> base view NAL units (length-prefixed)
//   BlockAdditional (BlockAddID 1) -> dependent view NAL units (length-prefixed)
//
// We rewrap each length-prefixed NAL as an Annex B unit (`0x000001` start
// code + payload) and write base view NALs followed by dependent view
// NALs for each frame, in cluster order. The mvcC config record's SPS
// and PPS NALs are emitted once at the head of the output so a downstream
// MVC decoder (we target JM ldecod) has the parameter sets it needs.

use std::io::{Read, Seek, Write};

use anyhow::{anyhow, Context, Result};

use super::ebml::{self, EbmlReader};
use super::mvcc::{find_mvcc_bytes, parse as parse_mvcc};

const ANNEX_B_START_CODE: &[u8] = &[0x00, 0x00, 0x00, 0x01];

/// The BlockAddID inside a BlockMore that carries the dependent view's
/// length-prefixed NAL data. Matroska assigns the value 1 to the
/// `BlockAdditionMapping` corresponding to the mvcC mapping by
/// convention.
const DEFAULT_BLOCK_ADD_ID: u64 = 1;

/// Walk an MKV, find the video track that carries an mvcC BlockAddition
/// mapping, and write a self-contained Annex B byte stream to `out`. The
/// resulting stream is what we hand to ldecod to recover both views.
///
/// Returns counts of (frames, base_nals, dep_nals) that were written.
pub fn extract_to_annex_b<R: Read + Seek, W: Write>(
    reader: &mut EbmlReader<R>,
    out: &mut W,
) -> Result<ExtractionStats> {
    // Pass 1: walk the Tracks block to learn (a) which track number is
    // the video track with mvcC, and (b) the bytes of its mvcC
    // BlockAddition extra data. The mvcC bytes give us the parameter
    // sets we need to seed the Annex B output.
    reader.seek(0)?;
    let info = find_mvc_track(reader)?
        .ok_or_else(|| anyhow!("no video track with mvcC BlockAdditionMapping found in MKV"))?;

    let mvcc = parse_mvcc(&info.mvcc_bytes).context("parsing mvcC record")?;
    // Header: each SPS / PPS NAL prefixed with the Annex B start code.
    for nal in mvcc.sps_nals.iter().chain(mvcc.pps_nals.iter()) {
        out.write_all(ANNEX_B_START_CODE)?;
        out.write_all(nal)?;
    }

    let length_size = mvcc.length_size_minus_one as usize + 1;
    if !(1..=4).contains(&length_size) {
        return Err(anyhow!("mvcC lengthSizeMinusOne yields {length_size}; expected 1..=4"));
    }

    // Pass 2: walk all Cluster -> Block / BlockGroup -> base + dep view
    // NAL units for the target track.
    reader.seek(0)?;
    let mut stats = ExtractionStats::default();
    walk_segment_for_blocks(reader, info.track_number, length_size, out, &mut stats)?;
    Ok(stats)
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ExtractionStats {
    pub frames: u64,
    pub base_nals: u64,
    pub dep_nals: u64,
}

#[derive(Debug, Clone)]
struct McvTrackInfo {
    track_number: u64,
    mvcc_bytes: Vec<u8>,
}

fn find_mvc_track<R: Read + Seek>(
    reader: &mut EbmlReader<R>,
) -> Result<Option<McvTrackInfo>> {
    let segment_size = match walk_to(reader, ebml::id::SEGMENT)? {
        Some(s) => s,
        None => return Ok(None),
    };
    let segment_end = reader.position()? + segment_size;
    while reader.position()? < segment_end {
        let id = reader.read_vint_id()?;
        let size = reader.read_vint_size()?;
        if id == ebml::id::TRACKS {
            let tracks_end = reader.position()? + size;
            while reader.position()? < tracks_end {
                let id = reader.read_vint_id()?;
                let size = reader.read_vint_size()?;
                if id == ebml::id::TRACK_ENTRY {
                    let entry_end = reader.position()? + size;
                    if let Some(info) = scan_track_entry(reader, entry_end)? {
                        return Ok(Some(info));
                    }
                    reader.seek(entry_end)?;
                } else {
                    reader.skip(size)?;
                }
            }
            return Ok(None);
        } else {
            reader.skip(size)?;
        }
    }
    Ok(None)
}

fn scan_track_entry<R: Read + Seek>(
    reader: &mut EbmlReader<R>,
    end: u64,
) -> Result<Option<McvTrackInfo>> {
    let mut track_number: Option<u64> = None;
    let mut mvcc_bytes: Option<Vec<u8>> = None;
    while reader.position()? < end {
        let id = reader.read_vint_id()?;
        let size = reader.read_vint_size()?;
        match id {
            ebml::id::TRACK_NUMBER => {
                track_number = Some(reader.read_uint(size as usize)?);
            }
            ebml::id::BLOCK_ADDITION_MAPPING => {
                let map_end = reader.position()? + size;
                if let Some(bytes) = scan_mapping_for_mvcc(reader, map_end)? {
                    mvcc_bytes = Some(bytes);
                }
                reader.seek(map_end)?;
            }
            _ => reader.skip(size)?,
        }
    }
    Ok(match (track_number, mvcc_bytes) {
        (Some(track_number), Some(mvcc_bytes)) => Some(McvTrackInfo { track_number, mvcc_bytes }),
        _ => None,
    })
}

fn scan_mapping_for_mvcc<R: Read + Seek>(
    reader: &mut EbmlReader<R>,
    end: u64,
) -> Result<Option<Vec<u8>>> {
    let mut bat: Option<u64> = None;
    let mut extra: Option<Vec<u8>> = None;
    while reader.position()? < end {
        let id = reader.read_vint_id()?;
        let size = reader.read_vint_size()?;
        match id {
            ebml::id::BLOCK_ADD_ID_TYPE => {
                bat = Some(reader.read_uint(size as usize)?);
            }
            ebml::id::BLOCK_ADD_ID_EXTRA_DATA => {
                extra = Some(reader.read_bytes(size as usize)?);
            }
            _ => reader.skip(size)?,
        }
    }
    if bat == Some(super::mvcc::MVCC_TYPE as u64) {
        Ok(extra)
    } else {
        Ok(None)
    }
}

fn walk_segment_for_blocks<R: Read + Seek, W: Write>(
    reader: &mut EbmlReader<R>,
    track_number: u64,
    length_size: usize,
    out: &mut W,
    stats: &mut ExtractionStats,
) -> Result<()> {
    let segment_size = match walk_to(reader, ebml::id::SEGMENT)? {
        Some(s) => s,
        None => return Err(anyhow!("MKV has no Segment")),
    };
    let segment_end = reader.position()? + segment_size;
    while reader.position()? < segment_end {
        let id = reader.read_vint_id()?;
        let size = reader.read_vint_size()?;
        if id == ebml::id::CLUSTER {
            let cluster_end = reader.position()? + size;
            walk_cluster(reader, cluster_end, track_number, length_size, out, stats)?;
        } else {
            reader.skip(size)?;
        }
    }
    Ok(())
}

fn walk_cluster<R: Read + Seek, W: Write>(
    reader: &mut EbmlReader<R>,
    end: u64,
    track_number: u64,
    length_size: usize,
    out: &mut W,
    stats: &mut ExtractionStats,
) -> Result<()> {
    while reader.position()? < end {
        let id = reader.read_vint_id()?;
        let size = reader.read_vint_size()?;
        match id {
            ebml::id::SIMPLE_BLOCK => {
                let block_end = reader.position()? + size;
                let block_bytes = reader.read_bytes(size as usize)?;
                if let Some(frame_data) = block_frame_data(&block_bytes, track_number)? {
                    write_length_prefixed(out, frame_data, length_size)?;
                    stats.frames += 1;
                    stats.base_nals += count_length_prefixed(frame_data, length_size)?;
                }
                let _ = block_end;
            }
            ebml::id::BLOCK_GROUP => {
                let bg_end = reader.position()? + size;
                walk_block_group(reader, bg_end, track_number, length_size, out, stats)?;
            }
            _ => reader.skip(size)?,
        }
    }
    Ok(())
}

fn walk_block_group<R: Read + Seek, W: Write>(
    reader: &mut EbmlReader<R>,
    end: u64,
    track_number: u64,
    length_size: usize,
    out: &mut W,
    stats: &mut ExtractionStats,
) -> Result<()> {
    let mut base_frame: Option<Vec<u8>> = None;
    let mut dep_frame: Option<Vec<u8>> = None;
    while reader.position()? < end {
        let id = reader.read_vint_id()?;
        let size = reader.read_vint_size()?;
        match id {
            ebml::id::BLOCK => {
                let block_bytes = reader.read_bytes(size as usize)?;
                if let Some(frame_data) = block_frame_data(&block_bytes, track_number)? {
                    base_frame = Some(frame_data.to_vec());
                }
            }
            ebml::id::BLOCK_ADDITIONS => {
                let adds_end = reader.position()? + size;
                if let Some(bytes) = pick_default_block_additional(reader, adds_end)? {
                    dep_frame = Some(bytes);
                }
            }
            _ => reader.skip(size)?,
        }
    }
    if let Some(base) = base_frame {
        write_length_prefixed(out, &base, length_size)?;
        stats.frames += 1;
        stats.base_nals += count_length_prefixed(&base, length_size)?;
        if let Some(dep) = dep_frame {
            write_length_prefixed(out, &dep, length_size)?;
            stats.dep_nals += count_length_prefixed(&dep, length_size)?;
        }
    }
    Ok(())
}

fn pick_default_block_additional<R: Read + Seek>(
    reader: &mut EbmlReader<R>,
    end: u64,
) -> Result<Option<Vec<u8>>> {
    let mut chosen: Option<Vec<u8>> = None;
    while reader.position()? < end {
        let id = reader.read_vint_id()?;
        let size = reader.read_vint_size()?;
        if id == ebml::id::BLOCK_MORE {
            let more_end = reader.position()? + size;
            let mut add_id: Option<u64> = None;
            let mut payload: Option<Vec<u8>> = None;
            while reader.position()? < more_end {
                let id = reader.read_vint_id()?;
                let size = reader.read_vint_size()?;
                match id {
                    ebml::id::BLOCK_ADD_ID => add_id = Some(reader.read_uint(size as usize)?),
                    ebml::id::BLOCK_ADDITIONAL => {
                        payload = Some(reader.read_bytes(size as usize)?);
                    }
                    _ => reader.skip(size)?,
                }
            }
            if add_id.unwrap_or(1) == DEFAULT_BLOCK_ADD_ID {
                if let Some(p) = payload {
                    chosen = Some(p);
                }
            }
        } else {
            reader.skip(size)?;
        }
    }
    Ok(chosen)
}

/// Decode the Matroska Block header on `block_bytes`. Returns the
/// length-prefixed NAL byte slice for the body, or `None` if the block
/// is for a different track or uses an unsupported lacing mode.
fn block_frame_data<'a>(
    block_bytes: &'a [u8],
    target_track: u64,
) -> Result<Option<&'a [u8]>> {
    // Block header: track-number VINT, then 2-byte signed timecode, then
    // 1-byte flags (bit 5..6 = lacing).
    let (track_number, vint_len) = read_vint_size_from(block_bytes)?;
    if track_number != target_track {
        return Ok(None);
    }
    let after_track = vint_len;
    if block_bytes.len() < after_track + 3 {
        return Err(anyhow!("Block too short for header"));
    }
    let flags = block_bytes[after_track + 2];
    let lacing = (flags >> 1) & 0b11;
    let body_start = after_track + 3;
    if lacing != 0 {
        // Lacing handling for AVC video is uncommon. Skip the block
        // rather than misinterpret it.
        return Ok(None);
    }
    Ok(Some(&block_bytes[body_start..]))
}

/// Read a VINT *size* (marker bit stripped) from the start of `bytes`,
/// returning the value and how many bytes it occupied.
fn read_vint_size_from(bytes: &[u8]) -> Result<(u64, usize)> {
    if bytes.is_empty() {
        return Err(anyhow!("empty VINT"));
    }
    let first = bytes[0];
    if first == 0 {
        return Err(anyhow!("invalid VINT (leading byte is 0)"));
    }
    let len = (first.leading_zeros() + 1) as usize;
    if len > 8 || len > bytes.len() {
        return Err(anyhow!("VINT length {len} not supported here"));
    }
    let mut raw = first as u64;
    for &b in &bytes[1..len] {
        raw = (raw << 8) | (b as u64);
    }
    raw &= !(1u64 << (7 * len));
    Ok((raw, len))
}

fn write_length_prefixed<W: Write>(out: &mut W, data: &[u8], length_size: usize) -> Result<()> {
    let mut cursor = 0usize;
    while cursor + length_size <= data.len() {
        let mut nal_len = 0u64;
        for i in 0..length_size {
            nal_len = (nal_len << 8) | (data[cursor + i] as u64);
        }
        let nal_len = nal_len as usize;
        cursor += length_size;
        if cursor + nal_len > data.len() {
            break;
        }
        out.write_all(ANNEX_B_START_CODE)?;
        out.write_all(&data[cursor..cursor + nal_len])?;
        cursor += nal_len;
    }
    Ok(())
}

fn count_length_prefixed(data: &[u8], length_size: usize) -> Result<u64> {
    let mut cursor = 0usize;
    let mut count = 0u64;
    while cursor + length_size <= data.len() {
        let mut nal_len = 0u64;
        for i in 0..length_size {
            nal_len = (nal_len << 8) | (data[cursor + i] as u64);
        }
        cursor += length_size;
        let nal_len = nal_len as usize;
        if cursor + nal_len > data.len() {
            break;
        }
        cursor += nal_len;
        count += 1;
    }
    Ok(count)
}

fn walk_to<R: Read + Seek>(
    reader: &mut EbmlReader<R>,
    target_id: u32,
) -> Result<Option<u64>> {
    loop {
        match reader.read_vint_id() {
            Ok(id) => {
                let size = reader.read_vint_size()?;
                if id == target_id {
                    return Ok(Some(size));
                }
                reader.skip(size)?;
            }
            Err(super::ebml::EbmlError::Io(e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                return Ok(None)
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// Suppress the `find_mvcc_bytes` import warning when the only mvcC
/// consumer in this module is via `parse_mvcc` on the already-found
/// bytes. (Reserved for the validated full-MKV path.)
#[allow(dead_code)]
fn _silence_unused(reader: &mut EbmlReader<std::io::Cursor<&[u8]>>) {
    let _ = find_mvcc_bytes(reader);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_vint_size_from_handles_one_byte() {
        // 0x82 = 1000 0010 -> length 1, value 2
        let (v, len) = read_vint_size_from(&[0x82, 0xFF]).unwrap();
        assert_eq!(v, 2);
        assert_eq!(len, 1);
    }

    #[test]
    fn write_length_prefixed_wraps_each_nal_with_start_code() {
        // length_size = 4. One NAL of 3 bytes.
        let mut data = vec![0u8, 0, 0, 3];
        data.extend_from_slice(&[0x67, 0x42, 0x00]);
        let mut out = Vec::new();
        write_length_prefixed(&mut out, &data, 4).unwrap();
        assert_eq!(out, vec![0, 0, 0, 1, 0x67, 0x42, 0x00]);
    }

    #[test]
    fn count_length_prefixed_matches_write() {
        // Two NALs back to back, each 2 bytes, with 4-byte length prefixes.
        let mut data = vec![];
        data.extend_from_slice(&[0, 0, 0, 2, 0x67, 0x42]);
        data.extend_from_slice(&[0, 0, 0, 2, 0x68, 0xCE]);
        assert_eq!(count_length_prefixed(&data, 4).unwrap(), 2);
    }

    #[test]
    fn block_frame_data_skips_lacing() {
        // Track = 1 (VINT 0x81), timecode 0x0000, flags 0x02 (lacing != 0).
        let block = [0x81, 0x00, 0x00, 0b0000_0010, 0xAA, 0xBB];
        let r = block_frame_data(&block, 1).unwrap();
        assert!(r.is_none());
    }

    #[test]
    fn block_frame_data_filters_by_track() {
        let block = [0x82, 0x00, 0x00, 0, 0xAA];
        // Block is for track 2; we want track 1.
        let r = block_frame_data(&block, 1).unwrap();
        assert!(r.is_none());
    }

    #[test]
    fn block_frame_data_returns_body_after_header() {
        let block = [0x81, 0x00, 0x00, 0, 0xAA, 0xBB, 0xCC];
        let body = block_frame_data(&block, 1).unwrap().unwrap();
        assert_eq!(body, &[0xAA, 0xBB, 0xCC]);
    }
}
