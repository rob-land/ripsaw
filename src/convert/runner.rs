// Drive a ConversionPlan to completion.
//
// Three paths today:
//
// 1. `AlreadyPacked` source: pure ffmpeg `stereo3d` filter conversion.
//    Works end-to-end right now.
// 2. `MvcWithBlockAdditions` / `MvcInlineLaced` source: returns
//    `Pending` with a clear "MVC decoder still under construction"
//    error. The runner contract is in place so the path will plug in
//    cleanly once `libmvc` exposes a `decode_to_yuv` API.
// 3. `NotStereo`: rejected up front.

use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::convert::plan::{ConversionPlan, StereoSource};

#[derive(Debug, Clone)]
pub enum ConversionEvent {
    /// Free-form line of stderr from ffmpeg. The UI's log expander tails
    /// these so the user can see progress / errors.
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
        StereoSource::MvcWithBlockAdditions | StereoSource::MvcInlineLaced => {
            Err(anyhow!(
                "MVC dependent-view decode is not implemented yet — see docs/mvc3d.md \
                 § \"Implementation phasing\" (phase 1). The conversion plan is wired \
                 through to this point; the missing piece is `libmvc::decode_to_yuv`."
            ))
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
    if let Some(parent) = plan.output.parent() {
        tokio::fs::create_dir_all(parent).await.with_context(|| {
            format!("creating output directory {}", parent.display())
        })?;
    }

    // `stereo3d=<in>:<out>` per ffmpeg docs. e.g. `stereo3d=sbsl:abl`
    // converts a side-by-side-left input to over/under-left.
    let filter = format!("stereo3d={}:{}", input_layout, plan.format.ffmpeg_stereo3d_out());

    let mut child = Command::new("ffmpeg")
        .arg("-hide_banner")
        .arg("-y")
        .arg("-i")
        .arg(&plan.input)
        .arg("-vf")
        .arg(&filter)
        .arg("-c:v")
        .arg("libx264")
        .arg("-preset")
        .arg("medium")
        .arg("-crf")
        .arg("18")
        .arg("-c:a")
        .arg("copy")
        .arg("-c:s")
        .arg("copy")
        .arg(&plan.output)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("spawn ffmpeg")?;

    if let Some(stderr) = child.stderr.take() {
        let tx = event_tx.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                if let Some(tx) = &tx {
                    let _ = tx.try_send(ConversionEvent::Log(line));
                }
            }
        });
    }

    let status = child.wait().await.context("waiting for ffmpeg")?;
    if !status.success() {
        return Err(anyhow!("ffmpeg exited with status {}", status));
    }
    Ok(plan.output.clone())
}
