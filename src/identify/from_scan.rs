// Bridge from raw makemkvcon parse output (rip::makemkv_parse::MakemkvScan)
// into the identify-layer types. Pure transformation: no I/O, no parsing
// of fields beyond what the scan layer already did.

use crate::identify::{
    DiscType, MakeMkvDiscInfo, StreamFingerprint, StreamKind, TitleFingerprint,
};
use crate::rip::makemkv_parse::{MakemkvScan, StreamAttributes, TitleAttributes};

/// Infer the disc type from a scan's content-type CINFO field, codec
/// strings, and codec IDs. Doesn't need filesystem access (so it works
/// on the scan output alone, before the disc is mounted), but the more
/// reliable signal is the filesystem layout — callers that have a mount
/// path should prefer `detect_disc_type_with_mount`.
pub fn detect_disc_type(scan: &MakemkvScan) -> DiscType {
    if let Some(ct) = scan.disc.content_type.as_deref() {
        let lower = ct.to_ascii_lowercase();
        if lower.contains("dvd") {
            return DiscType::Dvd;
        }
    }

    if scan_has_mvc_stream(scan) {
        return DiscType::BluRay3D;
    }
    if scan_has_hevc_stream(scan) {
        return DiscType::UltraHdBluRay;
    }
    DiscType::BluRay
}

/// Like `detect_disc_type`, but also consults the on-disc directory
/// layout. If `mount` carries any of the format-specific markers, those
/// win over scan-side inference. Used by the identify pipeline once a
/// disc is mounted.
pub fn detect_disc_type_with_mount(
    scan: &MakemkvScan,
    mount: &std::path::Path,
) -> DiscType {
    // UHD-specific marker: the AACS2 subdirectory.
    if mount.join("AACS2").is_dir() {
        return DiscType::UltraHdBluRay;
    }
    // 3D-BD-specific marker: the SSIF subdirectory.
    if mount.join("BDMV").join("STREAM").join("SSIF").is_dir() {
        return DiscType::BluRay3D;
    }
    // DVD-specific marker: VIDEO_TS without BDMV.
    if mount.join("VIDEO_TS").is_dir() && !mount.join("BDMV").is_dir() {
        return DiscType::Dvd;
    }
    detect_disc_type(scan)
}

fn scan_has_mvc_stream(scan: &MakemkvScan) -> bool {
    scan.titles.iter().any(|t| {
        t.streams.iter().any(|s| {
            let short = s.codec_short.as_deref().unwrap_or("");
            let long = s.codec_long.as_deref().unwrap_or("");
            short.contains("MVC") || long.contains("MVC")
        })
    })
}

fn scan_has_hevc_stream(scan: &MakemkvScan) -> bool {
    scan.titles.iter().any(|t| {
        t.streams.iter().any(|s| {
            let id = s.codec_id.as_deref().unwrap_or("");
            id.contains("HEVC") || id.contains("H265") || id.contains("MPEGH")
        })
    })
}

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
    fn jp_3d_disc_detected_as_blu_ray_3d_via_mvc_stream() {
        assert_eq!(detect_disc_type(&jp_scan()), DiscType::BluRay3D);
    }

    #[test]
    fn detect_disc_type_with_mount_recognises_uhd_via_aacs2() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("BDMV").join("STREAM")).unwrap();
        std::fs::create_dir_all(tmp.path().join("AACS2")).unwrap();
        // Empty scan still resolves UHD on the AACS2 evidence.
        let scan = crate::rip::makemkv_parse::MakemkvScan::default();
        assert_eq!(detect_disc_type_with_mount(&scan, tmp.path()), DiscType::UltraHdBluRay);
    }

    #[test]
    fn detect_disc_type_with_mount_recognises_3d_via_ssif() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("BDMV").join("STREAM").join("SSIF")).unwrap();
        let scan = crate::rip::makemkv_parse::MakemkvScan::default();
        assert_eq!(detect_disc_type_with_mount(&scan, tmp.path()), DiscType::BluRay3D);
    }

    #[test]
    fn detect_disc_type_with_mount_recognises_dvd_via_video_ts_only() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("VIDEO_TS")).unwrap();
        let scan = crate::rip::makemkv_parse::MakemkvScan::default();
        assert_eq!(detect_disc_type_with_mount(&scan, tmp.path()), DiscType::Dvd);
    }

    #[test]
    fn empty_scan_with_no_markers_defaults_to_blu_ray() {
        let scan = crate::rip::makemkv_parse::MakemkvScan::default();
        assert_eq!(detect_disc_type(&scan), DiscType::BluRay);
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
