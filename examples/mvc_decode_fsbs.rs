// Decode an MVC Annex B elementary stream to a single full-side-by-side planar
// YUV420 file (view 0 left, view 1 right) — the composed form the conversion
// runner streams straight into ffmpeg (no per-view scratch files).
//
//   mvc_decode_fsbs -- stream.h264 out_fsbs.yuv

use ripsaw::mvc::clip::decode_annex_b_to_fsbs_writer;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let src = args.next().expect("usage: mvc_decode_fsbs <stream.h264> <out_fsbs.yuv>");
    let out = args.next().expect("missing output path");
    let reader = std::io::BufReader::new(std::fs::File::open(&src)?);
    let file = std::fs::File::create(&out)?;
    let info = decode_annex_b_to_fsbs_writer(reader, file)?;
    eprintln!("decoded {} frames → {out} ({}×{} full-SBS)", info.frames, info.width * 2, info.height);
    Ok(())
}
