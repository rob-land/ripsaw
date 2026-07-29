// Blu-ray playlist (.mpls) navigation — pick the 3D feature and its clip order.
//
// A 3D Blu-ray's BDMV/PLAYLIST/*.mpls files each describe a play sequence: an
// ordered list of PlayItems, every one naming a clip (00000, 00001, …) with an
// IN/OUT time. The feature is the longest playlist whose clips are all 3D (have
// a matching BDMV/STREAM/SSIF/<clip>.ssif). This replaces the "largest single
// SSIF" heuristic so multi-title discs pick the movie (not a trailer/menu) and
// multi-clip features decode their clips in the right order.
//
// We parse only what we need — clip names + durations — and skip each PlayItem
// via its length field, so the STN tables / angle blocks don't have to be
// understood. 3D is detected by the SSIF's presence on disk, not by decoding the
// STN_table_SS extension.

use std::path::{Path, PathBuf};

/// One PlayItem: the clip it plays and its IN/OUT presentation times (45 kHz).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayItem {
    pub clip: String,
    pub in_time: u32,
    pub out_time: u32,
}

impl PlayItem {
    fn duration_ticks(&self) -> u64 {
        (self.out_time.saturating_sub(self.in_time)) as u64
    }
}

/// The chosen 3D feature: its clips in play order, and total duration (45 kHz).
#[derive(Debug, Clone)]
pub struct Feature {
    pub clips: Vec<String>,
    pub duration_ticks: u64,
}

impl Feature {
    /// Seconds of runtime (45 kHz presentation clock).
    pub fn duration_seconds(&self) -> u64 {
        self.duration_ticks / 45_000
    }

    /// Map the feature's clips to their `(ssif, m2ts)` paths under `mount`.
    pub fn clip_paths(&self, mount: &Path) -> Vec<(PathBuf, PathBuf)> {
        self.clips
            .iter()
            .map(|c| {
                (
                    mount.join("BDMV/STREAM/SSIF").join(format!("{c}.ssif")),
                    mount.join("BDMV/STREAM").join(format!("{c}.m2ts")),
                )
            })
            .collect()
    }
}

/// Parse the PlayItems out of a `.mpls` file. Returns `None` if the header or
/// structure is malformed. Bounds-checked throughout — a truncated/garbage file
/// yields `None` rather than panicking.
pub fn parse_mpls(data: &[u8]) -> Option<Vec<PlayItem>> {
    if data.len() < 16 || &data[0..4] != b"MPLS" {
        return None;
    }
    let pl_start = u32::from_be_bytes(data[8..12].try_into().ok()?) as usize;
    let pl = data.get(pl_start..)?;
    // PlayList section: length(4), reserved(2), number_of_PlayItems(2),
    // number_of_SubPaths(2), then the PlayItems.
    if pl.len() < 10 {
        return None;
    }
    let num_items = u16::from_be_bytes(pl[6..8].try_into().ok()?) as usize;
    let mut off = 10usize;
    let mut items = Vec::with_capacity(num_items);
    for _ in 0..num_items {
        let len = u16::from_be_bytes(pl.get(off..off + 2)?.try_into().ok()?) as usize;
        let item = pl.get(off + 2..off + 2 + len)?;
        // clip_information_file_name(5), clip_codec_id(4), flags(2), stc(1),
        // IN_time(4) @12, OUT_time(4) @16.
        if item.len() < 20 {
            return None;
        }
        let clip = std::str::from_utf8(&item[0..5]).ok()?.trim().to_string();
        let in_time = u32::from_be_bytes(item[12..16].try_into().ok()?);
        let out_time = u32::from_be_bytes(item[16..20].try_into().ok()?);
        items.push(PlayItem { clip, in_time, out_time });
        off += 2 + len;
    }
    Some(items)
}

/// True if the mounted disc looks AACS-encrypted (an AACS directory present).
/// The native SSIF path only works on decrypted / unencrypted discs.
pub fn is_encrypted(mount: &Path) -> bool {
    mount.join("AACS").is_dir() || mount.join("BDMV/AACS").is_dir()
}

/// Scan a mounted Blu-ray's playlists and return the 3D feature: the longest
/// playlist all of whose clips have an SSIF and a base m2ts on disk. `None` if
/// the disc has no 3D playlist (not a 3D BD, or a layout we don't recognise).
pub fn find_feature_3d(mount: &Path) -> Option<Feature> {
    let dir = mount.join("BDMV/PLAYLIST");
    let mut best: Option<Feature> = None;
    for entry in std::fs::read_dir(&dir).ok()?.flatten() {
        let path = entry.path();
        if !path.extension().is_some_and(|e| e.eq_ignore_ascii_case("mpls")) {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else { continue };
        let Some(items) = parse_mpls(&bytes) else { continue };
        if items.is_empty() {
            continue;
        }
        // Every clip must be a real 3D clip (has SSIF + base m2ts).
        let all_3d = items.iter().all(|pi| {
            mount.join("BDMV/STREAM/SSIF").join(format!("{}.ssif", pi.clip)).is_file()
                && mount.join("BDMV/STREAM").join(format!("{}.m2ts", pi.clip)).is_file()
        });
        if !all_3d {
            continue;
        }
        let duration_ticks: u64 = items.iter().map(PlayItem::duration_ticks).sum();
        let clips: Vec<String> = items.into_iter().map(|pi| pi.clip).collect();
        if best.as_ref().is_none_or(|b| duration_ticks > b.duration_ticks) {
            best = Some(Feature { clips, duration_ticks });
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal `.mpls` (header + PlayList section) with the given
    /// (clip, in, out) PlayItems, so the parser can be tested without a disc.
    fn synth_mpls(items: &[(&str, u32, u32)]) -> Vec<u8> {
        // PlayList section, built first so we know its size for the header.
        let mut pl = Vec::new();
        pl.extend_from_slice(&[0, 0, 0, 0]); // length (filled after)
        pl.extend_from_slice(&[0, 0]); // reserved
        pl.extend_from_slice(&(items.len() as u16).to_be_bytes()); // num_play_items
        pl.extend_from_slice(&[0, 0]); // num_sub_paths
        for (clip, in_t, out_t) in items {
            let mut it = Vec::new();
            it.extend_from_slice(clip.as_bytes()); // 5-byte clip name
            it.extend_from_slice(b"M2TS"); // codec
            it.extend_from_slice(&[0, 0]); // flags
            it.push(0); // stc
            it.extend_from_slice(&in_t.to_be_bytes());
            it.extend_from_slice(&out_t.to_be_bytes());
            // (No STN table etc. — the length lets the parser skip to the next.)
            pl.extend_from_slice(&(it.len() as u16).to_be_bytes());
            pl.extend_from_slice(&it);
        }
        let pl_len = (pl.len() - 4) as u32;
        pl[0..4].copy_from_slice(&pl_len.to_be_bytes());

        let pl_start = 40u32; // AppInfoPlayList would go at 40; we put PlayList there
        let mut d = Vec::new();
        d.extend_from_slice(b"MPLS");
        d.extend_from_slice(b"0200");
        d.extend_from_slice(&pl_start.to_be_bytes()); // PlayList_start_address
        d.extend_from_slice(&0u32.to_be_bytes()); // PlayListMark_start_address
        d.extend_from_slice(&0u32.to_be_bytes()); // ExtensionData_start_address
        d.resize(pl_start as usize, 0); // pad reserved region up to PlayList
        d.extend_from_slice(&pl);
        d
    }

    #[test]
    fn parses_clip_names_and_durations() {
        let d = synth_mpls(&[("00000", 90_000, 90_000 + 45_000 * 60), ("00001", 0, 45_000 * 30)]);
        let items = parse_mpls(&d).expect("parse");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].clip, "00000");
        assert_eq!(items[0].duration_ticks(), 45_000 * 60); // 60 s
        assert_eq!(items[1].clip, "00001");
        assert_eq!(items[1].duration_ticks(), 45_000 * 30);
    }

    #[test]
    fn rejects_non_mpls() {
        assert!(parse_mpls(b"not an mpls file at all").is_none());
        assert!(parse_mpls(b"MPLS").is_none()); // too short
    }
}
