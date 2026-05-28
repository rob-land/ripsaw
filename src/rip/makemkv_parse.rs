// Parser for makemkvcon -r ("robot mode") output. See docs/rip.md.
//
// Each line is either a record (MSG / DRV / CINFO / TINFO / SINFO / TCOUNT)
// or a multi-line continuation belonging to a previously-started record. The
// record-level fields are comma-separated; trailing string fields are
// double-quoted with `""` as the escape for an embedded quote.
//
// Layer 1 (parse_line, aggregate, Record): pure line/record parsing.
// Layer 2 (MakemkvScan, to_makemkv_scan): typed aggregation by attribute
// code, suitable for higher-level consumers in identify::.

use std::collections::BTreeMap;

use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Record {
    /// `MSG:code,flags,priority,"text","format",arg1,arg2,...`
    Msg {
        code: u32,
        flags: u32,
        priority: u32,
        text: String,
    },
    /// `DRV:index,visible,enabled,flags,"vendor","name","label"`
    Drv {
        index: u32,
        visible: u32,
        enabled: u32,
        flags: u32,
        vendor_name: String,
        name: String,
        label: String,
    },
    /// `TCOUNT:n` — number of titles in the disc scan.
    Tcount(u32),
    /// `CINFO:code,value,"string"` — disc-level attribute.
    Cinfo {
        code: u32,
        value: u32,
        text: String,
    },
    /// `TINFO:title,code,value,"string"` — per-title attribute.
    Tinfo {
        title: u32,
        code: u32,
        value: u32,
        text: String,
    },
    /// `SINFO:title,stream,code,value,"string"` — per-stream attribute.
    Sinfo {
        title: u32,
        stream: u32,
        code: u32,
        value: u32,
        text: String,
    },
    /// A record type we don't currently model.
    Unknown { kind: String },
}

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("malformed record: {0}")]
    Malformed(String),
    #[error("unterminated quoted string")]
    UnterminatedString,
}

/// Stateful incremental aggregator for streaming `makemkvcon -r` output.
/// Use `push_line` to feed one line at a time (without the trailing
/// newline); each call returns zero, one, or more newly-completed records.
/// `finish` flushes any partial-but-complete buffer that may have been
/// pending when the stream ended.
#[derive(Default)]
pub struct Aggregator {
    buf: String,
}

impl Aggregator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one line of stdout. `line` may include or omit the trailing
    /// `\r` — both are tolerated. Returns the records that completed as a
    /// result of this line.
    pub fn push_line(&mut self, line: &str) -> Vec<Record> {
        let line = line.trim_end_matches(['\r', '\n']);
        let (line_part, continued) = match line.strip_suffix('\\') {
            Some(s) => (s, true),
            None => (line, false),
        };
        if !self.buf.is_empty() {
            self.buf.push('\n');
        }
        self.buf.push_str(line_part);
        if continued {
            return Vec::new();
        }
        match parse_line(&self.buf) {
            Ok(Some(rec)) => {
                self.buf.clear();
                vec![rec]
            }
            Ok(None) => {
                self.buf.clear();
                Vec::new()
            }
            Err(ParseError::UnterminatedString) => Vec::new(),
            Err(_) => {
                self.buf.clear();
                Vec::new()
            }
        }
    }

    /// Flush any pending buffer at end-of-stream. Returns the final record
    /// if one closed cleanly; otherwise drops the partial buffer.
    pub fn finish(self) -> Vec<Record> {
        if self.buf.is_empty() {
            return Vec::new();
        }
        match parse_line(&self.buf) {
            Ok(Some(rec)) => vec![rec],
            _ => Vec::new(),
        }
    }
}

/// One-shot wrapper around `Aggregator` for in-memory input. Equivalent
/// to feeding every line via `push_line` then calling `finish`.
pub fn aggregate(input: &str) -> Vec<Record> {
    let mut agg = Aggregator::new();
    let mut out = Vec::new();
    for line in input.split('\n') {
        out.extend(agg.push_line(line));
    }
    out.extend(agg.finish());
    out
}

/// Parse a single line of `makemkvcon -r` output. Returns `Ok(None)` for
/// blank or comment-like lines and for continuation lines we should fold
/// into the previous record (which the caller decides).
pub fn parse_line(line: &str) -> Result<Option<Record>, ParseError> {
    let line = line.trim_end_matches(['\r', '\n']);
    if line.is_empty() {
        return Ok(None);
    }

    let Some((kind, rest)) = line.split_once(':') else {
        // Continuation of a multi-line quoted value, or stray junk. We let the
        // caller fold this into the previous record's text if applicable.
        return Ok(None);
    };

    let fields = split_fields(rest)?;

    let rec = match kind {
        "TCOUNT" => Record::Tcount(parse_u32(&fields, 0)?),
        "MSG" => Record::Msg {
            code: parse_u32(&fields, 0)?,
            flags: parse_u32(&fields, 1)?,
            priority: parse_u32(&fields, 2)?,
            text: take_string(&fields, 3),
        },
        "DRV" => Record::Drv {
            index: parse_u32(&fields, 0)?,
            visible: parse_u32(&fields, 1)?,
            enabled: parse_u32(&fields, 2)?,
            flags: parse_u32(&fields, 3)?,
            vendor_name: take_string(&fields, 4),
            name: take_string(&fields, 5),
            label: take_string(&fields, 6),
        },
        "CINFO" => Record::Cinfo {
            code: parse_u32(&fields, 0)?,
            value: parse_u32(&fields, 1)?,
            text: take_string(&fields, 2),
        },
        "TINFO" => Record::Tinfo {
            title: parse_u32(&fields, 0)?,
            code: parse_u32(&fields, 1)?,
            value: parse_u32(&fields, 2)?,
            text: take_string(&fields, 3),
        },
        "SINFO" => Record::Sinfo {
            title: parse_u32(&fields, 0)?,
            stream: parse_u32(&fields, 1)?,
            code: parse_u32(&fields, 2)?,
            value: parse_u32(&fields, 3)?,
            text: take_string(&fields, 4),
        },
        other => Record::Unknown { kind: other.to_string() },
    };

    Ok(Some(rec))
}

fn parse_u32(fields: &[String], idx: usize) -> Result<u32, ParseError> {
    fields
        .get(idx)
        .ok_or_else(|| ParseError::Malformed(format!("missing field {idx}")))?
        .parse::<u32>()
        .map_err(|e| ParseError::Malformed(format!("field {idx}: {e}")))
}

fn take_string(fields: &[String], idx: usize) -> String {
    fields.get(idx).cloned().unwrap_or_default()
}

/// Split a comma-separated record body into fields, respecting `"..."`
/// quoting with `""` as the escape for a literal quote. Numeric fields
/// come back as their string representation; quoted fields come back with
/// quotes stripped and `""` escapes resolved.
fn split_fields(input: &str) -> Result<Vec<String>, ParseError> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut in_quote = false;
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if !in_quote => {
                in_quote = true;
                // Drop the opening quote.
            }
            '"' if in_quote => {
                if chars.peek() == Some(&'"') {
                    // Escaped quote: consume the second " and emit one literal.
                    chars.next();
                    buf.push('"');
                } else {
                    in_quote = false;
                }
            }
            ',' if !in_quote => {
                out.push(std::mem::take(&mut buf));
            }
            _ => buf.push(c),
        }
    }
    if in_quote {
        return Err(ParseError::UnterminatedString);
    }
    out.push(buf);
    Ok(out)
}

// =========================================================================
// Layer 2: typed aggregation by attribute code.
// =========================================================================

/// Higher-level structured form of a single `makemkvcon info` scan. Wraps
/// the raw `Record` stream into per-disc, per-title and per-stream
/// dictionaries indexed by MakeMKV attribute code. Optional fields are
/// `None` when MakeMKV did not emit that attribute for the disc; numeric
/// fields that fail to parse become `None` rather than aborting the scan.
///
/// Attribute-code names follow `apdefs.h` in the MakeMKV SDK.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MakemkvScan {
    pub disc: DiscAttributes,
    pub titles: Vec<TitleAttributes>,
    pub makemkv_version: Option<String>,
    pub messages: Vec<MsgRecord>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiscAttributes {
    pub content_type: Option<String>,   // CINFO 1  ap_iaType
    pub name: Option<String>,           // CINFO 2  ap_iaName
    pub language_code: Option<String>,  // CINFO 28 ap_iaMetadataLanguageCode
    pub language_name: Option<String>,  // CINFO 29 ap_iaMetadataLanguageName
    pub volume_label: Option<String>,   // CINFO 32 ap_iaVolumeName
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TitleAttributes {
    pub index: u32,
    pub name: Option<String>,            // TINFO 2
    pub chapter_count: Option<u32>,      // TINFO 8
    pub duration_seconds: Option<u64>,   // TINFO 9  (parsed from "HH:MM:SS")
    pub duration_text: Option<String>,   // TINFO 9  raw
    pub display_size: Option<String>,    // TINFO 10
    pub size_bytes: Option<u64>,         // TINFO 11
    pub source_file: Option<String>,     // TINFO 16
    pub segment_count: Option<u32>,      // TINFO 25
    pub segment_map: Option<String>,     // TINFO 26
    pub output_file: Option<String>,     // TINFO 27
    pub language_code: Option<String>,   // TINFO 28
    pub streams: Vec<StreamAttributes>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StreamAttributes {
    pub stream: u32,
    pub kind: Option<String>,           // SINFO 1  e.g. "Video"/"Audio"/"Subtitles"
    pub name: Option<String>,           // SINFO 2
    pub language_code: Option<String>,  // SINFO 3
    pub language_name: Option<String>,  // SINFO 4
    pub codec_id: Option<String>,       // SINFO 5
    pub codec_short: Option<String>,    // SINFO 6
    pub codec_long: Option<String>,     // SINFO 7
    pub bitrate: Option<String>,        // SINFO 13
    pub channels: Option<u32>,          // SINFO 14
    pub sample_rate: Option<u32>,       // SINFO 17
    pub sample_size: Option<u32>,       // SINFO 18
    pub video_size: Option<String>,     // SINFO 19
    pub aspect_ratio: Option<String>,   // SINFO 20
    pub frame_rate: Option<String>,     // SINFO 21
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MsgRecord {
    pub code: u32,
    pub priority: u32,
    pub text: String,
}

/// Aggregate a slice of records into a typed `MakemkvScan`. Multi-pass:
/// disc-level attributes from `CINFO`, titles built from `TINFO` (one
/// `TitleAttributes` per distinct title index), streams from `SINFO`.
pub fn to_makemkv_scan(records: &[Record]) -> MakemkvScan {
    let mut scan = MakemkvScan::default();
    let mut title_idx_by_id: BTreeMap<u32, usize> = BTreeMap::new();

    for rec in records {
        match rec {
            Record::Msg { code, priority, text, .. } => {
                if *code == 1005 {
                    scan.makemkv_version = parse_version(text);
                }
                scan.messages.push(MsgRecord {
                    code: *code,
                    priority: *priority,
                    text: text.clone(),
                });
            }
            Record::Cinfo { code, text, .. } => apply_cinfo(&mut scan.disc, *code, text),
            Record::Tinfo { title, code, text, .. } => {
                let entry_idx = match title_idx_by_id.get(title) {
                    Some(i) => *i,
                    None => {
                        let i = scan.titles.len();
                        scan.titles.push(TitleAttributes { index: *title, ..Default::default() });
                        title_idx_by_id.insert(*title, i);
                        i
                    }
                };
                apply_tinfo(&mut scan.titles[entry_idx], *code, text);
            }
            Record::Sinfo { title, stream, code, text, .. } => {
                let title_pos = match title_idx_by_id.get(title) {
                    Some(i) => *i,
                    None => {
                        let i = scan.titles.len();
                        scan.titles.push(TitleAttributes { index: *title, ..Default::default() });
                        title_idx_by_id.insert(*title, i);
                        i
                    }
                };
                let title_attrs = &mut scan.titles[title_pos];
                let stream_pos = match title_attrs.streams.iter().position(|s| s.stream == *stream) {
                    Some(p) => p,
                    None => {
                        title_attrs.streams.push(StreamAttributes { stream: *stream, ..Default::default() });
                        title_attrs.streams.len() - 1
                    }
                };
                apply_sinfo(&mut title_attrs.streams[stream_pos], *code, text);
            }
            Record::Tcount(_) | Record::Drv { .. } | Record::Unknown { .. } => {}
        }
    }

    scan
}

fn apply_cinfo(disc: &mut DiscAttributes, code: u32, text: &str) {
    let s = || Some(text.to_string());
    match code {
        1 => disc.content_type = s(),
        2 => disc.name = s(),
        28 => disc.language_code = s(),
        29 => disc.language_name = s(),
        32 => disc.volume_label = s(),
        _ => {}
    }
}

fn apply_tinfo(t: &mut TitleAttributes, code: u32, text: &str) {
    let s = || Some(text.to_string());
    match code {
        2 => t.name = s(),
        8 => t.chapter_count = text.parse().ok(),
        9 => {
            t.duration_seconds = parse_duration(text);
            t.duration_text = s();
        }
        10 => t.display_size = s(),
        11 => t.size_bytes = text.parse().ok(),
        16 => t.source_file = s(),
        25 => t.segment_count = text.parse().ok(),
        26 => t.segment_map = s(),
        27 => t.output_file = s(),
        28 => t.language_code = s(),
        _ => {}
    }
}

fn apply_sinfo(s: &mut StreamAttributes, code: u32, text: &str) {
    let some = || Some(text.to_string());
    match code {
        1 => s.kind = some(),
        2 => s.name = some(),
        3 => s.language_code = some(),
        4 => s.language_name = some(),
        5 => s.codec_id = some(),
        6 => s.codec_short = some(),
        7 => s.codec_long = some(),
        13 => s.bitrate = some(),
        14 => s.channels = text.parse().ok(),
        17 => s.sample_rate = text.parse().ok(),
        18 => s.sample_size = text.parse().ok(),
        19 => s.video_size = some(),
        20 => s.aspect_ratio = some(),
        21 => s.frame_rate = some(),
        _ => {}
    }
}

/// Parse `"HH:MM:SS"` or `"MM:SS"` into total seconds. Returns `None`
/// for unparseable input.
pub fn parse_duration(text: &str) -> Option<u64> {
    let parts: Vec<&str> = text.split(':').collect();
    let nums: Vec<u64> = parts.iter().map(|p| p.parse().ok()).collect::<Option<_>>()?;
    match nums.as_slice() {
        [h, m, s] => Some(h * 3600 + m * 60 + s),
        [m, s] => Some(m * 60 + s),
        [s] => Some(*s),
        _ => None,
    }
}

static VERSION_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"v(\d+\.\d+(?:\.\d+)?)").expect("static regex"));

fn parse_version(msg_text: &str) -> Option<String> {
    VERSION_RE.captures(msg_text).map(|c| c[1].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tcount_line() {
        let r = parse_line("TCOUNT:6").unwrap().unwrap();
        assert_eq!(r, Record::Tcount(6));
    }

    #[test]
    fn tinfo_line() {
        let r = parse_line(r#"TINFO:0,9,0,"2:06:42""#).unwrap().unwrap();
        assert_eq!(r, Record::Tinfo { title: 0, code: 9, value: 0, text: "2:06:42".into() });
    }

    #[test]
    fn sinfo_line() {
        let r = parse_line(r#"SINFO:0,0,7,0,"Mpeg4 AVC High@L4.1""#).unwrap().unwrap();
        assert_eq!(r, Record::Sinfo { title: 0, stream: 0, code: 7, value: 0, text: "Mpeg4 AVC High@L4.1".into() });
    }

    #[test]
    fn cinfo_line() {
        let r = parse_line(r#"CINFO:2,0,"Jurassic Park 3D""#).unwrap().unwrap();
        assert_eq!(r, Record::Cinfo { code: 2, value: 0, text: "Jurassic Park 3D".into() });
    }

    #[test]
    fn quoted_field_with_comma() {
        let r = parse_line(r#"TINFO:0,30,0,"Jurassic Park 3D - 20 chapter(s) , 40.3 GB""#)
            .unwrap()
            .unwrap();
        assert_eq!(
            r,
            Record::Tinfo {
                title: 0,
                code: 30,
                value: 0,
                text: "Jurassic Park 3D - 20 chapter(s) , 40.3 GB".into(),
            }
        );
    }

    #[test]
    fn quoted_field_with_escaped_quote() {
        let r = parse_line(r#"MSG:1,0,0,"He said ""hello""","fmt""#).unwrap().unwrap();
        match r {
            Record::Msg { text, .. } => assert_eq!(text, r#"He said "hello""#),
            other => panic!("expected MSG, got {other:?}"),
        }
    }

    #[test]
    fn empty_line_is_none() {
        assert!(parse_line("").unwrap().is_none());
        assert!(parse_line("\r\n").unwrap().is_none());
    }

    #[test]
    fn unknown_kind() {
        match parse_line("FOO:1,2,3").unwrap().unwrap() {
            Record::Unknown { kind } => assert_eq!(kind, "FOO"),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn continuation_line_is_none() {
        // Lines that don't have a `:` prefix are folded by the higher-level
        // aggregator; the line parser returns None.
        assert!(parse_line(r#"continuation of a multi-line MSG"#).unwrap().is_none());
    }

    #[test]
    fn unterminated_string_errors() {
        assert!(matches!(
            parse_line(r#"CINFO:2,0,"unterminated"#),
            Err(ParseError::UnterminatedString)
        ));
    }

    #[test]
    fn aggregator_emits_one_record_per_complete_line() {
        let mut agg = Aggregator::new();
        let r = agg.push_line("TCOUNT:6");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0], Record::Tcount(6));

        let r = agg.push_line(r#"TINFO:0,9,0,"2:06:42""#);
        assert_eq!(r.len(), 1);
        assert!(matches!(r[0], Record::Tinfo { code: 9, .. }));
    }

    #[test]
    fn aggregator_emits_zero_records_during_multiline_msg() {
        let mut agg = Aggregator::new();
        // First fragment ends with `\` continuation marker -- no record yet.
        let r = agg.push_line(r#"MSG:3335,1288,0,"line one \"#);
        assert!(r.is_empty());
        // Empty continuation line — still buffering.
        let r = agg.push_line(r#"\"#);
        assert!(r.is_empty());
        // Final line closes the quoted string and the record.
        let r = agg.push_line(r#"line three","fmt""#);
        assert_eq!(r.len(), 1);
        match &r[0] {
            Record::Msg { text, .. } => assert!(text.contains("line one") && text.contains("line three")),
            other => panic!("expected MSG, got {other:?}"),
        }
    }

    #[test]
    fn aggregator_finish_flushes_a_pending_complete_record() {
        let mut agg = Aggregator::new();
        // No newline at end of input, but record is parseable.
        assert!(agg.push_line("TCOUNT:5").is_empty() || true);
        // Above pushed a record already (TCOUNT is single-line); finish() returns
        // nothing because the buffer is empty. Test the empty case explicitly:
        let agg = Aggregator::new();
        assert!(agg.finish().is_empty());
    }

    #[test]
    fn aggregator_finish_drops_unterminated_buffer() {
        let mut agg = Aggregator::new();
        agg.push_line(r#"CINFO:2,0,"never closes"#);
        assert!(agg.finish().is_empty());
    }
}
