// Bridge from raw makemkvcon parse output (rip::makemkv_parse::MakemkvScan)
// into the identify-layer types. Pure transformation: no I/O, no parsing
// of fields beyond what the scan layer already did.

use crate::identify::{
    MakeMkvDiscInfo, StreamFingerprint, StreamKind, TitleFingerprint,
};
use crate::rip::makemkv_parse::{MakemkvScan, StreamAttributes, TitleAttributes};

impl From<&MakemkvScan> for MakeMkvDiscInfo {
    fn from(scan: &MakemkvScan) -> Self {
        MakeMkvDiscInfo {
            name: scan.disc.name.clone(),
            comment: None,
            language_code: scan.disc.language_code.clone(),
            content_type: scan.disc.content_type.clone(),
            year: None,
        }
    }
}

/// Project every title from the scan into a `TitleFingerprint`. Fields that
/// MakeMKV omits become defaults (empty strings, `0`s, empty stream list).
pub fn title_fingerprints(scan: &MakemkvScan) -> Vec<TitleFingerprint> {
    scan.titles.iter().map(title_fingerprint_from).collect()
}

fn title_fingerprint_from(t: &TitleAttributes) -> TitleFingerprint {
    TitleFingerprint {
        index: t.index,
        duration_seconds: t.duration_seconds.unwrap_or(0),
        size_bytes: t.size_bytes.unwrap_or(0),
        source_file: t.source_file.clone().unwrap_or_default(),
        segment_map: t.segment_map.clone().unwrap_or_default(),
        chapter_count: t.chapter_count.unwrap_or(0),
        streams: t.streams.iter().map(stream_fingerprint_from).collect(),
    }
}

fn stream_fingerprint_from(s: &StreamAttributes) -> StreamFingerprint {
    StreamFingerprint {
        index: s.stream,
        kind: stream_kind_from_label(s.kind.as_deref()),
        codec: s.codec_id.clone().unwrap_or_default(),
        language_code: s.language_code.clone(),
        channels: s.channels.map(|c| c.min(u8::MAX as u32) as u8),
        title: s.name.clone(),
    }
}

fn stream_kind_from_label(label: Option<&str>) -> StreamKind {
    // MakeMKV CINFO/SINFO code 1 returns localised strings, but for the
    // English locale we configure these are stable.
    match label {
        Some("Video") => StreamKind::Video,
        Some("Audio") => StreamKind::Audio,
        Some("Subtitles") | Some("Subtitle") => StreamKind::Subtitle,
        _ => StreamKind::Video,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rip::makemkv_parse::{aggregate, to_makemkv_scan};

    fn jp_scan() -> MakemkvScan {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/makemkv_scan/jurassic_park_2013_3d.raw.txt");
        let raw = std::fs::read_to_string(path).expect("fixture readable");
        to_makemkv_scan(&aggregate(&raw))
    }

    #[test]
    fn disc_info_carries_name_and_content_type() {
        let info = MakeMkvDiscInfo::from(&jp_scan());
        assert_eq!(info.name.as_deref(), Some("Jurassic Park 3D"));
        assert_eq!(info.content_type.as_deref(), Some("Blu-ray disc"));
        assert_eq!(info.language_code.as_deref(), Some("eng"));
    }

    #[test]
    fn six_title_fingerprints_with_expected_main_feature() {
        let fps = title_fingerprints(&jp_scan());
        assert_eq!(fps.len(), 6);

        let t0 = &fps[0];
        assert_eq!(t0.index, 0);
        assert_eq!(t0.duration_seconds, 7602);
        assert_eq!(t0.size_bytes, 43_274_268_672);
        assert_eq!(t0.source_file, "00803.mpls");
        assert_eq!(t0.segment_map, "23/95");
        assert_eq!(t0.chapter_count, 20);
        assert!(!t0.streams.is_empty(), "main feature should have streams");

        let video = t0.streams.iter().find(|s| s.kind == StreamKind::Video).expect("video stream");
        assert_eq!(video.codec, "V_MPEG4/ISO/AVC");
    }

    #[test]
    fn stream_kind_label_recognition() {
        assert_eq!(stream_kind_from_label(Some("Video")), StreamKind::Video);
        assert_eq!(stream_kind_from_label(Some("Audio")), StreamKind::Audio);
        assert_eq!(stream_kind_from_label(Some("Subtitles")), StreamKind::Subtitle);
        assert_eq!(stream_kind_from_label(Some("Subtitle")), StreamKind::Subtitle);
        assert_eq!(stream_kind_from_label(None), StreamKind::Video);
        assert_eq!(stream_kind_from_label(Some("unknown")), StreamKind::Video);
    }
}
