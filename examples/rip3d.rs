// One-shot: a 3D Blu-ray ISO (or an MVC MKV) → a full-side-by-side HEVC MKV.
//
//   cargo run --release --example rip3d -- <input.iso|input.mkv> <output.mkv>
//
// - ISO input: loop-mount it (unprivileged, via udisksctl/UDisks2). For an
//   UNENCRYPTED 3D Blu-ray this takes the fast path — libmvc decodes the disc's
//   SSIF (stereoscopic transport stream) directly, with no makemkvcon rip and no
//   ~40 GB intermediate MKV. Otherwise it falls back to ripping the main MVC
//   title with MakeMKV's keep-MVC profile, then converting that.
// - MKV input: use it directly if it carries an MVC stream ripsaw detects.
// Either way the convert pipeline packs full-side-by-side and encodes HEVC. The
// output MKV is tagged with the Matroska StereoMode (left_right) and keeps the
// original audio/subtitles.
//
// HW encode is auto-selected (hevc_vaapi etc.); set RIPSAW_HW=software for
// libx265 (better quality per bit, slower).

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use ripsaw::convert::format::OutputFormat;
use ripsaw::convert::hw::{EncodeCodec, HwBackend};
use ripsaw::convert::plan::{detect_stereo_source, ConversionPlan, StereoSource};
use ripsaw::convert::runner::{convert_bd_ssif, run_conversion, ConversionEvent};
use ripsaw::identify::pipeline::identify_iso;
use ripsaw::rip::bd_playlist::find_feature_3d;
use ripsaw::rip::iso_mount::MountedIso;
use ripsaw::rip::makemkv::{extract_title, ExtractEvent};
use tokio::sync::mpsc;

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let input = args
        .next()
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("usage: rip3d <input.iso|input.mkv> <output.mkv>"))?;
    let output = args.next().map(PathBuf::from).ok_or_else(|| anyhow!("missing output path"))?;
    let hw_backend = match std::env::var("RIPSAW_HW").as_deref() {
        Ok("software") | Ok("sw") => HwBackend::Software,
        _ => HwBackend::Auto,
    };

    // Keep the rip temp dir + ISO mount alive until the convert finishes.
    let mut rip_dir: Option<tempfile::TempDir> = None;
    let mut mount: Option<MountedIso> = None;

    // Resolve the input to an MVC MKV the convert pipeline can consume — unless
    // an ISO lets us take the SSIF fast path, which converts and returns here.
    let mkv: PathBuf = if detect_stereo_source(&input).is_some() {
        eprintln!("Input is an MVC MKV — converting directly.");
        input.clone()
    } else {
        // Fast path: mount the ISO and, if it's an UNENCRYPTED 3D Blu-ray, let
        // libmvc decode the SSIF straight off the disc — no makemkvcon at all,
        // no rip, no ~40 GB intermediate. (Needs no makemkvcon, so it works even
        // where MakeMKV isn't installed.)
        eprintln!("Mounting {} …", input.display());
        if let Ok(m) = MountedIso::mount(&input).await {
            if !is_encrypted(&m.mount_point) {
                if let Some(feature) = find_feature_3d(&m.mount_point) {
                    let clips = feature.clip_paths(&m.mount_point);
                    eprintln!(
                        "Unencrypted 3D BD — feature is {} clip(s), {} min; decoding directly (no makemkvcon, no intermediate).",
                        clips.len(),
                        feature.duration_seconds() / 60
                    );
                    let plan = ConversionPlan {
                        input: PathBuf::new(), // unused by convert_bd_ssif
                        output: output.clone(),
                        format: OutputFormat::FullSbs,
                        source: StereoSource::MvcWithBlockAdditions, // unused by convert_bd_ssif
                        codec: EncodeCodec::H265,
                        hw_backend,
                    };
                    let (tx, rx) = mpsc::channel::<ConversionEvent>(64);
                    let printer = spawn_convert_printer(rx);
                    let result = convert_bd_ssif(&clips, &plan, Some(tx)).await;
                    let _ = printer.await;
                    let _ = m.unmount().await;
                    result.context("decoding the Blu-ray SSIF")?;
                    eprintln!("Done → {}", output.display());
                    return Ok(());
                }
            }
            // Not a plain SSIF disc (encrypted, or no 3D playlist) — release this
            // mount; the makemkvcon path re-mounts as part of identify.
            let _ = m.unmount().await;
        }

        // Fallback: identify + rip the main MVC title with makemkvcon, then
        // convert the resulting MKV.
        eprintln!("Identifying {} (makemkvcon) …", input.display());
        let ident = identify_iso(input.clone())
            .await
            .with_context(|| format!("identifying {}", input.display()))?;
        eprintln!(
            "  disc type: {:?} · {} title(s) · 3D-MVC: {}",
            ident.disc_type,
            ident.scan.titles.len(),
            ident.has_mvc
        );

        // Main title = the longest one carrying an MVC stream, else just the
        // longest. (A 3D movie disc's feature is the long MVC title.)
        let pick = ident
            .scan
            .titles
            .iter()
            .filter(|t| t.has_mvc_stream())
            .max_by_key(|t| t.duration_seconds.unwrap_or(0))
            .or_else(|| ident.scan.titles.iter().max_by_key(|t| t.duration_seconds.unwrap_or(0)))
            .ok_or_else(|| anyhow!("no titles found on {}", input.display()))?;
        eprintln!(
            "Ripping title {} ({}, keep-MVC) — this writes a full MVC MKV, so it needs room…",
            pick.index,
            pick.duration_text.as_deref().unwrap_or("?")
        );

        let dir = tempfile::Builder::new()
            .prefix("rip3d-")
            .tempdir_in(output.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new(".")))
            .context("creating rip temp dir next to the output")?;
        let (tx, mut rx) = mpsc::channel::<ExtractEvent>(64);
        let printer = tokio::spawn(async move {
            let mut last = -1i32;
            while let Some(ev) = rx.recv().await {
                if let ExtractEvent::Progress(p) = ev {
                    let pct = if p.max > 0 { (p.total as f32 / p.max as f32 * 100.0) as i32 } else { 0 };
                    if pct != last {
                        eprint!("\r  rip {pct:3}%   ");
                        last = pct;
                    }
                }
            }
            eprintln!();
        });
        let ripped = extract_title(&ident.source, pick.index, dir.path(), "title.mkv", Some(tx))
            .await
            .context("ripping the MVC title with makemkvcon")?;
        let _ = printer.await;
        rip_dir = Some(dir);
        mount = ident.mount;
        ripped
    };

    // Convert the MVC MKV → full-SBS HEVC.
    let source = detect_stereo_source(&mkv)
        .ok_or_else(|| anyhow!("{} has no MVC/stereo stream ripsaw can convert", mkv.display()))?;
    eprintln!("Converting → full-SBS HEVC: {}", output.display());
    let plan = ConversionPlan {
        input: mkv,
        output: output.clone(),
        format: OutputFormat::FullSbs,
        source,
        codec: EncodeCodec::H265,
        hw_backend,
    };
    let (tx, rx) = mpsc::channel::<ConversionEvent>(64);
    let printer = spawn_convert_printer(rx);
    let result = run_conversion(plan, Some(tx)).await;
    let _ = printer.await;

    // Best-effort cleanup regardless of outcome.
    if let Some(m) = mount.take() {
        let _ = m.unmount().await;
    }
    drop(rip_dir);
    result.context("converting to full-SBS HEVC")?;

    eprintln!("Done → {}", output.display());
    Ok(())
}

/// True if the mounted disc looks AACS-encrypted (an AACS directory present).
/// The native SSIF path only works on decrypted / unencrypted discs.
fn is_encrypted(mount: &Path) -> bool {
    mount.join("AACS").is_dir() || mount.join("BDMV/AACS").is_dir()
}

fn spawn_convert_printer(mut rx: mpsc::Receiver<ConversionEvent>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            match ev {
                ConversionEvent::Progress { current_seconds, total_seconds } => match total_seconds {
                    Some(t) => eprint!("\r  encode {current_seconds:.0}/{t:.0}s   "),
                    None => eprint!("\r  encode {current_seconds:.0}s   "),
                },
                ConversionEvent::Log(l) => eprintln!("  {l}"),
            }
        }
        eprintln!();
    })
}
