// Verify composite-detection against captured + hand-crafted scan fixtures.
// See docs/identify.md § "Composite titles".

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use threedrip::identify::composite::{analyze_relations, TitleRelation};

#[derive(Deserialize)]
struct Fixture {
    label: String,
    titles: Vec<FixtureTitle>,
    expected_relations: BTreeMap<String, ExpectedRelation>,
}

#[derive(Deserialize)]
struct FixtureTitle {
    index: u32,
    segment_map: String,
}

#[derive(Deserialize, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ExpectedRelation {
    Atomic,
    Composite { constituents: Vec<u32> },
    Constituent { containers: Vec<u32> },
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/title_scan")
}

fn load_all() -> Vec<(PathBuf, Fixture)> {
    let mut out: Vec<_> = fs::read_dir(fixtures_dir())
        .expect("fixtures dir exists")
        .filter_map(|e| {
            let path = e.unwrap().path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                return None;
            }
            let fx: Fixture = serde_json::from_str(&fs::read_to_string(&path).unwrap())
                .unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));
            Some((path, fx))
        })
        .collect();
    out.sort_by(|a, b| a.1.label.cmp(&b.1.label));
    out
}

#[test]
fn fixture_corpus_covers_atomic_composite_and_constituent_cases() {
    let fixtures = load_all();
    assert!(fixtures.len() >= 3, "need >=3 fixtures, found {}", fixtures.len());

    let mut has_atomic = false;
    let mut has_composite = false;
    let mut has_constituent = false;
    for (_, fx) in &fixtures {
        for r in fx.expected_relations.values() {
            match r {
                ExpectedRelation::Atomic => has_atomic = true,
                ExpectedRelation::Composite { .. } => has_composite = true,
                ExpectedRelation::Constituent { .. } => has_constituent = true,
            }
        }
    }
    assert!(has_atomic, "no atomic case in corpus");
    assert!(has_composite, "no composite case in corpus");
    assert!(has_constituent, "no constituent case in corpus");
}

#[test]
fn every_fixture_classification_matches() {
    let mut failures = Vec::new();
    for (_, fx) in load_all() {
        let pairs: Vec<(u32, &str)> = fx
            .titles
            .iter()
            .map(|t| (t.index, t.segment_map.as_str()))
            .collect();
        let actual = analyze_relations(&pairs);
        for (t, computed) in fx.titles.iter().zip(actual.iter()) {
            let key = t.index.to_string();
            let expected = fx
                .expected_relations
                .get(&key)
                .unwrap_or_else(|| panic!("{}: missing expected_relations[\"{}\"]", fx.label, key));
            if !same(computed, expected) {
                failures.push(format!(
                    "{} title {}: expected {:?}, got {:?}",
                    fx.label, t.index, expected, computed
                ));
            }
        }
    }
    assert!(failures.is_empty(), "fixture mismatches:\n  {}", failures.join("\n  "));
}

fn same(actual: &TitleRelation, expected: &ExpectedRelation) -> bool {
    match (actual, expected) {
        (TitleRelation::Atomic, ExpectedRelation::Atomic) => true,
        (TitleRelation::Composite { constituents: a }, ExpectedRelation::Composite { constituents: e }) => a == e,
        (TitleRelation::Constituent { containers: a }, ExpectedRelation::Constituent { containers: e }) => a == e,
        _ => false,
    }
}
