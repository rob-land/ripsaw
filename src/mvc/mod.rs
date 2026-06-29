// 3D MVC pipeline. See docs/mvc3d.md.
//
// `bitstream`, `rbsp`, `nal`, `sps`, `pps`, `slice_header`,
// `ref_pic_list_modification`, and `sei` form the complete pure-Rust
// front-end parser: from an Annex B byte stream down to fully decoded
// SPS / Subset SPS / PPS / slice headers for both base and MVC
// dependent-view slices. What's still ahead in the libmvc skeleton is
// the decode core — residual parsing, transform, prediction,
// deblocking, DPB, and inter-view reference construction.

pub mod annexb;
pub mod bitstream;
pub mod cabac;
pub mod transform;
pub mod decoder;
pub mod ebml;
pub mod layout;
pub mod mkv_extract;
pub mod mvcc;
pub mod nal;
pub mod pps;
pub mod rbsp;
pub mod ref_pic_list_modification;
pub mod sei;
pub mod slice_header;
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
