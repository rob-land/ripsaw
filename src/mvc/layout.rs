// Stereo layout composition. See docs/mvc3d.md § "Output layouts".

use super::StereoLayout;
use super::decoder::StereoFrame;

#[derive(Debug, Clone)]
pub struct OutputFrameDescriptor {
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    pub pix_fmt: PixelFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Yuv420P,
    Yuv420P10Le,
}

pub fn descriptor_for(
    _layout: StereoLayout,
    _src_w: u32,
    _src_h: u32,
    _src_fps_num: u32,
    _src_fps_den: u32,
    _bit_depth: u8,
) -> OutputFrameDescriptor {
    todo!("derive output dimensions/fps per docs/mvc3d.md output-layouts table")
}

pub fn compose(_layout: StereoLayout, _frame: &StereoFrame, _dst: &mut [u8]) {
    todo!("write the composed frame into the destination buffer in the descriptor's pix_fmt")
}
