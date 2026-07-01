// Decode an MVC Annex B elementary stream to two per-view planar YUV420
// files, entirely in libmvc — the native replacement for the `ldecod` decode
// step in the 3D conversion pipeline.
//
//   mvc_decode_yuv -- stream.h264 view0.yuv view1.yuv

use ripsaw::mvc::clip::decode_annex_b_to_yuv_files;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let src = args.next().expect("usage: mvc_decode_yuv <stream.h264> <view0.yuv> <view1.yuv>");
    let view0 = args.next().expect("missing view0 output path");
    let view1 = args.next().expect("missing view1 output path");
    let data = std::fs::read(&src)?;
    let info = decode_annex_b_to_yuv_files(&data, view0.as_ref(), view1.as_ref())?;
    eprintln!("decoded {} frames, {}×{} per view → {view0}, {view1}", info.frames, info.width, info.height);
    Ok(())
}
