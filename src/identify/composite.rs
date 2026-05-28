// Composite/constituent title detection from MakeMKV segment maps.
// See docs/identify.md § "Composite titles".

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TitleRelation {
    Atomic,
    Composite { constituents: Vec<u32> },
    Constituent { containers: Vec<u32> },
}

/// Parse a MakeMKV segment-map string into a set of segment identifiers.
///
/// Splits only on `+` and `,` — the two delimiters MakeMKV documents as
/// segment separators. Other characters (notably `/`, which appears inside
/// single-segment identifiers in some MakeMKV builds) are preserved so a
/// segment map like `"23/95"` becomes one opaque identifier, not two.
pub fn parse_segment_map(s: &str) -> BTreeSet<String> {
    s.split(|c: char| c == '+' || c == ',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Classify each title in `(index, segment_map)` order. Result has the same
/// length as the input. See docs/identify.md for the relation semantics.
pub fn analyze_relations(titles: &[(u32, &str)]) -> Vec<TitleRelation> {
    let sets: Vec<(u32, BTreeSet<String>)> = titles
        .iter()
        .map(|(idx, sm)| (*idx, parse_segment_map(sm)))
        .collect();

    sets.iter().map(|(idx, set)| classify(*idx, set, &sets)).collect()
}

fn classify(idx: u32, set: &BTreeSet<String>, all: &[(u32, BTreeSet<String>)]) -> TitleRelation {
    if set.is_empty() {
        return TitleRelation::Atomic;
    }
    let constituents: Vec<u32> = all
        .iter()
        .filter(|(i, s)| *i != idx && !s.is_empty() && s.is_subset(set) && s != set)
        .map(|(i, _)| *i)
        .collect();
    let containers: Vec<u32> = all
        .iter()
        .filter(|(i, s)| *i != idx && !s.is_empty() && set.is_subset(s) && s != set)
        .map(|(i, _)| *i)
        .collect();
    match (constituents.is_empty(), containers.is_empty()) {
        (true, true) => TitleRelation::Atomic,
        (false, _) => TitleRelation::Composite { constituents },
        (true, false) => TitleRelation::Constituent { containers },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plus_separated() {
        let s = parse_segment_map("1+2+3");
        assert_eq!(s.len(), 3);
        assert!(s.contains("1") && s.contains("2") && s.contains("3"));
    }

    #[test]
    fn keeps_internal_slashes_as_one_segment() {
        let s = parse_segment_map("23/95");
        assert_eq!(s.len(), 1);
        assert!(s.contains("23/95"));
    }

    #[test]
    fn empty_and_separator_only_yield_empty() {
        assert!(parse_segment_map("").is_empty());
        assert!(parse_segment_map("+,+,").is_empty());
    }

    #[test]
    fn disjoint_segment_sets_are_all_atomic() {
        let r = analyze_relations(&[(0, "1"), (1, "2"), (2, "3+4")]);
        assert!(matches!(r[0], TitleRelation::Atomic));
        assert!(matches!(r[1], TitleRelation::Atomic));
        assert!(matches!(r[2], TitleRelation::Atomic));
    }

    #[test]
    fn tv_series_pattern_yields_composite_plus_constituents() {
        let r = analyze_relations(&[(0, "1+2+3+4"), (1, "1"), (2, "2"), (3, "3"), (4, "4")]);
        match &r[0] {
            TitleRelation::Composite { constituents } => assert_eq!(constituents, &vec![1, 2, 3, 4]),
            other => panic!("title 0: expected composite, got {other:?}"),
        }
        for i in 1..=4 {
            match &r[i] {
                TitleRelation::Constituent { containers } => assert_eq!(containers, &vec![0]),
                other => panic!("title {i}: expected constituent, got {other:?}"),
            }
        }
    }

    #[test]
    fn identical_segment_maps_are_atomic_not_constituent() {
        // Neither is a *proper* subset of the other.
        let r = analyze_relations(&[(0, "1+2"), (1, "1+2")]);
        assert!(matches!(r[0], TitleRelation::Atomic));
        assert!(matches!(r[1], TitleRelation::Atomic));
    }
}
