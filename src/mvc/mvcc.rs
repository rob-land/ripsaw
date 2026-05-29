// MVCDecoderConfigurationRecord reader.
//
// The MVC equivalent of `avcC`, defined in ISO/IEC 14496-15 § 7.4.
// Modern MakeMKV emits this as a Matroska BlockAdditionMapping with
// BlockAddIDType == 'mvcC' (0x6D766343). The record holds the dependent
// view's Subset SPS NAL unit(s) and PPS NAL unit(s) -- exactly what our
// `sps::parse_sps_mvc_extension` needs to be validated against.

use std::io::{Read, Seek};

use super::ebml::{self, EbmlError, EbmlReader};

/// `mvcC` magic as a u32 (corresponds to the four ASCII bytes `mvcC`).
pub const MVCC_TYPE: u32 = 0x6D76_6343;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MvcDecoderConfigurationRecord {
    pub configuration_version: u8,
    pub avc_profile_indication: u8,
    pub profile_compatibility: u8,
    pub avc_level_indication: u8,
    pub length_size_minus_one: u8,
    /// Raw Subset SPS (or regular SPS) NAL units, each including its
    /// header byte. Strip the first byte and pass the rest through
    /// `rbsp::extract_rbsp` to feed our subset-SPS parser.
    pub sps_nals: Vec<Vec<u8>>,
    pub pps_nals: Vec<Vec<u8>>,
}

#[derive(Debug, thiserror::Error)]
pub enum MvccError {
    #[error("buffer too short for header (got {0} bytes, need >= 6)")]
    TooShort(usize),
    #[error("buffer truncated while reading NAL unit list")]
    TruncatedNalList,
}

/// Parse the on-wire `MVCDecoderConfigurationRecord` bytes (the payload
/// of a Matroska `BlockAddIDExtraData` element with `BlockAddIDType ==
/// 'mvcC'`). The PPS / SPS extension section is not parsed in this pass
/// -- we stop after the SPS and PPS NAL lists which is all phase-1
/// validation needs.
pub fn parse(bytes: &[u8]) -> Result<MvcDecoderConfigurationRecord, MvccError> {
    if bytes.len() < 6 {
        return Err(MvccError::TooShort(bytes.len()));
    }
    let configuration_version = bytes[0];
    let avc_profile_indication = bytes[1];
    let profile_compatibility = bytes[2];
    let avc_level_indication = bytes[3];
    let length_size_minus_one = bytes[4] & 0b11;
    let num_sps = (bytes[5] & 0b0111_1111) as usize;

    let mut cursor = 6usize;
    let mut sps_nals = Vec::with_capacity(num_sps);
    for _ in 0..num_sps {
        if cursor + 2 > bytes.len() {
            return Err(MvccError::TruncatedNalList);
        }
        let len = u16::from_be_bytes([bytes[cursor], bytes[cursor + 1]]) as usize;
        cursor += 2;
        if cursor + len > bytes.len() {
            return Err(MvccError::TruncatedNalList);
        }
        sps_nals.push(bytes[cursor..cursor + len].to_vec());
        cursor += len;
    }

    if cursor >= bytes.len() {
        return Err(MvccError::TruncatedNalList);
    }
    let num_pps = bytes[cursor] as usize;
    cursor += 1;
    let mut pps_nals = Vec::with_capacity(num_pps);
    for _ in 0..num_pps {
        if cursor + 2 > bytes.len() {
            return Err(MvccError::TruncatedNalList);
        }
        let len = u16::from_be_bytes([bytes[cursor], bytes[cursor + 1]]) as usize;
        cursor += 2;
        if cursor + len > bytes.len() {
            return Err(MvccError::TruncatedNalList);
        }
        pps_nals.push(bytes[cursor..cursor + len].to_vec());
        cursor += len;
    }

    Ok(MvcDecoderConfigurationRecord {
        configuration_version,
        avc_profile_indication,
        profile_compatibility,
        avc_level_indication,
        length_size_minus_one,
        sps_nals,
        pps_nals,
    })
}

/// 3D content detected in an MKV. Carries the mvcC bytes when present
/// and the Matroska StereoMode element when one is set on the video
/// track.
#[derive(Debug, Clone, Default)]
pub struct Mkv3dInfo {
    pub mvcc_bytes: Option<Vec<u8>>,
    pub stereo_mode: Option<u64>,
}

impl Mkv3dInfo {
    /// `true` if either the mvcC BlockAddition is present OR the Matroska
    /// StereoMode indicates MVC-style packing (modes 13 and 14 = "both
    /// eyes laced in one block"). Modes 1/2/3 (already-packed
    /// side-by-side / over-under) are NOT MVC.
    pub fn has_mvc(&self) -> bool {
        if self.mvcc_bytes.is_some() {
            return true;
        }
        matches!(self.stereo_mode, Some(13) | Some(14))
    }
}

/// Walk an MKV and collect both the mvcC BlockAddition bytes (when
/// present) and the video track's StereoMode (when set). One pass over
/// the file.
pub fn scan_3d_info<R: Read + Seek>(
    reader: &mut EbmlReader<R>,
) -> Result<Mkv3dInfo, EbmlError> {
    let mut info = Mkv3dInfo::default();
    let segment_size = match walk_to(reader, ebml::id::SEGMENT, None)? {
        Some(s) => s,
        None => return Ok(info),
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
                    scan_track_entry_full(reader, entry_end, &mut info)?;
                    reader.seek(entry_end)?;
                } else {
                    reader.skip(size)?;
                }
            }
            return Ok(info);
        } else {
            reader.skip(size)?;
        }
    }
    Ok(info)
}

fn scan_track_entry_full<R: Read + Seek>(
    reader: &mut EbmlReader<R>,
    end: u64,
    info: &mut Mkv3dInfo,
) -> Result<(), EbmlError> {
    while reader.position()? < end {
        let id = reader.read_vint_id()?;
        let size = reader.read_vint_size()?;
        match id {
            ebml::id::BLOCK_ADDITION_MAPPING => {
                let map_end = reader.position()? + size;
                if let Some(bytes) = scan_block_addition_mapping(reader, map_end)? {
                    if info.mvcc_bytes.is_none() {
                        info.mvcc_bytes = Some(bytes);
                    }
                }
                reader.seek(map_end)?;
            }
            ebml::id::VIDEO => {
                let video_end = reader.position()? + size;
                while reader.position()? < video_end {
                    let id = reader.read_vint_id()?;
                    let size = reader.read_vint_size()?;
                    if id == ebml::id::STEREO_MODE {
                        let mode = reader.read_uint(size as usize)?;
                        if info.stereo_mode.is_none() {
                            info.stereo_mode = Some(mode);
                        }
                    } else {
                        reader.skip(size)?;
                    }
                }
            }
            _ => reader.skip(size)?,
        }
    }
    Ok(())
}

/// Walk an MKV looking for the first BlockAdditionMapping whose
/// BlockAddIDType equals `mvcC` (0x6D766343), and return the bytes
/// stored in its BlockAddIDExtraData. Returns `Ok(None)` if no such
/// mapping is present.
pub fn find_mvcc_bytes<R: Read + Seek>(
    reader: &mut EbmlReader<R>,
) -> Result<Option<Vec<u8>>, EbmlError> {
    // Find the Segment.
    let segment_size = match walk_to(reader, ebml::id::SEGMENT, None)? {
        Some(s) => s,
        None => return Ok(None),
    };
    let segment_end = reader.position()? + segment_size;

    // Walk Segment children looking for Tracks.
    while reader.position()? < segment_end {
        let id = reader.read_vint_id()?;
        let size = reader.read_vint_size()?;
        if id == ebml::id::TRACKS {
            let tracks_end = reader.position()? + size;
            // Walk TrackEntry children.
            while reader.position()? < tracks_end {
                let id = reader.read_vint_id()?;
                let size = reader.read_vint_size()?;
                if id == ebml::id::TRACK_ENTRY {
                    let entry_end = reader.position()? + size;
                    let bytes = scan_track_entry(reader, entry_end)?;
                    if bytes.is_some() {
                        return Ok(bytes);
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

/// Inside a single TrackEntry, walk children looking for a
/// BlockAdditionMapping whose BlockAddIDType == mvcC.
fn scan_track_entry<R: Read + Seek>(
    reader: &mut EbmlReader<R>,
    end: u64,
) -> Result<Option<Vec<u8>>, EbmlError> {
    while reader.position()? < end {
        let id = reader.read_vint_id()?;
        let size = reader.read_vint_size()?;
        if id == ebml::id::BLOCK_ADDITION_MAPPING {
            let map_end = reader.position()? + size;
            let bytes = scan_block_addition_mapping(reader, map_end)?;
            if bytes.is_some() {
                return Ok(bytes);
            }
            reader.seek(map_end)?;
        } else {
            reader.skip(size)?;
        }
    }
    Ok(None)
}

fn scan_block_addition_mapping<R: Read + Seek>(
    reader: &mut EbmlReader<R>,
    end: u64,
) -> Result<Option<Vec<u8>>, EbmlError> {
    let mut block_add_type: Option<u64> = None;
    let mut extra_data: Option<Vec<u8>> = None;
    while reader.position()? < end {
        let id = reader.read_vint_id()?;
        let size = reader.read_vint_size()?;
        match id {
            ebml::id::BLOCK_ADD_ID_TYPE => {
                block_add_type = Some(reader.read_uint(size as usize)?);
            }
            ebml::id::BLOCK_ADD_ID_EXTRA_DATA => {
                extra_data = Some(reader.read_bytes(size as usize)?);
            }
            _ => reader.skip(size)?,
        }
    }
    if block_add_type == Some(MVCC_TYPE as u64) {
        Ok(extra_data)
    } else {
        Ok(None)
    }
}

/// Walk top-level EBML elements until one with the given `target_id` is
/// found. Returns the size of that element (the cursor will be at the
/// start of its children).
fn walk_to<R: Read + Seek>(
    reader: &mut EbmlReader<R>,
    target_id: u32,
    max_search: Option<u64>,
) -> Result<Option<u64>, EbmlError> {
    let start = reader.position()?;
    loop {
        if let Some(limit) = max_search {
            if reader.position()? - start >= limit {
                return Ok(None);
            }
        }
        match reader.read_vint_id() {
            Ok(id) => {
                let size = reader.read_vint_size()?;
                if id == target_id {
                    return Ok(Some(size));
                }
                reader.skip(size)?;
            }
            Err(EbmlError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Ok(None);
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Construct a minimal mvcC byte sequence that holds one fake SPS
    /// NAL unit and no PPS, then assert the parser returns the right
    /// shape.
    #[test]
    fn parses_handcrafted_mvcc_with_one_sps_no_pps() {
        let sps_payload = vec![0x6F, 0x42, 0x12]; // type-15 header + dummy payload
        let len = sps_payload.len() as u16;
        let mut bytes = vec![
            0x01,                // configurationVersion
            0x80,                // AVCProfileIndication = 128 (Multiview High)
            0x00,                // profile_compatibility
            0x29,                // AVCLevelIndication = 0x29 (Level 4.1)
            0xFF,                // reserved | lengthSizeMinusOne = 3
            0x81,                // reserved bit + numOfSequenceParameterSets = 1
        ];
        bytes.extend_from_slice(&len.to_be_bytes());
        bytes.extend_from_slice(&sps_payload);
        bytes.push(0x00); // numOfPictureParameterSets = 0

        let parsed = parse(&bytes).expect("parse");
        assert_eq!(parsed.configuration_version, 1);
        assert_eq!(parsed.avc_profile_indication, 128);
        assert_eq!(parsed.avc_level_indication, 0x29);
        assert_eq!(parsed.length_size_minus_one, 3);
        assert_eq!(parsed.sps_nals.len(), 1);
        assert_eq!(parsed.sps_nals[0], sps_payload);
        assert_eq!(parsed.pps_nals.len(), 0);
    }

    #[test]
    fn rejects_truncated_buffer() {
        assert!(matches!(parse(&[0, 1, 2]), Err(MvccError::TooShort(3))));
    }

    #[test]
    fn rejects_truncated_nal_list() {
        // Header claims 1 SPS but no bytes follow.
        let bytes = vec![0x01, 0x80, 0x00, 0x29, 0xFF, 0x81];
        assert!(matches!(parse(&bytes), Err(MvccError::TruncatedNalList)));
    }
}
