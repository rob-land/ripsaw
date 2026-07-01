// Drive the real conversion runner end-to-end on a 3D Blu-ray MKV:
// detect the stereo source, build a ConversionPlan, and run_conversion —
// extract → native libmvc decode → ffmpeg compose → encode → output MKV.
//
//   run_pipeline -- in.mkv out.fsbs.mkv

use ripsaw::convert::format::OutputFormat;
use ripsaw::convert::hw::{EncodeCodec, HwBackend};
use ripsaw::convert::plan::{detect_stereo_source, ConversionPlan};
use ripsaw::convert::runner::{run_conversion, ConversionEvent};
use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    let input = PathBuf::from(std::env::args().nth(1).expect("usage: run_pipeline <in.mkv> <out.mkv>"));
    let output = PathBuf::from(std::env::args().nth(2).expect("missing output path"));
    let source = detect_stereo_source(&input).ok_or_else(|| anyhow::anyhow!("{} is not a recognised stereo source", input.display()))?;
    eprintln!("detected stereo source: {source:?}");
    eprintln!("format: Full-SBS, codec: H.264, encoder: software (libx264)");

    let plan = ConversionPlan {
        input,
        output,
        format: OutputFormat::FullSbs,
        source,
        codec: EncodeCodec::H264,
        hw_backend: HwBackend::Software,
    };

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ConversionEvent>(256);
        let pump = tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                match ev {
                    ConversionEvent::Log(l) => eprintln!("[runner] {l}"),
                    ConversionEvent::Progress { current_seconds, total_seconds } => {
                        eprintln!("[progress] {current_seconds:.1}s / {total_seconds:?}")
                    }
                }
            }
        });
        let out = run_conversion(plan, Some(tx)).await?;
        pump.await.ok();
        eprintln!("✓ OUTPUT: {}", out.display());
        anyhow::Ok(())
    })?;
    Ok(())
}
