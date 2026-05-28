// FFmpeg transcoding pipeline. See docs/transcode.md.

pub mod ffmpeg;
pub mod presets;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Preset {
    pub name: String,
    pub target: Encoder,
    pub crf: Option<u8>,
    pub preset: Option<String>,
    pub tune: Option<String>,
    pub hdr: HdrPolicy,
    pub audio: AudioPolicy,
    pub subtitles: SubtitlePolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Encoder { X264, X265, Av1, Passthrough }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HdrPolicy { Preserve, Tonemap, Strip }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AudioPolicy {
    Passthrough,
    Aac { bitrate_kbps: u32 },
    Opus { bitrate_kbps: u32 },
    Ac3 { bitrate_kbps: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubtitlePolicy { Passthrough, Strip }
