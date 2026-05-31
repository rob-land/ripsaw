// DVD-Video region detection from VIDEO_TS/VIDEO_TS.IFO.
//
// The DVD-Video spec stores the "prohibited regions" mask as the
// second byte (bits 16-23, big-endian) of the `vmg_category` u32 in
// the VMG_MAT at offset 0x22 of `VIDEO_TS.IFO`. So byte 0x23 of the
// file is the mask:
//
//   0x00 -> Region 0 (no regions prohibited; plays anywhere)
//   bit N set -> region N+1 is PROHIBITED
//
//   0xFE -> only bit 0 clear -> Region 1 only
//   0xFD -> Region 2 only
//   0xFB -> Region 3 only, etc.
//
// Multi-region discs clear multiple bits; we surface the lowest-
// numbered allowed region as the canonical "region code" -- studios
// that publish multi-region discs nearly always include region 1.

use std::path::Path;

use anyhow::Result;

/// Read the prohibited-regions mask byte from a mounted DVD-Video
/// disc. Returns `Ok(None)` when the file isn't there or doesn't have
/// the VMG magic -- callers treat missing region info as "unknown."
pub fn read_region_mask(mount: &Path) -> Result<Option<u8>> {
    let ifo = mount.join("VIDEO_TS").join("VIDEO_TS.IFO");
    if !ifo.is_file() {
        return Ok(None);
    }
    let bytes = std::fs::read(&ifo)?;
    if bytes.len() < 0x24 {
        return Ok(None);
    }
    if &bytes[..12] != b"DVDVIDEO-VMG" {
        return Ok(None);
    }
    Ok(Some(bytes[0x23]))
}

/// Translate a prohibited-regions mask byte into a single region
/// number. Picks the lowest-numbered allowed region when multiple are
/// allowed. Returns `Some("0")` for region-free discs and `None` when
/// no region is allowed (unusual).
pub fn region_code_from_mask(mask: u8) -> Option<&'static str> {
    if mask == 0x00 {
        return Some("0");
    }
    if mask == 0xFF {
        return None;
    }
    for n in 0..8 {
        if mask & (1 << n) == 0 {
            return Some(match n {
                0 => "1",
                1 => "2",
                2 => "3",
                3 => "4",
                4 => "5",
                5 => "6",
                6 => "7",
                7 => "8",
                _ => unreachable!(),
            });
        }
    }
    None
}

/// Read the IFO and return the region code in one step.
pub fn read_region_code(mount: &Path) -> Result<Option<&'static str>> {
    Ok(read_region_mask(mount)?.and_then(region_code_from_mask))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_zero_for_zero_mask() {
        assert_eq!(region_code_from_mask(0x00), Some("0"));
    }

    #[test]
    fn region_one_for_0xfe() {
        assert_eq!(region_code_from_mask(0xFE), Some("1"));
    }

    #[test]
    fn region_two_for_0xfd() {
        assert_eq!(region_code_from_mask(0xFD), Some("2"));
    }

    #[test]
    fn region_four_for_0xf7() {
        assert_eq!(region_code_from_mask(0xF7), Some("4"));
    }

    #[test]
    fn lowest_region_wins_for_multi_region() {
        // 0xFC = 0b11111100: regions 1 + 2 allowed; lowest wins.
        assert_eq!(region_code_from_mask(0xFC), Some("1"));
    }

    #[test]
    fn no_regions_allowed_yields_none() {
        assert_eq!(region_code_from_mask(0xFF), None);
    }

    #[test]
    fn read_region_returns_none_when_no_video_ts() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(read_region_mask(tmp.path()).unwrap(), None);
    }

    #[test]
    fn read_region_returns_none_for_non_dvd_ifo() {
        let tmp = tempfile::tempdir().unwrap();
        let ifo_dir = tmp.path().join("VIDEO_TS");
        std::fs::create_dir_all(&ifo_dir).unwrap();
        std::fs::write(
            ifo_dir.join("VIDEO_TS.IFO"),
            b"NOTAVMG\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0",
        )
        .unwrap();
        assert_eq!(read_region_mask(tmp.path()).unwrap(), None);
    }

    #[test]
    fn read_region_returns_byte_when_magic_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let ifo_dir = tmp.path().join("VIDEO_TS");
        std::fs::create_dir_all(&ifo_dir).unwrap();
        let mut buf = vec![0u8; 0x40];
        buf[..12].copy_from_slice(b"DVDVIDEO-VMG");
        buf[0x23] = 0xFE;
        std::fs::write(ifo_dir.join("VIDEO_TS.IFO"), &buf).unwrap();
        assert_eq!(read_region_mask(tmp.path()).unwrap(), Some(0xFE));
        assert_eq!(read_region_code(tmp.path()).unwrap(), Some("1"));
    }
}
