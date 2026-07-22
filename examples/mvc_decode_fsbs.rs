// Decode an MVC Annex B elementary stream to a single full-side-by-side planar
// file (view 0 left, view 1 right) — the composed form the conversion runner
// streams straight into ffmpeg (no per-view scratch files). The optional third
// argument selects the pixel format (yuv420p default, or nv12 — the CSC-fused
// form VAAPI consumes).
//
//   mvc_decode_fsbs -- stream.h264 out_fsbs.yuv [yuv420p|nv12]

use ripsaw::mvc::clip::{decode_annex_b_to_fsbs_writer, FsbsPixFmt};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let src = args.next().expect("usage: mvc_decode_fsbs <stream.h264> <out_fsbs.yuv> [yuv420p|nv12]");
    let out = args.next().expect("missing output path");
    let pixfmt = match args.next().as_deref() {
        None | Some("yuv420p") => FsbsPixFmt::Yuv420p,
        Some("nv12") => FsbsPixFmt::Nv12,
        Some(other) => anyhow::bail!("unknown pixel format {other:?} (want yuv420p or nv12)"),
    };
    let reader = std::io::BufReader::new(std::fs::File::open(&src)?);
    let file = std::fs::File::create(&out)?;
    let info = decode_annex_b_to_fsbs_writer(reader, file, pixfmt)?;
    eprintln!("decoded {} frames → {out} ({}×{} full-SBS, {})", info.frames, info.width * 2, info.height, pixfmt.ffmpeg_name());
    Ok(())
}
