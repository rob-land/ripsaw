// One-shot 3D → packed-stereo conversion via the convert pipeline.
//
//   cargo run --release --example convert_3d -- <input.mkv> <output.mkv> <fsbs|hsbs|fou|hou|fs>
//
// Detects the stereo source (mvcC BlockAddition / inline laced /
// already-packed), builds a ConversionPlan, and drives run_conversion.
// Logs the convert pipeline's events to stderr so the JM ldecod
// progress + ffmpeg pass are visible.

use std::path::PathBuf;

use ripsaw::convert::format::OutputFormat;
use ripsaw::convert::plan::{detect_stereo_source, ConversionPlan};
use ripsaw::convert::runner::{run_conversion, ConversionEvent};

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let input = args
        .next()
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("usage: convert_3d <in.mkv> <out.mkv> <format>"))?;
    let output = args
        .next()
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("missing output path"))?;
    let format_str = args.next().unwrap_or_else(|| "fsbs".to_string());

    let format = match format_str.as_str() {
        "fsbs" => OutputFormat::FullSbs,
        "hsbs" => OutputFormat::HalfSbs,
        "fou" => OutputFormat::FullTab,
        "hou" => OutputFormat::HalfTab,
        "fs" => OutputFormat::FrameSequential,
        other => anyhow::bail!("unknown format {other}; use fsbs|hsbs|fou|hou|fs"),
    };

    let source = detect_stereo_source(&input).ok_or_else(|| {
        anyhow::anyhow!(
            "couldn't detect stereo source in {} (no mvcC, no stereo_mode 13/14)",
            input.display()
        )
    })?;
    eprintln!("input:  {}", input.display());
    eprintln!("output: {}", output.display());
    eprintln!("source: {source:?}");
    eprintln!("format: {format:?}");

    let codec = match std::env::var("RIPSAW_CODEC").as_deref() {
        Ok("h265") | Ok("hevc") => ripsaw::convert::hw::EncodeCodec::H265,
        _ => ConversionPlan::default_codec(),
    };
    let hw_backend = match std::env::var("RIPSAW_HW").as_deref() {
        Ok("auto") => ripsaw::convert::hw::HwBackend::Auto,
        Ok("vaapi") => ripsaw::convert::hw::HwBackend::Vaapi,
        Ok("nvenc") => ripsaw::convert::hw::HwBackend::Nvenc,
        Ok("qsv") => ripsaw::convert::hw::HwBackend::Qsv,
        Ok("amf") => ripsaw::convert::hw::HwBackend::Amf,
        Ok("v4l2") => ripsaw::convert::hw::HwBackend::V4l2M2m,
        _ => ConversionPlan::default_hw_backend(),
    };
    eprintln!("codec:  {codec:?}");
    eprintln!("hw:     {hw_backend:?}");

    let plan = ConversionPlan {
        input,
        output: output.clone(),
        format,
        source,
        codec,
        hw_backend,
    };

    let (tx, mut rx) = tokio::sync::mpsc::channel::<ConversionEvent>(64);
    let printer = tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            match ev {
                ConversionEvent::Log(line) => eprintln!("[convert] {line}"),
                ConversionEvent::Progress { current_seconds, total_seconds } => {
                    let total = total_seconds
                        .map(|t| format!("/{t:.1}s"))
                        .unwrap_or_default();
                    eprintln!("[convert] progress {current_seconds:.1}s{total}");
                }
            }
        }
    });

    let result = run_conversion(plan, Some(tx)).await;
    let _ = printer.await;
    let landed = result?;
    println!("OK: {}", landed.display());
    Ok(())
}
