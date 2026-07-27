// Extract a real MVCDecoderConfigurationRecord from one of the sample
// MKVs in samples/ and run the full mvcC -> Subset SPS -> SpsMvcExtension
// chain on it. Skips gracefully when the sample isn't available (e.g.
// running tests on CI without the local sample collection).
//
// On the developer's machine this is the strongest validation we can do
// short of integrating with a working H.264 base-view decoder, because
// it exercises every piece we've built (EBML walker, mvcC parser, NAL
// header, RBSP extraction) end-to-end on a bitstream MakeMKV produced
// from a real 3D Blu-ray.

use std::fs::File;
use std::path::{Path, PathBuf};

use libmvc::ebml::EbmlReader;
use libmvc::mvcc::{find_mvcc_bytes, parse, MvcDecoderConfigurationRecord};
use libmvc::nal::{parse_nal_unit_header, NAL_SUBSET_SPS};
use libmvc::rbsp::extract_rbsp;
use libmvc::sps::parse_subset_sps_rbsp;

fn sample_mkv() -> Option<PathBuf> {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("samples/3D_LR_Pattern.mkv");
    if path.is_file() {
        Some(path)
    } else {
        None
    }
}

fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/mvc/3d_lr_pattern.mvcc.bin")
}

fn extract_or_load_mvcc_bytes() -> Option<Vec<u8>> {
    // Prefer the captured fixture so tests still pass even if the sample
    // is moved or absent.
    if let Ok(bytes) = std::fs::read(fixture_path()) {
        return Some(bytes);
    }
    let mkv = sample_mkv()?;
    let file = File::open(&mkv).ok()?;
    let mut reader = EbmlReader::new(file);
    let bytes = find_mvcc_bytes(&mut reader).ok().flatten()?;
    // Capture the extracted bytes as a fixture for future runs.
    if let Some(parent) = fixture_path().parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(fixture_path(), &bytes);
    Some(bytes)
}

#[test]
fn real_world_mvcc_header_looks_like_multiview_high_at_level_4_1() {
    let Some(bytes) = extract_or_load_mvcc_bytes() else {
        eprintln!("sample MKV not available; skipping");
        return;
    };
    let record: MvcDecoderConfigurationRecord = parse(&bytes).expect("mvcC parse");

    eprintln!("configurationVersion = {}", record.configuration_version);
    eprintln!("AVCProfileIndication = {}", record.avc_profile_indication);
    eprintln!("profile_compatibility = {}", record.profile_compatibility);
    eprintln!("AVCLevelIndication = {}", record.avc_level_indication);
    eprintln!("lengthSizeMinusOne = {}", record.length_size_minus_one);
    eprintln!("SPS NALs = {}", record.sps_nals.len());
    eprintln!("PPS NALs = {}", record.pps_nals.len());

    assert_eq!(record.configuration_version, 1);
    // Multiview High profile = 118, Stereo High profile = 128. Either is
    // plausible for a 3D BD source.
    assert!(
        matches!(record.avc_profile_indication, 118 | 128 | 134),
        "expected one of the multiview profile_idc values, got {}",
        record.avc_profile_indication
    );
    // Level 4.1 = 0x29 = 41. Real 1080p24 BD is usually 4.1.
    assert!(
        record.avc_level_indication >= 30,
        "level_idc {} suspiciously low for HD source",
        record.avc_level_indication
    );
    assert!(!record.sps_nals.is_empty(), "expected at least one SPS NAL");
}

#[test]
fn at_least_one_sps_nal_in_real_world_mvcc_is_a_subset_sps() {
    let Some(bytes) = extract_or_load_mvcc_bytes() else {
        return;
    };
    let record = parse(&bytes).expect("mvcC parse");

    let mut found_subset_sps = false;
    for (i, nal_bytes) in record.sps_nals.iter().enumerate() {
        let (header, consumed) =
            parse_nal_unit_header(nal_bytes).expect("NAL header parse");
        eprintln!(
            "SPS NAL #{i}: nal_unit_type = {} (forbidden={}, ref_idc={}, length={})",
            header.nal_unit_type,
            header.forbidden_zero_bit,
            header.nal_ref_idc,
            nal_bytes.len(),
        );
        if header.nal_unit_type == NAL_SUBSET_SPS {
            found_subset_sps = true;
            // Strip the NAL header and the emulation-prevention bytes,
            // then look at the first few bytes of the RBSP -- they
            // should be plausible for the start of a base SPS (which
            // begins with profile_idc as 8 unsigned bits).
            let rbsp = extract_rbsp(&nal_bytes[consumed..]);
            assert!(!rbsp.is_empty(), "subset SPS RBSP must not be empty");
            eprintln!(
                "  RBSP first bytes: {:02X?}",
                &rbsp[..rbsp.len().min(8)]
            );
        }
    }
    assert!(
        found_subset_sps,
        "no NAL of type {} (Subset SPS) found in the SPS list",
        NAL_SUBSET_SPS
    );
}

#[test]
fn real_world_subset_sps_fully_decodes_to_two_view_1080p() {
    // The strongest end-to-end parser check: take the real mvcC's Subset
    // SPS and decode the *whole* RBSP -- base seq_parameter_set_data()
    // (geometry, POC, VUI/HRD) plus the MVC extension (view count / refs)
    // -- not just the first byte. Validates the new base-SPS parser
    // against a bitstream a real 3D Blu-ray produced.
    let Some(bytes) = extract_or_load_mvcc_bytes() else {
        eprintln!("sample/fixture not available; skipping");
        return;
    };
    let record = parse(&bytes).expect("mvcC parse");

    let mut decoded = false;
    for nal_bytes in &record.sps_nals {
        let (header, consumed) =
            parse_nal_unit_header(nal_bytes).expect("NAL header parse");
        if header.nal_unit_type != NAL_SUBSET_SPS {
            continue;
        }
        let rbsp = extract_rbsp(&nal_bytes[consumed..]);
        let subset = parse_subset_sps_rbsp(&rbsp).expect("subset SPS decode");
        eprintln!(
            "decoded subset SPS: profile={} level={} {}x{} views={} view_ids={:?}",
            subset.sps.profile_idc,
            subset.sps.level_idc,
            subset.sps.width,
            subset.sps.height,
            subset.mvc.num_views_minus1 + 1,
            subset.mvc.view_id,
        );

        // Multiview/Stereo High family.
        assert!(matches!(subset.sps.profile_idc, 118 | 128 | 134));
        // 1080p Blu-ray 3D: 1920x1080, level 4.1.
        assert_eq!((subset.sps.width, subset.sps.height), (1920, 1080));
        assert_eq!(subset.sps.level_idc, 41);
        assert_eq!(subset.sps.chroma_format_idc, 1); // 4:2:0
        // Two views, dependent view references the base view inter-view.
        assert_eq!(subset.mvc.num_views_minus1, 1);
        assert_eq!(subset.mvc.view_id.len(), 2);
        assert_eq!(subset.mvc.anchor_refs[1].l0, vec![subset.mvc.view_id[0]]);
        decoded = true;
        break;
    }
    assert!(decoded, "no Subset SPS decoded from the real-world mvcC");
}
