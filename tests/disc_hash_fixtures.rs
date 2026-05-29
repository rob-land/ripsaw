// Verify our content_hash() implementation against real TheDiscDB records.
// Each fixture is a disc whose published ContentHash and per-file size list
// were captured from github.com/TheDiscDb/data. See docs/disc-hash.md.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use ripsaw::identify::disc_hash::{content_hash, enumerate_disc_files, DiscFile};

#[derive(Deserialize)]
struct Fixture {
    label: String,
    format: String,
    source: String,
    expected_hash: String,
    file_count: usize,
    files: Vec<FixtureFile>,
}

#[derive(Deserialize)]
struct FixtureFile {
    index: u32,
    name: String,
    size: u64,
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/disc_hash")
}

fn load_fixtures() -> Vec<Fixture> {
    let mut out = Vec::new();
    for entry in fs::read_dir(fixtures_dir()).expect("fixtures dir present") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let text = fs::read_to_string(&path).expect("fixture readable");
        let fx: Fixture = serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));
        out.push(fx);
    }
    out.sort_by(|a, b| a.label.cmp(&b.label));
    out
}

#[test]
fn fixtures_directory_has_diverse_corpus() {
    let fxs = load_fixtures();
    assert!(fxs.len() >= 5, "expected at least 5 fixtures, found {}", fxs.len());

    let formats: std::collections::BTreeSet<&str> =
        fxs.iter().map(|f| f.format.as_str()).collect();
    for required in ["uhd", "bluray", "dvd"] {
        assert!(formats.contains(required), "missing {required} fixture; have {formats:?}");
    }
}

#[test]
fn every_fixture_hashes_to_published_value() {
    let fxs = load_fixtures();
    let mut failures = Vec::new();
    for fx in &fxs {
        assert_eq!(
            fx.file_count, fx.files.len(),
            "{}: file_count mismatch in fixture file itself", fx.label
        );
        let files: Vec<DiscFile> = fx
            .files
            .iter()
            .map(|f| DiscFile { index: f.index, name: f.name.clone(), size: f.size })
            .collect();
        let got = content_hash(&files);
        if got != fx.expected_hash {
            failures.push(format!(
                "{} ({}, {} files, source {}): got {} expected {}",
                fx.label, fx.format, fx.file_count, fx.source, got, fx.expected_hash
            ));
        }
    }
    assert!(failures.is_empty(), "fixture failures:\n  {}", failures.join("\n  "));
}

/// End-to-end: synthesise each fixture's file tree on the filesystem as
/// sparse files, walk it with `enumerate_disc_files`, hash the result,
/// verify it matches the published `expected_hash`. This proves the
/// enumeration's sort order, directory selection, and extension filter
/// reproduce TheDiscDB's `ImportBuddy/DiskContentHash.HashMediaDisc`
/// behaviour on real disc structures.
#[test]
fn enumerate_then_hash_matches_published_value() {
    let fxs = load_fixtures();
    assert!(!fxs.is_empty());

    let mut failures = Vec::new();
    for fx in &fxs {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mount = tmp.path();

        // Reconstruct the on-disc layout. BD/UHD => BDMV/STREAM/, DVD => VIDEO_TS/.
        let target_dir = match fx.format.as_str() {
            "bluray" | "uhd" => mount.join("BDMV").join("STREAM"),
            "dvd" => mount.join("VIDEO_TS"),
            other => {
                failures.push(format!("{}: unknown format {other}", fx.label));
                continue;
            }
        };
        fs::create_dir_all(&target_dir).unwrap();
        for f in &fx.files {
            let path = target_dir.join(&f.name);
            let file = fs::File::create(&path).expect("create sparse file");
            file.set_len(f.size).expect("set_len on sparse file");
        }

        let walked = match enumerate_disc_files(mount) {
            Ok(v) => v,
            Err(e) => {
                failures.push(format!("{}: enumerate_disc_files failed: {e}", fx.label));
                continue;
            }
        };
        if walked.len() != fx.files.len() {
            failures.push(format!(
                "{}: walked {} files but fixture has {}",
                fx.label,
                walked.len(),
                fx.files.len()
            ));
            continue;
        }
        let got = content_hash(&walked);
        if got != fx.expected_hash {
            failures.push(format!(
                "{} ({}, {} files): walked hash {} != expected {}",
                fx.label, fx.format, fx.file_count, got, fx.expected_hash,
            ));
        }
    }
    assert!(failures.is_empty(), "end-to-end failures:\n  {}", failures.join("\n  "));
}
