// 3D MVC pipeline. See docs/mvc3d.md.

pub mod decoder;
pub mod layout;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StereoLayout {
    FullSbs,
    HalfSbs,
    FullTab,
    HalfTab,
    FrameSequential,
    Interleaved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DecoderBackend {
    BritzFfmpeg,
    JmReference,
    WineFrim,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MvcOutputPlan {
    pub layout: StereoLayout,
    pub decoder: DecoderBackend,
    pub subtitle_depth: SubtitleDepth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubtitleDepth {
    Passthrough,
    HardcodedWithDepth,
    Skip,
}
