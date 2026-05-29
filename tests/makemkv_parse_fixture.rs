// End-to-end test: take a captured `makemkvcon -r info` run from a real
// Blu-ray 3D disc and assert that the parser + aggregator produces the
// expected disc-, title-, and stream-level attributes.

use std::fs;
use std::path::{Path, PathBuf};

use ripsaw::rip::makemkv_parse::{aggregate, to_makemkv_scan};

fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/makemkv_scan/jurassic_park_2013_3d.raw.txt")
}

fn load_scan() -> ripsaw::rip::makemkv_parse::MakemkvScan {
    let raw = fs::read_to_string(fixture_path()).expect("fixture present");
    let records = aggregate(&raw);
    to_makemkv_scan(&records)
}

#[test]
fn aggregator_recovers_disc_level_attributes() {
    let scan = load_scan();
    assert_eq!(scan.disc.name.as_deref(), Some("Jurassic Park 3D"));
    assert_eq!(scan.disc.volume_label.as_deref(), Some("JURASSIC_PARK_3D_G74"));
    assert_eq!(scan.disc.language_code.as_deref(), Some("eng"));
    assert_eq!(scan.disc.language_name.as_deref(), Some("English"));
    assert_eq!(scan.disc.content_type.as_deref(), Some("Blu-ray disc"));
}

#[test]
fn aggregator_parses_makemkv_version() {
    let scan = load_scan();
    assert_eq!(scan.makemkv_version.as_deref(), Some("1.17.8"));
}

#[test]
fn aggregator_finds_six_titles() {
    let scan = load_scan();
    assert_eq!(scan.titles.len(), 6);
    for (i, t) in scan.titles.iter().enumerate() {
        assert_eq!(t.index, i as u32, "titles should be in index order");
    }
}

#[test]
fn aggregator_decodes_main_feature_title_0() {
    let scan = load_scan();
    let t0 = &scan.titles[0];
    assert_eq!(t0.name.as_deref(), Some("Jurassic Park 3D"));
    assert_eq!(t0.chapter_count, Some(20));
    assert_eq!(t0.duration_seconds, Some(2 * 3600 + 6 * 60 + 42));
    assert_eq!(t0.duration_text.as_deref(), Some("2:06:42"));
    assert_eq!(t0.size_bytes, Some(43_274_268_672));
    assert_eq!(t0.display_size.as_deref(), Some("40.3 GB"));
    assert_eq!(t0.source_file.as_deref(), Some("00803.mpls"));
    assert_eq!(t0.segment_count, Some(1));
    assert_eq!(t0.segment_map.as_deref(), Some("23/95"));
    assert_eq!(t0.output_file.as_deref(), Some("Jurassic Park 3D_t00.mkv"));
    assert_eq!(t0.language_code.as_deref(), Some("eng"));
}

#[test]
fn aggregator_decodes_atomic_m2ts_title_3() {
    let scan = load_scan();
    let t3 = &scan.titles[3];
    assert_eq!(t3.duration_seconds, Some(59));
    assert_eq!(t3.source_file.as_deref(), Some("00016.m2ts"));
    assert_eq!(t3.segment_map.as_deref(), Some("16"));
}

#[test]
fn aggregator_emits_streams_per_title() {
    let scan = load_scan();
    // Every title has at least one video stream; the main feature carries
    // many audio + subtitle tracks. We don't assert exact counts because
    // makemkvcon's stream numbering varies, but we do require that the
    // captured stream attributes look stream-like.
    let t0 = &scan.titles[0];
    assert!(t0.streams.len() > 1, "main feature should have multiple streams, got {}", t0.streams.len());

    let video = t0.streams.iter().find(|s| s.kind.as_deref() == Some("Video"));
    let video = video.expect("title 0 should have a video stream");
    assert_eq!(video.codec_id.as_deref(), Some("V_MPEG4/ISO/AVC"));
    assert_eq!(video.video_size.as_deref(), Some("1920x1080"));

    let audio_count = t0.streams.iter().filter(|s| s.kind.as_deref() == Some("Audio")).count();
    assert!(audio_count >= 1, "main feature should have ≥1 audio track");
}

#[test]
fn aggregator_collects_diagnostic_messages() {
    let scan = load_scan();
    // The captured scan contains a "Using direct disc access mode" info MSG
    // and several "title skipped because too short" warnings.
    assert!(!scan.messages.is_empty());
    let any_skipped = scan.messages.iter().any(|m| m.text.contains("was therefore skipped"));
    assert!(any_skipped, "expected at least one 'title skipped' MSG record");
}

#[test]
fn aggregator_handles_multiline_quoted_messages() {
    // The captured fixture contains the multi-line AnyDVD warning, which
    // tests the `\<LF>` continuation-folding path in `aggregate`. If folding
    // broke we'd either see a malformed record or lose subsequent records.
    let scan = load_scan();
    let has_anydvd_msg = scan.messages.iter().any(|m| m.text.contains("AnyDVD"));
    assert!(has_anydvd_msg, "AnyDVD multi-line MSG should be parsed intact");

    // And we should still have all six titles after the multi-line record,
    // proving the parser kept its place in the stream.
    assert_eq!(scan.titles.len(), 6);
}
