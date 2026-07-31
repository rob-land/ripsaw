// Smoke-test the UDF + SSIF ISO path end to end: detect a 3D Blu-ray ISO's
// feature and convert it straight to full-SBS — no mount, no makemkvcon.
//
//   iso3d <image.iso> <out.mkv>
use std::path::{Path, PathBuf};

use ripsaw::convert::format::OutputFormat;
use ripsaw::convert::hw::{EncodeCodec, HwBackend};
use ripsaw::convert::plan::{ConversionPlan, StereoSource};
use ripsaw::rip::bd_playlist::find_feature_3d_iso;
use ripsaw::rip::udf::Udf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let iso = std::env::args().nth(1).expect("usage: iso3d <image.iso> <out.mkv>");
    let out = std::env::args().nth(2).expect("missing out path");

    let mut f = std::fs::File::open(&iso)?;
    let udf = Udf::open(&mut f)?;
    let feat = find_feature_3d_iso(&udf, &mut f).expect("no 3D feature in ISO");
    eprintln!("feature: clips {:?}, {} min", feat.clips, feat.duration_seconds() / 60);

    let plan = ConversionPlan {
        input: PathBuf::new(),
        output: PathBuf::from(&out),
        format: OutputFormat::FullSbs,
        source: StereoSource::MvcWithBlockAdditions,
        codec: EncodeCodec::H265,
        hw_backend: HwBackend::Qsv,
    };
    let (tx, mut rx) = tokio::sync::mpsc::channel::<ripsaw::convert::runner::ConversionEvent>(64);
    tokio::spawn(async move {
        use ripsaw::convert::runner::ConversionEvent;
        while let Some(ev) = rx.recv().await {
            match ev {
                ConversionEvent::Progress { current_seconds, total_seconds, label } => {
                    let pct = total_seconds.map(|t| 100.0 * current_seconds / t).unwrap_or(0.0);
                    eprintln!("PROGRESS [{label}] {current_seconds:.0}s / {total_seconds:?} ({pct:.1}%)");
                }
                ConversionEvent::Log(l) => eprintln!("LOG {l}"),
            }
        }
    });
    ripsaw::convert::runner::convert_bd_ssif_iso(Path::new(&iso), &feat.clips, feat.duration_seconds(), &plan, Some(tx)).await?;
    eprintln!("done → {out}");
    Ok(())
}
