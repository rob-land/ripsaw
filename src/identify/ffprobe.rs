// Minimal `ffprobe -of json` wrapper. We only model the fields we
// actually consume (format duration / size, video stream count / codec
// / width / height / frame rate, chapters list); everything else in the
// JSON is ignored.

use std::path::Path;
use std::process::Stdio;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct FfprobeReport {
    #[serde(default)]
    pub format: FfprobeFormat,
    #[serde(default)]
    pub streams: Vec<FfprobeStream>,
    #[serde(default)]
    pub chapters: Vec<FfprobeChapter>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct FfprobeFormat {
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default)]
    pub duration: Option<String>,
    #[serde(default)]
    pub size: Option<String>,
    #[serde(default)]
    pub bit_rate: Option<String>,
    #[serde(default)]
    pub format_name: Option<String>,
    #[serde(default)]
    pub tags: Option<std::collections::BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FfprobeStream {
    pub index: u32,
    pub codec_type: String,
    #[serde(default)]
    pub codec_name: Option<String>,
    #[serde(default)]
    pub codec_long_name: Option<String>,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default)]
    pub r_frame_rate: Option<String>,
    #[serde(default)]
    pub channels: Option<u32>,
    #[serde(default)]
    pub sample_rate: Option<String>,
    #[serde(default)]
    pub tags: Option<std::collections::BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FfprobeChapter {
    // ffprobe emits the chapter id as a signed int64. For Matroska it's
    // derived from the (64-bit) ChapterUID, so large UIDs come through as
    // negative numbers -- a u64 here rejects the whole report with
    // "invalid value: integer `-2206971460243292344`, expected u64".
    pub id: i64,
    #[serde(default)]
    pub start_time: Option<String>,
    #[serde(default)]
    pub end_time: Option<String>,
    #[serde(default)]
    pub tags: Option<std::collections::BTreeMap<String, String>>,
}

/// Run `ffprobe -of json -show_format -show_streams -show_chapters PATH`
/// and parse the result.
pub async fn probe(path: &Path) -> Result<FfprobeReport> {
    let output = crate::hostcmd::host_command("ffprobe")
        .args([
            "-v",
            "error",
            "-of",
            "json",
            "-show_format",
            "-show_streams",
            "-show_chapters",
        ])
        .arg(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("spawn ffprobe")?;
    if !output.status.success() {
        return Err(anyhow!(
            "ffprobe failed ({}) on {}: {}",
            output.status,
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    parse(&output.stdout)
}

pub fn parse(json: &[u8]) -> Result<FfprobeReport> {
    serde_json::from_slice(json).context("parsing ffprobe JSON")
}

impl FfprobeReport {
    pub fn video_streams(&self) -> impl Iterator<Item = &FfprobeStream> {
        self.streams.iter().filter(|s| s.codec_type == "video")
    }

    pub fn audio_streams(&self) -> impl Iterator<Item = &FfprobeStream> {
        self.streams.iter().filter(|s| s.codec_type == "audio")
    }

    pub fn subtitle_streams(&self) -> impl Iterator<Item = &FfprobeStream> {
        self.streams.iter().filter(|s| s.codec_type == "subtitle")
    }

    /// File-format-level duration in seconds, parsed from the `duration`
    /// string ffprobe emits.
    pub fn duration_seconds(&self) -> Option<u64> {
        self.format
            .duration
            .as_ref()
            .and_then(|s| s.parse::<f64>().ok())
            .map(|d| d.round() as u64)
    }

    pub fn size_bytes(&self) -> Option<u64> {
        self.format.size.as_ref().and_then(|s| s.parse::<u64>().ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "streams": [
        {"index": 0, "codec_type": "video", "codec_name": "h264", "profile": "High",
         "width": 1920, "height": 1080, "r_frame_rate": "24000/1001"},
        {"index": 1, "codec_type": "audio", "codec_name": "ac3", "channels": 6,
         "sample_rate": "48000"},
        {"index": 2, "codec_type": "subtitle"}
      ],
      "format": {
        "filename": "/tmp/x.mkv",
        "duration": "1542.291667",
        "size": "955451392",
        "bit_rate": "4956000",
        "format_name": "matroska,webm",
        "tags": { "title": "My Show", "creation_time": "2024-01-01T00:00:00.000Z" }
      },
      "chapters": [
        {"id": 0, "start_time": "0.0", "end_time": "10.0",
         "tags": { "title": "Opening" }},
        {"id": 1, "start_time": "10.0", "end_time": "20.0",
         "tags": { "title": "Act 1" }}
      ]
    }"#;

    #[test]
    fn parses_negative_matroska_chapter_id() {
        // Matroska ChapterUIDs are 64-bit; ffprobe prints the chapter id
        // as a signed int, so big UIDs surface as negative numbers. The
        // report must still parse (regression: a u64 field rejected it).
        let json = r#"{
            "streams": [],
            "format": {},
            "chapters": [
                {"id": -2206971460243292344, "start_time": "0.0",
                 "end_time": "10.0", "tags": {"title": "Reel 1"}}
            ]
        }"#;
        let report = parse(json.as_bytes()).unwrap();
        assert_eq!(report.chapters.len(), 1);
        assert_eq!(report.chapters[0].id, -2206971460243292344);
    }

    #[test]
    fn parses_basic_report() {
        let report = parse(SAMPLE.as_bytes()).unwrap();
        assert_eq!(report.streams.len(), 3);
        assert_eq!(report.video_streams().count(), 1);
        assert_eq!(report.audio_streams().count(), 1);
        assert_eq!(report.subtitle_streams().count(), 1);
        assert_eq!(report.chapters.len(), 2);
        assert_eq!(report.duration_seconds(), Some(1542));
        assert_eq!(report.size_bytes(), Some(955451392));
        assert_eq!(report.format.filename.as_deref(), Some("/tmp/x.mkv"));
        let video = report.video_streams().next().unwrap();
        assert_eq!(video.codec_name.as_deref(), Some("h264"));
        assert_eq!(video.profile.as_deref(), Some("High"));
        assert_eq!(video.width, Some(1920));
    }
}
