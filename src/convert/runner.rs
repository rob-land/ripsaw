// Drive a ConversionPlan to completion.
//
// Three paths today:
//
// 1. `AlreadyPacked` source: pure ffmpeg `stereo3d` filter conversion.
//    Works end-to-end.
// 2. `MvcInlineLaced` source: mkvextract -> JM ldecod -> ffmpeg compose.
//    Real MVC decode end-to-end. Slow because ldecod is the JVT
//    reference implementation, not an optimised decoder.
// 3. `MvcWithBlockAdditions` source: our EBML walker
//    (src/mvc/mkv_extract.rs) interleaves base-track NALs with the
//    per-frame BlockAdditions into an Annex B stream, then feeds the
//    same ldecod -> ffmpeg compose tail as path 2.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::convert::format::OutputFormat;
use crate::convert::plan::{ConversionPlan, StereoSource};
use crate::identify::ffprobe;

#[derive(Debug, Clone)]
pub enum ConversionEvent {
    /// Free-form line of stderr from a subprocess.
    Log(String),
    /// Filled in when ffmpeg reports `out_time_us` via `-progress`.
    Progress { current_seconds: f64, total_seconds: Option<f64> },
}

pub async fn run_conversion(
    plan: ConversionPlan,
    event_tx: Option<tokio::sync::mpsc::Sender<ConversionEvent>>,
) -> Result<PathBuf> {
    match plan.source {
        StereoSource::AlreadyPacked { input_layout } => {
            run_stereo3d_filter(&plan, input_layout.ffmpeg_stereo3d_in(), event_tx).await
        }
        StereoSource::MvcInlineLaced => run_mvc_inline_pipeline(&plan, event_tx).await,
        StereoSource::MvcWithBlockAdditions => {
            run_mvc_block_additions_pipeline(&plan, event_tx).await
        }
        StereoSource::NotStereo => Err(anyhow!(
            "{} doesn't look like a stereo source — no 3D content to convert.",
            plan.input.display()
        )),
    }
}

async fn run_stereo3d_filter(
    plan: &ConversionPlan,
    input_layout: &str,
    event_tx: Option<tokio::sync::mpsc::Sender<ConversionEvent>>,
) -> Result<PathBuf> {
    ensure_parent_dir(&plan.output).await?;
    let stereo3d_filter =
        format!("stereo3d={}:{}", input_layout, plan.format.ffmpeg_stereo3d_out());
    let encoder = resolve_encoder_args(plan, event_tx.as_ref());

    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-hide_banner").arg("-y");
    // VAAPI's `-vaapi_device` has to come before `-i`.
    for a in &encoder.init {
        cmd.arg(a);
    }
    cmd.arg("-i").arg(&plan.input);
    // The encoder's pre-input filter chain (e.g. format=nv12,hwupload)
    // has to run AFTER the stereo3d composition, so chain them.
    let filter_chain = match &encoder.pre_input_vf {
        Some(extra) => format!("{stereo3d_filter},{extra}"),
        None => stereo3d_filter,
    };
    cmd.arg("-vf").arg(&filter_chain);
    for a in &encoder.encoder_args {
        cmd.arg(a);
    }
    cmd.arg("-c:a").arg("copy").arg("-c:s").arg("copy");
    apply_stereo_mode_tag(&mut cmd, plan.format);
    cmd.arg(&plan.output)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd.spawn().context("spawn ffmpeg")?;
    forward_stderr(&mut child, event_tx.clone());
    let status = child.wait().await.context("waiting for ffmpeg")?;
    if !status.success() {
        return Err(anyhow!("ffmpeg exited with status {}", status));
    }
    Ok(plan.output.clone())
}

async fn run_mvc_inline_pipeline(
    plan: &ConversionPlan,
    event_tx: Option<tokio::sync::mpsc::Sender<ConversionEvent>>,
) -> Result<PathBuf> {
    run_mvc_pipeline(plan, event_tx, MvcExtractor::Mkvextract).await
}

/// Same shape as run_mvc_inline_pipeline but extracts the Annex B
/// stream via our own EBML walker (src/mvc/mkv_extract.rs). Used for
/// mvcC-packaged MakeMKV sources where mkvextract on its own would
/// not deal with the BlockAddition variant of MVC packaging.
async fn run_mvc_block_additions_pipeline(
    plan: &ConversionPlan,
    event_tx: Option<tokio::sync::mpsc::Sender<ConversionEvent>>,
) -> Result<PathBuf> {
    run_mvc_pipeline(plan, event_tx, MvcExtractor::Builtin).await
}

#[derive(Copy, Clone)]
enum MvcExtractor {
    /// `mkvextract <input> tracks 0:track.h264` — works for inline /
    /// stereo-mode 13/14 sources where the full MVC bitstream lives
    /// in the regular video track.
    Mkvextract,
    /// `libmvc::mkv_extract::extract_to_annex_b` -- our own
    /// EBML walker for mvcC BlockAdditionMapping sources.
    Builtin,
}

async fn run_mvc_pipeline(
    plan: &ConversionPlan,
    event_tx: Option<tokio::sync::mpsc::Sender<ConversionEvent>>,
    extractor: MvcExtractor,
) -> Result<PathBuf> {
    ensure_parent_dir(&plan.output).await?;

    let report = ffprobe::probe(&plan.input).await?;
    let video = report
        .video_streams()
        .next()
        .ok_or_else(|| anyhow!("no video stream in {}", plan.input.display()))?;
    let width = video.width.unwrap_or(1920);
    let height = video.height.unwrap_or(1080);
    let frame_rate = video.r_frame_rate.clone().unwrap_or_else(|| "24000/1001".to_string());

    // MVC decode intermediates are huge: each view is 1920x1080 yuv420p
    // = 3.1 MB / frame, so a feature-length source needs tens of GB of
    // scratch per run. The default /tmp on most distros (and certainly
    // GNOME's tmpfs default) is RAM-backed and far smaller than this --
    // ldecod silently fails its writes once the tmpfs fills.
    //
    // Place the temp dir next to the final output instead. That path is
    // already a real on-disk location the user picked (their media
    // library root), and they have to have enough room for the final
    // MKV anyway. Fall back to system tmp if the output has no parent.
    let temp_parent = plan
        .output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(std::env::temp_dir);
    tokio::fs::create_dir_all(&temp_parent)
        .await
        .with_context(|| {
            format!("ensuring temp parent {} exists", temp_parent.display())
        })?;
    let temp = tempfile::Builder::new()
        .prefix("ripsaw-convert-")
        .tempdir_in(&temp_parent)
        .with_context(|| {
            format!("creating temp dir under {}", temp_parent.display())
        })?;
    let h264_path = temp.path().join("track.h264");
    let cfg_path = temp.path().join("decoder.cfg");
    let yuv_base_stem = temp.path().join("output.yuv");
    let view0 = temp.path().join("output_ViewId0000.yuv");
    let view1 = temp.path().join("output_ViewId0001.yuv");

    match extractor {
        MvcExtractor::Mkvextract => {
            log(&event_tx, "Extracting H.264 track from MKV (mkvextract)...");
            let mkvextract_arg = format!("0:{}", h264_path.display());
            let status = Command::new("mkvextract")
                .arg(&plan.input)
                .arg("tracks")
                .arg(&mkvextract_arg)
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .status()
                .await
                .context("spawn mkvextract")?;
            if !status.success() {
                return Err(anyhow!("mkvextract failed with status {}", status));
            }
        }
        MvcExtractor::Builtin => {
            log(
                &event_tx,
                "Extracting H.264 + MVC dep-view via built-in mvcC walker...",
            );
            let input = plan.input.clone();
            let out_path = h264_path.clone();
            // The walker is synchronous (std::io); run it on a blocking
            // thread so we don't stall the runtime on large MKVs.
            let stats = tokio::task::spawn_blocking(move || -> Result<_> {
                let file = std::fs::File::open(&input)
                    .with_context(|| format!("opening {}", input.display()))?;
                let mut reader = libmvc::ebml::EbmlReader::new(file);
                let out_file = std::fs::File::create(&out_path)
                    .with_context(|| format!("creating {}", out_path.display()))?;
                let mut writer = std::io::BufWriter::new(out_file);
                let stats = libmvc::mkv_extract::extract_to_annex_b(
                    &mut reader,
                    &mut writer,
                )?;
                use std::io::Write;
                writer.flush().ok();
                Ok(stats)
            })
            .await
            .context("mvcC extractor thread panicked")??;
            log(
                &event_tx,
                &format!(
                    "Extracted {} frames ({} base + {} dep NALs)",
                    stats.frames, stats.base_nals, stats.dep_nals
                ),
            );
        }
    }

    // Decode with libmvc (pure Rust, bit-exact vs JM on the real disc),
    // streaming composed full-SBS frames straight into ffmpeg — so the pipeline
    // needs no tens-of-GB intermediate YUV on disk. If libmvc can't handle the
    // stream (an unsupported feature, or a panic), fall back to the JM reference
    // decoder, which writes per-view YUV files that ffmpeg then composes.
    let video: Box<dyn std::io::Read + Send> = Box::new(std::io::BufReader::new(
        std::fs::File::open(&h264_path).with_context(|| format!("opening {}", h264_path.display()))?,
    ));
    match decode_pipe_encode(plan, &event_tx, video, &plan.input, width, height, &frame_rate).await? {
        PipeOutcome::Success => return Ok(plan.output.clone()),
        PipeOutcome::DecodeFailed(e) => {
            log(
                &event_tx,
                &format!("libmvc decode failed ({e}); falling back to the JM reference decoder…"),
            );
            let cfg = build_decoder_cfg(&h264_path, &yuv_base_stem);
            tokio::fs::write(&cfg_path, cfg).await.context("writing decoder.cfg")?;
            let ldecod = resolve_ldecod_path()
                .with_context(|| format!("libmvc decode failed ({e}) and no JM ldecod fallback found"))?;
            log(
                &event_tx,
                &format!("Decoding MVC via {} (slow, JM reference decoder)…", ldecod.display()),
            );
            let mut child = Command::new(&ldecod)
                .arg("-f")
                .arg(&cfg_path)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .context("spawn ldecod")?;
            forward_stderr(&mut child, event_tx.clone());
            forward_stdout(&mut child, event_tx.clone());
            let status = child.wait().await.context("waiting for ldecod")?;
            if !status.success() {
                return Err(anyhow!("ldecod exited with status {}", status));
            }
        }
    }

    // Reached only via the ldecod fallback (the piped libmvc path returns above).
    if !view0.exists() || !view1.exists() {
        return Err(anyhow!(
            "MVC decode produced no dependent view output -- the source may not contain MVC NALs"
        ));
    }

    log(&event_tx, "Composing stereo output and encoding...");
    let compose = compose_filter(plan.format);
    let video_size = format!("{width}x{height}");
    let encoder = resolve_encoder_args(plan, event_tx.as_ref());
    // The compose filter ends with the labeled output [v]. If the
    // chosen encoder requires a follow-on filter chain (e.g. VAAPI's
    // format=nv12,hwupload), append it to that output label.
    let filter_complex = match &encoder.pre_input_vf {
        Some(extra) => {
            // compose ends with the `[v]` output label; rewrite it
            // to `[vraw]` and pipe through the encoder's pre-input
            // filter (e.g. format=nv12,hwupload) into the final
            // `[v]` that `-map` consumes.
            let intermediate = compose.trim_end_matches("[v]");
            format!("{intermediate}[vraw];[vraw]{extra}[v]")
        }
        None => compose,
    };

    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-y").arg("-hide_banner");
    for a in &encoder.init {
        cmd.arg(a);
    }
    cmd.arg("-f").arg("rawvideo")
        .arg("-pixel_format").arg("yuv420p")
        .arg("-video_size").arg(&video_size)
        .arg("-framerate").arg(&frame_rate)
        .arg("-i").arg(&view0)
        .arg("-f").arg("rawvideo")
        .arg("-pixel_format").arg("yuv420p")
        .arg("-video_size").arg(&video_size)
        .arg("-framerate").arg(&frame_rate)
        .arg("-i").arg(&view1)
        .arg("-i").arg(&plan.input)
        .arg("-filter_complex").arg(&filter_complex)
        .arg("-map").arg("[v]")
        .arg("-map").arg("2:a?")
        .arg("-map").arg("2:s?");
    for a in &encoder.encoder_args {
        cmd.arg(a);
    }
    cmd.arg("-c:a").arg("copy").arg("-c:s").arg("copy");
    apply_stereo_mode_tag(&mut cmd, plan.format);
    let status = cmd
        .arg(&plan.output)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .status()
        .await
        .context("spawn ffmpeg compose")?;
    if !status.success() {
        return Err(anyhow!("ffmpeg compose failed with status {}", status));
    }

    Ok(plan.output.clone())
}

/// Result of the piped libmvc decode+encode: either it finished the whole
/// output, or the decode failed and the caller should fall back to ldecod.
enum PipeOutcome {
    Success,
    DecodeFailed(anyhow::Error),
}

/// Reads a sequence of SSIF files as one continuous Annex B MVC stream — each
/// de-interleaved by libmvc's `SsifReader`, concatenated in play order for a
/// multi-clip feature. SSIFs are opened lazily (one at a time) to keep memory
/// bounded; each clip begins with its own parameter sets + IDR, so the join is
/// clean for the decoder.
struct SeqSsifReader {
    remaining: std::vec::IntoIter<PathBuf>,
    cur: Option<libmvc::ssif::SsifReader<std::io::BufReader<std::fs::File>>>,
}

impl SeqSsifReader {
    fn open(paths: Vec<PathBuf>) -> std::io::Result<Self> {
        let mut remaining = paths.into_iter();
        let cur = remaining.next().map(libmvc::ssif::SsifReader::open).transpose()?;
        Ok(SeqSsifReader { remaining, cur })
    }
}

impl std::io::Read for SeqSsifReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            match self.cur.as_mut() {
                None => return Ok(0),
                Some(r) => {
                    let n = r.read(buf)?;
                    if n > 0 {
                        return Ok(n);
                    }
                    // Current clip exhausted — advance to the next SSIF.
                    self.cur = self.remaining.next().map(libmvc::ssif::SsifReader::open).transpose()?;
                }
            }
        }
    }
}

/// Convert a Blu-ray 3D feature straight to packed-stereo output, with **no
/// makemkvcon and no intermediate MKV**. `clips` is the feature's ordered
/// `(ssif, m2ts)` pairs (from [`crate::rip::bd_playlist`]). libmvc's
/// [`libmvc::ssif::SsifReader`] de-interleaves each SSIF and the clips are
/// decoded as one continuous stream into full-side-by-side frames (bit-exact vs
/// JM), streamed into ffmpeg; audio and subtitles are muxed from the clips'
/// base `.m2ts` (concatenated via the `concat:` protocol for multi-clip
/// features). `plan` supplies the output path, layout, codec and HW backend.
/// Only works for unencrypted discs (no AACS).
pub async fn convert_bd_ssif(
    clips: &[(PathBuf, PathBuf)],
    plan: &ConversionPlan,
    event_tx: Option<tokio::sync::mpsc::Sender<ConversionEvent>>,
) -> Result<PathBuf> {
    let (_, first_m2ts) = clips.first().ok_or_else(|| anyhow!("no SSIF clips to convert"))?;
    ensure_parent_dir(&plan.output).await?;
    // Per-view geometry + frame rate from the first clip's base m2ts (all clips
    // share geometry; the widest video stream skips the dimensionless MVC one).
    let report = ffprobe::probe(first_m2ts).await?;
    let vstream = report
        .video_streams()
        .max_by_key(|v| v.width.unwrap_or(0))
        .ok_or_else(|| anyhow!("no video stream in {}", first_m2ts.display()))?;
    let width = vstream.width.unwrap_or(1920);
    let height = vstream.height.unwrap_or(1080);
    let frame_rate = vstream.r_frame_rate.clone().unwrap_or_else(|| "24000/1001".to_string());

    log(
        &event_tx,
        &format!("Decoding {} SSIF clip(s) natively (libmvc) — no makemkvcon, no intermediate…", clips.len()),
    );
    let ssifs: Vec<PathBuf> = clips.iter().map(|(s, _)| s.clone()).collect();
    let video: Box<dyn std::io::Read + Send> =
        Box::new(SeqSsifReader::open(ssifs).context("opening the feature's SSIF clips")?);

    // ffmpeg reads audio/subtitles from the base m2ts — one path, or the clips
    // joined via the `concat:` protocol (m2ts are byte-concatenable TS).
    let av_arg: std::ffi::OsString = if clips.len() == 1 {
        clips[0].1.clone().into_os_string()
    } else {
        let joined = clips.iter().map(|(_, m)| m.display().to_string()).collect::<Vec<_>>().join("|");
        format!("concat:{joined}").into()
    };

    match decode_pipe_encode(plan, &event_tx, video, Path::new(&av_arg), width, height, &frame_rate).await? {
        PipeOutcome::Success => Ok(plan.output.clone()),
        PipeOutcome::DecodeFailed(e) => Err(e.context("decoding the Blu-ray SSIF")),
    }
}

/// Decode the extracted MVC stream with libmvc and stream composed full-SBS
/// frames straight into ffmpeg over a pipe — no intermediate YUV on disk. The
/// decoder (blocking, CPU-bound) and ffmpeg run concurrently: the decoder writes
/// one full-SBS frame per AU into the pipe, ffmpeg derives the requested layout
/// from it and muxes audio/subtitles from the original input. Returns
/// `DecodeFailed` (→ ldecod fallback) if libmvc errors or panics; propagates a
/// genuine ffmpeg/setup error.
async fn decode_pipe_encode(
    plan: &ConversionPlan,
    event_tx: &Option<tokio::sync::mpsc::Sender<ConversionEvent>>,
    // The FSBS video source: a reader that libmvc decodes into full-side-by-side
    // frames — an Annex B track file for the MKV path, or an `SsifReader` reading
    // a Blu-ray SSIF directly.
    video: Box<dyn std::io::Read + Send>,
    // What ffmpeg reads audio + subtitles from (input #1): the source MKV, or the
    // title's `.m2ts` for the SSIF path.
    av_source: &Path,
    width: u32,
    height: u32,
    frame_rate: &str,
) -> Result<PipeOutcome> {
    log(event_tx, "Decoding MVC natively (libmvc), streaming into ffmpeg…");

    let (pipe_reader, pipe_writer) = std::io::pipe().context("creating decode→ffmpeg pipe")?;

    let encoder = resolve_encoder_args(plan, event_tx.as_ref());
    let compose = compose_filter_fsbs(plan.format);
    let filter_complex = match &encoder.pre_input_vf {
        Some(extra) => {
            let intermediate = compose.trim_end_matches("[v]");
            format!("{intermediate}[vraw];[vraw]{extra}[v]")
        }
        None => compose,
    };
    // CSC fusion: when the encoder wants NV12 (VAAPI's `format=nv12,hwupload`),
    // the packer emits NV12 directly so ffmpeg's `format=nv12` is a no-op — no
    // separate swscale colour-space pass. Software encoders take yuv420p.
    let pixfmt = match &encoder.pre_input_vf {
        Some(vf) if vf.contains("nv12") => libmvc::clip::FsbsPixFmt::Nv12,
        _ => libmvc::clip::FsbsPixFmt::Yuv420p,
    };
    // One full-SBS frame per AU: 2×(per-view width) × height.
    let fsbs_size = format!("{}x{}", width * 2, height);

    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-y").arg("-hide_banner");
    for a in &encoder.init {
        cmd.arg(a);
    }
    cmd.arg("-f").arg("rawvideo")
        .arg("-pixel_format").arg(pixfmt.ffmpeg_name())
        .arg("-video_size").arg(&fsbs_size)
        .arg("-framerate").arg(frame_rate)
        .arg("-i").arg("pipe:0")
        .arg("-i").arg(av_source)
        .arg("-filter_complex").arg(&filter_complex)
        .arg("-map").arg("[v]")
        .arg("-map").arg("1:a?")
        .arg("-map").arg("1:s?");
    for a in &encoder.encoder_args {
        cmd.arg(a);
    }
    cmd.arg("-c:a").arg("copy").arg("-c:s").arg("copy");
    apply_stereo_mode_tag(&mut cmd, plan.format);
    let mut child = cmd
        .arg(&plan.output)
        .stdin(Stdio::from(pipe_reader))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("spawn ffmpeg (piped compose)")?;
    forward_stderr(&mut child, event_tx.clone());

    // Decode on a blocking thread, writing composed FSBS frames into the pipe.
    // When the closure returns, `pipe_writer` drops → EOF → ffmpeg finalises.
    let decode = tokio::task::spawn_blocking(move || -> Result<libmvc::clip::ClipInfo> {
        libmvc::clip::decode_annex_b_to_fsbs_writer(video, pipe_writer, pixfmt)
    });

    // Await the decode. ffmpeg consumes the pipe concurrently at the OS level and
    // its stderr is drained by forward_stderr, so neither side can deadlock. A
    // JoinError means a panic inside libmvc — treat it as a decode failure.
    let decode_res = match decode.await {
        Ok(inner) => inner,
        Err(join_err) => Err(anyhow!("libmvc panicked while decoding: {join_err}")),
    };

    match decode_res {
        Err(e) => {
            // Decode failed: the writer has dropped, so ffmpeg sees EOF and exits
            // on the partial input. Reap it (its output is discarded) and report
            // the decode error so the caller falls back to ldecod.
            let _ = child.wait().await;
            Ok(PipeOutcome::DecodeFailed(e))
        }
        Ok(info) => {
            log(
                event_tx,
                &format!("Decoded {} frames natively ({}×{} per view); encoding…", info.frames, info.width, info.height),
            );
            let status = child.wait().await.context("waiting for ffmpeg (piped compose)")?;
            if !status.success() {
                return Err(anyhow!("ffmpeg compose failed with status {}", status));
            }
            Ok(PipeOutcome::Success)
        }
    }
}

/// Filtergraph deriving the requested stereo layout from a single full-SBS input
/// `[0:v]` (2×width × height, view 0 left / view 1 right) — the form the piped
/// libmvc decoder emits. Counterpart of [`compose_filter`], which takes the two
/// views as separate inputs (the ldecod file path).
fn compose_filter_fsbs(format: OutputFormat) -> String {
    match format {
        // Already full-SBS; pass through ([v] label needed for -map / pre-input vf).
        OutputFormat::FullSbs => "[0:v]null[v]".into(),
        // Halve the packed width: 2W→W keeps each view at W/2.
        OutputFormat::HalfSbs => "[0:v]scale=iw/2:ih[v]".into(),
        // Split the halves and stack them vertically.
        OutputFormat::FullTab => "[0:v]crop=iw/2:ih:0:0[l];[0:v]crop=iw/2:ih:iw/2:0[r];[l][r]vstack=inputs=2[v]".into(),
        OutputFormat::HalfTab => "[0:v]crop=iw/2:ih:0:0,scale=iw:ih/2[t];[0:v]crop=iw/2:ih:iw/2:0,scale=iw:ih/2[b];[t][b]vstack=inputs=2[v]".into(),
        OutputFormat::FrameSequential => "[0:v]crop=iw/2:ih:0:0[l];[0:v]crop=iw/2:ih:iw/2:0[r];[l][r]framepack=frameseq[v]".into(),
    }
}

fn build_decoder_cfg(input: &Path, output_yuv: &Path) -> String {
    format!(
        "InputFile = \"{input}\"\nOutputFile = \"{output}\"\nWriteUV = 1\nFileFormat = 0\nRefOffset = 0\nPOCScale = 2\nDisplayDecParams = 0\nConcealMode = 0\nRefPOCGap = 2\nPOCGap = 2\nSilent = 1\nIntraProfileDeblocking = 1\nDecFrmNum = 0\nDecodeAllLayers = 1\n",
        input = input.display(),
        output = output_yuv.display(),
    )
}

/// Tag the output video track with its Matroska `StereoMode` so 3D-aware
/// players split the frame per eye and render soft subtitles into both eyes.
/// A no-op for layouts with no spatial packing (frame-sequential). Must be
/// added before the output filename (it's an output-file option) and targets
/// output video stream 0 — the composed `[v]` we always map first.
fn apply_stereo_mode_tag(cmd: &mut Command, format: OutputFormat) {
    if let Some(mode) = format.matroska_stereo_mode() {
        cmd.arg("-metadata:s:v:0").arg(format!("stereo_mode={mode}"));
    }
}

fn compose_filter(format: OutputFormat) -> String {
    match format {
        OutputFormat::FullSbs => "[0:v][1:v]hstack=inputs=2[v]".into(),
        OutputFormat::HalfSbs => {
            // halve each view's width then hstack.
            "[0:v]scale=iw/2:ih[l];[1:v]scale=iw/2:ih[r];[l][r]hstack=inputs=2[v]".into()
        }
        OutputFormat::FullTab => "[0:v][1:v]vstack=inputs=2[v]".into(),
        OutputFormat::HalfTab => {
            "[0:v]scale=iw:ih/2[t];[1:v]scale=iw:ih/2[b];[t][b]vstack=inputs=2[v]".into()
        }
        OutputFormat::FrameSequential => {
            // Frame-sequential: alternate L/R frames (framepack doubles the rate).
            "[0:v][1:v]framepack=frameseq[v]".into()
        }
    }
}

fn resolve_ldecod_path() -> Result<PathBuf> {
    // 1. RIPSAW_LDECOD env var beats everything. Always honour an
    //    explicit user override first.
    if let Some(raw) = std::env::var_os("RIPSAW_LDECOD") {
        let p = PathBuf::from(raw);
        if p.is_file() {
            return Ok(p);
        }
    }
    // 2. Compile-time CARGO_MANIFEST_DIR / scripts / ldecod. Works
    //    for a freshly-built binary in the project directory; becomes
    //    stale if the project directory is renamed after build (this
    //    is what bit us going 3drip -> ripsaw; the embedded path
    //    didn't move with the source tree).
    let manifest_wrapper =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scripts/ldecod");
    if manifest_wrapper.is_file() {
        return Ok(manifest_wrapper);
    }
    // 3. Runtime sibling of the running binary: walk up from
    //    current_exe() looking for a scripts/ldecod sibling. For a
    //    cargo target/release/ripsaw, three parents up is the project
    //    root. We walk up to five levels so target/release/deps and
    //    other cargo layouts work too.
    if let Ok(exe) = std::env::current_exe() {
        let mut cursor = exe.parent().map(|p| p.to_path_buf());
        for _ in 0..5 {
            match cursor {
                Some(dir) => {
                    let candidate = dir.join("scripts").join("ldecod");
                    if candidate.is_file() {
                        return Ok(candidate);
                    }
                    cursor = dir.parent().map(|p| p.to_path_buf());
                }
                None => break,
            }
        }
    }
    // 4. Bare `ldecod` on PATH (system install).
    if let Ok(path) = which::which("ldecod") {
        return Ok(path);
    }
    // 5. Last-ditch: a few well-known fixed locations.
    for fallback in [
        "/usr/local/bin/ldecod",
        "/opt/jm/bin/ldecod",
        "/home/rob/3rdparty/JM/bin/umake/gcc-15.2/x86_64/release/ldecod",
    ] {
        let p = PathBuf::from(fallback);
        if p.is_file() {
            return Ok(p);
        }
    }
    Err(anyhow!(
        "ldecod not found. Set RIPSAW_LDECOD=/path/to/ldecod, install the \
         scripts/ldecod wrapper next to the binary, or put ldecod on PATH. \
         Build instructions are in docs/mvc3d.md § 'Build / tooling state'."
    ))
}

/// Resolve the plan's encoder selection to concrete ffmpeg argv. Probes
/// the host for supported HW backends once per call and feeds the chosen
/// (codec, backend) into hw::encoder_args. Logs the resolution so the
/// rip progress page shows which encoder the run actually used.
fn resolve_encoder_args(
    plan: &ConversionPlan,
    event_tx: Option<&tokio::sync::mpsc::Sender<ConversionEvent>>,
) -> crate::convert::hw::EncoderArgs {
    use crate::convert::hw::{encoder_args, encoder_smoke_test, probe_hw_support, HwBackend};
    let support = probe_hw_support();
    let chosen = match plan.hw_backend {
        HwBackend::Auto => support.resolve_auto(plan.codec),
        explicit => {
            // If the user picked a specific backend that isn't actually
            // available on this box, fall back to software rather than
            // ffmpeg failing on a missing encoder.
            if support.supports(explicit, plan.codec) {
                explicit
            } else {
                if let Some(tx) = event_tx {
                    let _ = tx.try_send(ConversionEvent::Log(format!(
                        "HW backend {:?} unavailable for {:?}; falling back to software encode",
                        explicit, plan.codec
                    )));
                }
                HwBackend::Software
            }
        }
    };
    // Pre-flight: device-presence checks can still mis-pick a HW encoder
    // that fails to initialise (e.g. h264_qsv compiled in with no Intel
    // GPU). Since the encoder is now the default, validate it with a tiny
    // test encode and fall back to software rather than failing the whole
    // convert minutes in.
    let chosen = if chosen != HwBackend::Software
        && !encoder_smoke_test(chosen, plan.codec, support.vaapi_device.as_deref())
    {
        if let Some(tx) = event_tx {
            let _ = tx.try_send(ConversionEvent::Log(format!(
                "{} failed a pre-flight encode test; falling back to software encode",
                chosen.label()
            )));
        }
        HwBackend::Software
    } else {
        chosen
    };
    if let Some(tx) = event_tx {
        let _ = tx.try_send(ConversionEvent::Log(format!(
            "Encoding {:?} via {} ({})",
            plan.codec,
            chosen.label(),
            chosen
                .ffmpeg_encoder(plan.codec)
                .unwrap_or("libx264"),
        )));
    }
    encoder_args(chosen, plan.codec, 18, support.vaapi_device.as_deref())
}

async fn ensure_parent_dir(p: &Path) -> Result<()> {
    if let Some(parent) = p.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating output directory {}", parent.display()))?;
    }
    Ok(())
}

fn log(tx: &Option<tokio::sync::mpsc::Sender<ConversionEvent>>, msg: &str) {
    if let Some(tx) = tx {
        let _ = tx.try_send(ConversionEvent::Log(msg.to_string()));
    }
    tracing::info!("convert: {}", msg);
}

fn forward_stderr(
    child: &mut tokio::process::Child,
    tx: Option<tokio::sync::mpsc::Sender<ConversionEvent>>,
) {
    let Some(stderr) = child.stderr.take() else { return; };
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            if let Some(tx) = &tx {
                let _ = tx.try_send(ConversionEvent::Log(line));
            }
        }
    });
}

fn forward_stdout(
    child: &mut tokio::process::Child,
    tx: Option<tokio::sync::mpsc::Sender<ConversionEvent>>,
) {
    let Some(stdout) = child.stdout.take() else { return; };
    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            if let Some(tx) = &tx {
                let _ = tx.try_send(ConversionEvent::Log(line));
            }
        }
    });
}
