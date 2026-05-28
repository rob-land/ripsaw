// MVC dependent-view decoders. See docs/mvc3d.md § "Decoding strategy".

use super::DecoderBackend;

pub trait MvcDecoder: Send {
    fn decode(
        &mut self,
        src: &std::path::Path,
    ) -> Box<dyn futures::Stream<Item = anyhow::Result<StereoFrame>> + Send + Unpin>;
}

#[derive(Debug, Clone)]
pub struct StereoFrame {
    pub pts: i64,
    pub width: u32,
    pub height: u32,
    pub left:  YuvFrame,
    pub right: YuvFrame,
}

#[derive(Debug, Clone)]
pub struct YuvFrame {
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
    pub stride_y: u32,
    pub stride_uv: u32,
}

pub fn detect_available() -> Vec<DecoderBackend> {
    todo!("probe for threedrip-mvcdec, ldecod, wine+FRIM; return what's installed")
}

pub fn open(_backend: DecoderBackend) -> anyhow::Result<Box<dyn MvcDecoder>> {
    todo!("dispatch to BritzFfmpegDecoder / JmReferenceDecoder / WineFrimDecoder")
}
