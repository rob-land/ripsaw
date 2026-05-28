// makemkvcon driver. See docs/rip.md.

use super::{DiscScan, TitleScan};

#[derive(Debug, Clone)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

#[derive(Debug)]
pub enum ProbeOutcome {
    Missing,
    Outdated(Version),
    Ok(Version),
}

pub async fn probe() -> ProbeOutcome {
    todo!("run `makemkvcon --version`, parse, compare to minimum_supported_version()")
}

pub fn minimum_supported_version() -> Version {
    // Bumped per release; older builds fail on current discs because of expired beta keys
    // and lack of UHD AACS2 support.
    Version { major: 1, minor: 17, patch: 0 }
}

pub async fn scan(_disc_index: u32) -> anyhow::Result<DiscScan> {
    todo!("spawn makemkvcon info, parse -r records into DiscScan")
}

pub async fn extract_title(
    _disc_index: u32,
    _title: &TitleScan,
    _output_dir: &std::path::Path,
    _progress: tokio::sync::mpsc::Sender<Progress>,
) -> anyhow::Result<std::path::PathBuf> {
    todo!("spawn makemkvcon mkv, stream PRGV: progress, return final mkv path")
}

#[derive(Debug, Clone)]
pub struct Progress {
    pub current: u64,
    pub total: u64,
    pub message: Option<String>,
}
