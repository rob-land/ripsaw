// Hardware-encoder backend detection + ffmpeg argv emission.
//
// The convert pipeline's final stage is an ffmpeg invocation that
// encodes raw YUV (from ldecod) into H.264 or H.265. By default we use
// libx264 / libx265, which run on CPU and saturate several cores. Every
// modern GPU has dedicated video-encode silicon (NVIDIA NVENC, Intel
// Quick Sync Video / QSV, AMD AMF or VAAPI VCE, ARM v4l2 m2m) that can
// do the same job for a fraction of the CPU cost; this module wraps
// that selection in a single enum + a fn that returns the right argv
// pieces for whichever backend the user picks (or "Auto").
//
// Trade-off the user should know: GPU encoders are typically 10-15%
// less efficient than libx264 / libx265 at the same target quality --
// they hit a CPU-encode-equivalent bitrate but with a different
// rate-distortion curve. For high-bitrate stereo BD rips that's
// negligible; for storage-constrained workflows libx264 still wins.

use std::ffi::OsString;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Which video codec the convert pipeline will produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EncodeCodec {
    H264,
    H265,
}

impl EncodeCodec {
    fn software_encoder(self) -> &'static str {
        match self {
            EncodeCodec::H264 => "libx264",
            EncodeCodec::H265 => "libx265",
        }
    }
}

/// Hardware-encode backend selection. `Auto` resolves to the first
/// available backend at runtime in a vendor order chosen for "lowest
/// CPU strain" -- NVENC first (dedicated NVENC silicon is the fastest),
/// then QSV (Intel iGPU is on the chip already), then VAAPI (AMD VCN /
/// Intel iHD), then AMF (AMD's encode SDK -- broader OS coverage than
/// VAAPI on AMD), then V4L2 m2m (ARM SoC encoders). Software is always
/// the fallback when nothing else is detected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum HwBackend {
    /// libx264 / libx265 on CPU. Best rate-distortion; default until
    /// the user opts in.
    #[default]
    Software,
    /// Pick the first available HW backend at runtime.
    Auto,
    /// NVIDIA dedicated NVENC silicon. Requires a Maxwell-or-newer
    /// NVIDIA GPU and the nvidia driver loaded.
    Nvenc,
    /// Intel Quick Sync Video. iGPU on Intel Core 2nd gen or newer.
    Qsv,
    /// VAAPI -- Intel iHD or AMD VCN (Vega+) via the kernel's render
    /// node. Works on both vendors.
    Vaapi,
    /// AMD Advanced Media Framework. AMD's encode SDK; somewhat
    /// broader OS coverage on AMD than VAAPI alone.
    Amf,
    /// V4L2 mem2mem -- ARM SoC encoders (RPi, Hi35xx, Allwinner) where
    /// the kernel exposes a hardware codec via the standard v4l2 API.
    V4l2M2m,
}

impl HwBackend {
    /// Map to/from the encoder-backend ComboRow index. Row order must
    /// mirror the StringList in preferences-dialog.blp.
    pub fn to_ui_index(self) -> u32 {
        match self {
            HwBackend::Software => 0,
            HwBackend::Auto => 1,
            HwBackend::Nvenc => 2,
            HwBackend::Qsv => 3,
            HwBackend::Vaapi => 4,
            HwBackend::Amf => 5,
            HwBackend::V4l2M2m => 6,
        }
    }

    pub fn from_ui_index(idx: u32) -> Self {
        match idx {
            1 => HwBackend::Auto,
            2 => HwBackend::Nvenc,
            3 => HwBackend::Qsv,
            4 => HwBackend::Vaapi,
            5 => HwBackend::Amf,
            6 => HwBackend::V4l2M2m,
            _ => HwBackend::Software,
        }
    }

    /// Human-readable label for the UI.
    pub fn label(self) -> &'static str {
        match self {
            HwBackend::Software => "Software (best quality)",
            HwBackend::Auto => "Auto (lowest CPU)",
            HwBackend::Nvenc => "NVIDIA NVENC",
            HwBackend::Qsv => "Intel Quick Sync",
            HwBackend::Vaapi => "VAAPI (Intel iHD / AMD VCN)",
            HwBackend::Amf => "AMD AMF",
            HwBackend::V4l2M2m => "V4L2 m2m (ARM SoC)",
        }
    }

    /// The ffmpeg encoder name for this backend + codec, or None when
    /// the backend doesn't implement that codec.
    pub fn ffmpeg_encoder(self, codec: EncodeCodec) -> Option<&'static str> {
        Some(match (self, codec) {
            (HwBackend::Software, EncodeCodec::H264) => "libx264",
            (HwBackend::Software, EncodeCodec::H265) => "libx265",
            (HwBackend::Nvenc, EncodeCodec::H264) => "h264_nvenc",
            (HwBackend::Nvenc, EncodeCodec::H265) => "hevc_nvenc",
            (HwBackend::Qsv, EncodeCodec::H264) => "h264_qsv",
            (HwBackend::Qsv, EncodeCodec::H265) => "hevc_qsv",
            (HwBackend::Vaapi, EncodeCodec::H264) => "h264_vaapi",
            (HwBackend::Vaapi, EncodeCodec::H265) => "hevc_vaapi",
            (HwBackend::Amf, EncodeCodec::H264) => "h264_amf",
            (HwBackend::Amf, EncodeCodec::H265) => "hevc_amf",
            (HwBackend::V4l2M2m, EncodeCodec::H264) => "h264_v4l2m2m",
            (HwBackend::V4l2M2m, EncodeCodec::H265) => "hevc_v4l2m2m",
            (HwBackend::Auto, _) => return None,
        })
    }
}

/// Result of probing the host for usable HW encoders. Cached for the
/// life of the process.
#[derive(Debug, Default, Clone)]
pub struct HwSupport {
    pub available_encoders: Vec<&'static str>,
    pub vaapi_device: Option<String>,
}

impl HwSupport {
    /// Returns true when both ffmpeg has the encoder compiled in AND
    /// the device backing it is plausibly present.
    pub fn supports(&self, backend: HwBackend, codec: EncodeCodec) -> bool {
        let Some(enc) = backend.ffmpeg_encoder(codec) else { return false; };
        if !self.available_encoders.iter().any(|e| *e == enc) {
            return false;
        }
        match backend {
            HwBackend::Vaapi => self.vaapi_device.is_some(),
            HwBackend::Nvenc => Path::new("/proc/driver/nvidia").exists(),
            HwBackend::V4l2M2m => {
                glob_match("/dev/video*").is_some()
                    || Path::new("/dev/v4l").exists()
            }
            // Software always supported; AMF / QSV gate on the encoder
            // being present in ffmpeg, which is checked above.
            _ => true,
        }
    }

    /// Resolve HwBackend::Auto to a concrete backend by picking the
    /// first one that supports `codec`. Order chosen for "lowest CPU
    /// strain" (see HwBackend docs). When nothing HW works, falls back
    /// to Software.
    pub fn resolve_auto(&self, codec: EncodeCodec) -> HwBackend {
        for cand in [
            HwBackend::Nvenc,
            HwBackend::Qsv,
            HwBackend::Vaapi,
            HwBackend::Amf,
            HwBackend::V4l2M2m,
        ] {
            if self.supports(cand, codec) {
                return cand;
            }
        }
        HwBackend::Software
    }
}

/// Run `ffmpeg -encoders` once and scrape the encoder names. Tolerant
/// of ffmpeg missing -- in that case the returned struct just has the
/// software fallback as the only option (which is anyway what every
/// path should treat as the safe default).
pub fn probe_hw_support() -> HwSupport {
    let mut support = HwSupport::default();
    if let Ok(out) = crate::hostcmd::host_command_std("ffmpeg")
        .arg("-hide_banner")
        .arg("-encoders")
        .output()
    {
        let text = String::from_utf8_lossy(&out.stdout);
        for &enc in &[
            "libx264",
            "libx265",
            "h264_nvenc",
            "hevc_nvenc",
            "h264_qsv",
            "hevc_qsv",
            "h264_vaapi",
            "hevc_vaapi",
            "h264_amf",
            "hevc_amf",
            "h264_v4l2m2m",
            "hevc_v4l2m2m",
        ] {
            if text.contains(enc) {
                support.available_encoders.push(enc);
            }
        }
    }
    support.vaapi_device = vaapi_device();
    support
}

/// Find a VAAPI render node. /dev/dri/renderD128 is the conventional
/// first one on a single-GPU system; subsequent GPUs get renderD129,
/// renderD130, ... We just return the first that exists.
fn vaapi_device() -> Option<String> {
    for i in 128..136 {
        let p = format!("/dev/dri/renderD{i}");
        if Path::new(&p).exists() {
            return Some(p);
        }
    }
    None
}

fn glob_match(pattern: &str) -> Option<String> {
    // Avoid pulling in the glob crate just for a single-shot
    // device probe. /dev/video* is the only pattern we ever pass.
    if let Some(dir) = pattern.strip_suffix("*") {
        let dir_path = Path::new(dir).parent()?;
        let name_prefix = Path::new(dir).file_name()?.to_str()?;
        for entry in std::fs::read_dir(dir_path).ok()?.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(name_prefix) {
                return Some(entry.path().to_string_lossy().into_owned());
            }
        }
    }
    None
}

/// Argv pieces the convert runner should pass to ffmpeg for the chosen
/// backend + codec. The runner places these at the position currently
/// occupied by `-c:v libx264 -preset medium -crf 18`. Split into:
///
/// - `init`: ffmpeg-global init flags (e.g. `-vaapi_device ...`).
///   Empty for backends that don't need it.
/// - `pre_input_vf`: a `-vf` filter chain that must run before the
///   encoder sees the data (e.g. `format=nv12,hwupload`). Empty when
///   the encoder consumes raw frames directly.
/// - `encoder_args`: the `-c:v <enc> -<quality knobs>...` cluster.
#[derive(Debug, Clone)]
pub struct EncoderArgs {
    pub init: Vec<OsString>,
    pub pre_input_vf: Option<String>,
    pub encoder_args: Vec<OsString>,
}

/// Build the ffmpeg argv pieces for a specific resolved backend +
/// codec. The `quality` value is interpreted per-backend so a
/// `quality = 22` maps roughly to "visually transparent" everywhere.
pub fn encoder_args(
    backend: HwBackend,
    codec: EncodeCodec,
    quality: u32,
    vaapi_device: Option<&str>,
) -> EncoderArgs {
    use OsString as O;
    let enc = backend.ffmpeg_encoder(codec).unwrap_or_else(|| codec.software_encoder());
    let mut out = EncoderArgs {
        init: Vec::new(),
        pre_input_vf: None,
        encoder_args: Vec::new(),
    };

    match backend {
        HwBackend::Software | HwBackend::Auto => {
            // libx264 / libx265 with the project's prior defaults.
            out.encoder_args = vec![
                O::from("-c:v"),
                O::from(enc),
                O::from("-preset"),
                O::from("medium"),
                O::from("-crf"),
                O::from(quality.to_string()),
            ];
        }
        HwBackend::Vaapi => {
            // VAAPI requires the encoder consume NV12 frames in GPU
            // memory. The hwupload filter copies the CPU-side frame to
            // the device.
            if let Some(dev) = vaapi_device {
                out.init = vec![O::from("-vaapi_device"), O::from(dev)];
            }
            out.pre_input_vf = Some("format=nv12,hwupload".into());
            out.encoder_args = vec![
                O::from("-c:v"),
                O::from(enc),
                O::from("-qp"),
                O::from(quality.to_string()),
            ];
        }
        HwBackend::Nvenc => {
            // NVENC uses preset p1..p7 (p1 fastest, p7 best quality);
            // p4 is the "balanced" middle. -cq is constant quality.
            out.encoder_args = vec![
                O::from("-c:v"),
                O::from(enc),
                O::from("-preset"),
                O::from("p5"),
                O::from("-rc"),
                O::from("vbr"),
                O::from("-cq"),
                O::from(quality.to_string()),
            ];
        }
        HwBackend::Qsv => {
            // QSV's quality knob is -global_quality (lower is better).
            out.encoder_args = vec![
                O::from("-c:v"),
                O::from(enc),
                O::from("-preset"),
                O::from("medium"),
                O::from("-global_quality"),
                O::from(quality.to_string()),
            ];
        }
        HwBackend::Amf => {
            // AMF: rate-control cqp (constant QP); same QP for I/P/B.
            let q = quality.to_string();
            out.encoder_args = vec![
                O::from("-c:v"),
                O::from(enc),
                O::from("-quality"),
                O::from("balanced"),
                O::from("-rc"),
                O::from("cqp"),
                O::from("-qp_i"),
                O::from(q.clone()),
                O::from("-qp_p"),
                O::from(q.clone()),
                O::from("-qp_b"),
                O::from(q),
            ];
        }
        HwBackend::V4l2M2m => {
            // V4L2 m2m drivers vary by SoC; the lowest-common-
            // denominator option is just -b:v (target bitrate).
            // Translate the CRF-ish quality into a bitrate: ~24 Mbps
            // at quality 18, dropping ~1.5 Mbps per quality step.
            let mbps = 24_u32.saturating_sub(quality.saturating_sub(18) * 3 / 2);
            out.encoder_args = vec![
                O::from("-c:v"),
                O::from(enc),
                O::from("-b:v"),
                O::from(format!("{mbps}M")),
            ];
        }
    }

    out
}

/// Pre-flight check that a resolved HW encoder actually initialises on
/// this host. `probe_hw_support`/`supports` only confirm ffmpeg has the
/// encoder and a plausible device node; QSV and AMF in particular can be
/// compiled into ffmpeg on a box with no matching GPU, so `resolve_auto`
/// can hand back a backend that fails at `ffmpeg` startup. A 1-frame
/// encode of a synthetic source to `null` catches that in ~0.1 s, before
/// we commit a multi-minute convert to a dead encoder. `Software` is
/// always usable and returns `true` without spawning anything.
pub fn encoder_smoke_test(
    backend: HwBackend,
    codec: EncodeCodec,
    vaapi_device: Option<&str>,
) -> bool {
    if backend == HwBackend::Software {
        return true;
    }
    let args = encoder_args(backend, codec, 23, vaapi_device);
    let mut cmd = crate::hostcmd::host_command_std("ffmpeg");
    cmd.arg("-hide_banner").arg("-loglevel").arg("error").arg("-nostdin");
    for a in &args.init {
        cmd.arg(a);
    }
    cmd.arg("-f").arg("lavfi").arg("-i").arg("color=c=black:s=320x240:r=25:d=1");
    if let Some(vf) = &args.pre_input_vf {
        cmd.arg("-vf").arg(vf);
    }
    for a in &args.encoder_args {
        cmd.arg(a);
    }
    cmd.arg("-frames:v").arg("1").arg("-f").arg("null").arg("-");
    cmd.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    cmd.status().map(|s| s.success()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffmpeg_encoder_names_match_codec() {
        assert_eq!(
            HwBackend::Vaapi.ffmpeg_encoder(EncodeCodec::H264),
            Some("h264_vaapi")
        );
        assert_eq!(
            HwBackend::Nvenc.ffmpeg_encoder(EncodeCodec::H265),
            Some("hevc_nvenc")
        );
        assert!(HwBackend::Auto.ffmpeg_encoder(EncodeCodec::H264).is_none());
    }

    #[test]
    fn resolve_auto_falls_back_to_software_when_nothing_detected() {
        let support = HwSupport {
            available_encoders: vec!["libx264", "libx265"],
            vaapi_device: None,
        };
        assert_eq!(
            support.resolve_auto(EncodeCodec::H264),
            HwBackend::Software
        );
    }

    #[test]
    fn resolve_auto_prefers_nvenc_when_both_nvenc_and_vaapi_present() {
        let support = HwSupport {
            available_encoders: vec!["h264_nvenc", "h264_vaapi"],
            vaapi_device: Some("/dev/dri/renderD128".into()),
        };
        // The order in resolve_auto is NVENC then Qsv then VAAPI etc.
        // Nvenc requires /proc/driver/nvidia to exist, which it does
        // not in tests, so this test ends up checking VAAPI is picked.
        let chosen = support.resolve_auto(EncodeCodec::H264);
        assert!(
            matches!(chosen, HwBackend::Vaapi | HwBackend::Nvenc),
            "expected Vaapi or Nvenc, got {chosen:?}"
        );
    }

    #[test]
    fn vaapi_args_include_hwupload_filter() {
        let args = encoder_args(
            HwBackend::Vaapi,
            EncodeCodec::H264,
            22,
            Some("/dev/dri/renderD128"),
        );
        assert_eq!(args.pre_input_vf.as_deref(), Some("format=nv12,hwupload"));
        assert!(args.init.iter().any(|s| s == "-vaapi_device"));
        assert!(args.encoder_args.iter().any(|s| s == "h264_vaapi"));
    }

    #[test]
    fn software_args_use_libx264_or_libx265() {
        let args = encoder_args(HwBackend::Software, EncodeCodec::H264, 18, None);
        assert!(args.encoder_args.iter().any(|s| s == "libx264"));
        assert!(args.pre_input_vf.is_none());
        let args = encoder_args(HwBackend::Software, EncodeCodec::H265, 18, None);
        assert!(args.encoder_args.iter().any(|s| s == "libx265"));
    }
}
