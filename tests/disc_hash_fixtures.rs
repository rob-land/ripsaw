// Verify our content_hash() implementation against real TheDiscDB records.
// Each fixture is a disc whose published ContentHash and per-file size list
// were captured from github.com/TheDiscDb/data. See docs/disc-hash.md.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use threedrip::identify::disc_hash::{content_hash, DiscFile};

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
