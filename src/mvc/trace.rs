// JM ldecod trace comparison (docs/libmvc-poc.md § Validation).
//
// JM's reference decoder, built with TRACE=1 (scripts/build-ldecod-trace.sh),
// writes `trace_dec.txt` — one line per syntax element it decodes, in
// order. That gives a per-element ground truth for the macroblock decoder:
// emit the same `(name, value)` sequence and the FIRST divergence pinpoints
// exactly which element (and which MB) decodes wrong, instead of staring at
// a mismatched pixel block. This module parses the trace and finds that
// first divergence.
//
// Trace line shapes:
//   CABAC syntax element:  `@<n>   mb_type                    (  0)`
//   CAVLC header element:  `@<n>   SPS: profile_idc  01100100 (100)`
// The CABAC macroblock elements (no `Xyz:` prefix, no bit-pattern) are the
// ones the decode core produces and we compare against.

/// One decoded syntax element from the trace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceElement {
    /// The running symbol index (CABAC) or bit position (CAVLC) after `@`.
    pub index: u64,
    pub name: String,
    pub value: i64,
    /// Second value, for residual run/level lines (`@N name <level> <run>`,
    /// no parentheses): `value` holds the level, `value2` the run. `None`
    /// for ordinary `(value)` elements.
    pub value2: Option<i64>,
    /// True for the CAVLC header elements (`SPS:`/`PPS:`/`SH:` …) — these
    /// carry a `Foo:` group prefix and a bit-pattern; the macroblock
    /// comparison skips them.
    pub is_header: bool,
}

/// Parse a JM `trace_dec.txt` into its syntax-element sequence.
pub fn parse_trace(text: &str) -> Vec<TraceElement> {
    text.lines().filter_map(parse_line).collect()
}

fn parse_line(line: &str) -> Option<TraceElement> {
    let rest = line.strip_prefix('@')?;
    // Index is the leading integer.
    let idx_end = rest.find(|c: char| !c.is_ascii_digit())?;
    let index: u64 = rest[..idx_end].parse().ok()?;
    let body = &rest[idx_end..];

    if let Some(open) = body.rfind('(') {
        // `(value)` form — header / macroblock-header element.
        let close = body.rfind(')')?;
        if close < open {
            return None;
        }
        let value: i64 = body[open + 1..close].trim().parse().ok()?;
        let mut name_field = body[..open].trim().to_string();
        let is_header = name_field.contains(':');
        if is_header {
            if let Some(sp) = name_field.rsplit_once(char::is_whitespace) {
                if !sp.1.is_empty() && sp.1.bytes().all(|b| b == b'0' || b == b'1') {
                    name_field = sp.0.trim_end().to_string();
                }
            }
        }
        Some(TraceElement { index, name: name_field, value, value2: None, is_header })
    } else {
        // Residual run/level form: `name  <level>  <run>` (two trailing
        // integers, no parens). Names are like "Luma sng "/"Luma lev ".
        let trimmed = body.trim_end();
        let run_str = trimmed.rsplit(char::is_whitespace).next()?;
        let run: i64 = run_str.parse().ok()?;
        let before_run = trimmed[..trimmed.len() - run_str.len()].trim_end();
        let level_str = before_run.rsplit(char::is_whitespace).next()?;
        let level: i64 = level_str.parse().ok()?;
        let name = before_run[..before_run.len() - level_str.len()].trim().to_string();
        if name.is_empty() {
            return None;
        }
        Some(TraceElement { index, name, value: level, value2: Some(run), is_header: false })
    }
}

/// The non-header (CABAC macroblock) elements — what the decode core emits.
pub fn macroblock_elements(elems: &[TraceElement]) -> Vec<&TraceElement> {
    elems.iter().filter(|e| !e.is_header).collect()
}

/// Result of comparing the decoder's output against the reference trace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Comparison {
    /// Every compared element matched (up to the shorter of the two).
    Match { count: usize },
    /// First differing element: position in the compared sequence, the
    /// reference `(name, value)`, and what the decoder produced.
    Diverged {
        position: usize,
        reference: (String, i64),
        decoded: (String, i64),
    },
    /// Sequences matched as far as they went but differ in length.
    LengthMismatch { common: usize, reference_len: usize, decoded_len: usize },
}

/// Compare the decoder's `(name, value)` sequence against the reference
/// macroblock-element sequence, returning the first divergence.
pub fn first_divergence(reference: &[&TraceElement], decoded: &[(String, i64)]) -> Comparison {
    for (i, (r, d)) in reference.iter().zip(decoded.iter()).enumerate() {
        if r.name != d.0 || r.value != d.1 {
            return Comparison::Diverged {
                position: i,
                reference: (r.name.clone(), r.value),
                decoded: (d.0.clone(), d.1),
            };
        }
    }
    if reference.len() != decoded.len() {
        Comparison::LengthMismatch {
            common: reference.len().min(decoded.len()),
            reference_len: reference.len(),
            decoded_len: decoded.len(),
        }
    } else {
        Comparison::Match { count: reference.len() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
Annex B NALU w/ long startcode, len 52, forbidden_bit 0, nal_reference_idc 3, nal_unit_type 7
@0     SPS: profile_idc                                       01100100 (100)
@16    SPS: level_idc                                         00101001 ( 41)
@0      mb_type                                                         (  0)
@1      transform_size_8x8_flag                                         (  1)
@2      intra4x4_pred_mode                                              ( -1)
@7      coded_block_pattern                                             (  1)
@8      mb_qp_delta                                                     (  0)
@9      Luma sng                                                   -3    2
";

    #[test]
    fn parses_header_and_cabac_elements() {
        let elems = parse_trace(SAMPLE);
        assert_eq!(elems.len(), 8);

        // CAVLC header: name has the bit-pattern stripped, flagged header.
        let sps = &elems[0];
        assert_eq!(sps.name, "SPS: profile_idc");
        assert_eq!(sps.value, 100);
        assert!(sps.is_header);

        // CABAC MB element.
        let mbt = &elems[2];
        assert_eq!(mbt.name, "mb_type");
        assert_eq!(mbt.value, 0);
        assert!(!mbt.is_header);

        // Negative value parses.
        assert_eq!(elems[4].value, -1);
    }

    #[test]
    fn parses_residual_run_level_line() {
        // `@9 Luma sng  -3  2` -> name "Luma sng", level -3, run 2.
        let elems = parse_trace(SAMPLE);
        let res = elems.last().unwrap();
        assert_eq!(res.name, "Luma sng");
        assert_eq!(res.value, -3); // level
        assert_eq!(res.value2, Some(2)); // run
        assert!(!res.is_header);
    }

    #[test]
    fn macroblock_elements_drops_headers() {
        let elems = parse_trace(SAMPLE);
        let mb = macroblock_elements(&elems);
        assert_eq!(mb.len(), 6); // 2 headers dropped, residual kept
        assert_eq!(mb[0].name, "mb_type");
    }

    #[test]
    fn first_divergence_pinpoints_mismatch() {
        let elems = parse_trace(SAMPLE);
        let mb = macroblock_elements(&elems);

        // Exact match up to the available decoded elements.
        let good: Vec<(String, i64)> = mb.iter().map(|e| (e.name.clone(), e.value)).collect();
        assert_eq!(first_divergence(&mb, &good), Comparison::Match { count: 6 });

        // Diverge at element 3 (coded_block_pattern value wrong).
        let mut bad = good.clone();
        bad[3].1 = 9;
        match first_divergence(&mb, &bad) {
            Comparison::Diverged { position, reference, decoded } => {
                assert_eq!(position, 3);
                assert_eq!(reference, ("coded_block_pattern".into(), 1));
                assert_eq!(decoded, ("coded_block_pattern".into(), 9));
            }
            other => panic!("expected divergence, got {other:?}"),
        }

        // Length mismatch when the decoder stops early.
        assert_eq!(
            first_divergence(&mb, &good[..3]),
            Comparison::LengthMismatch { common: 3, reference_len: 6, decoded_len: 3 }
        );
    }
}
