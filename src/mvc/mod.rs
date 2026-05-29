// 3D MVC pipeline. See docs/mvc3d.md.
//
// `bitstream`, `rbsp`, `nal`, and `sps` are the phase-0 bitstream
// plumbing — they implement just enough of Annex G to recognise MVC
// NAL units and decode the Subset SPS MVC extension. The actual MVC
// decoder (slice header parsing, reference-picture-list construction,
// inter-view prediction wiring) lives ahead in the libmvc skeleton.

pub mod bitstream;
pub mod decoder;
pub mod ebml;
pub mod layout;
pub mod mvcc;
pub mod nal;
pub mod rbsp;
pub mod sps;

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
