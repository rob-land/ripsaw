// FFmpeg subprocess driver. See docs/transcode.md.

use super::Preset;

pub async fn run(
    _preset: &Preset,
    _source: &std::path::Path,
    _dest: &std::path::Path,
    _progress: tokio::sync::mpsc::Sender<Progress>,
) -> anyhow::Result<()> {
    todo!("ffprobe -> compose filter graph -> ffmpeg -progress pipe -> stream progress")
}

#[derive(Debug, Clone)]
pub struct Progress {
    pub out_time_ms: u64,
    pub total_duration_ms: u64,
    pub fps: f32,
    pub bitrate_kbps: f32,
}
