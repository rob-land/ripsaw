// Conversion plan: input file + chosen output layout + output path.

use std::path::{Path, PathBuf};

use crate::convert::format::OutputFormat;
use crate::convert::hw::{EncodeCodec, HwBackend};

#[derive(Debug, Clone)]
pub struct ConversionPlan {
    pub input: PathBuf,
    pub output: PathBuf,
    pub format: OutputFormat,
    pub source: StereoSource,
    /// Which video codec to emit. Defaults to H.264 for compatibility.
    pub codec: EncodeCodec,
    /// CPU vs GPU encode selection. `HwBackend::Software` keeps the
    /// previous libx264 / libx265 behaviour; `Auto` resolves to the
    /// first available HW encoder at runtime.
    pub hw_backend: HwBackend,
}

impl ConversionPlan {
    /// Default output codec: H.265/HEVC — ~half the size at the same quality,
    /// played by current 3D targets. Mirrors `UserSettings::conversion_codec`.
    pub fn default_codec() -> EncodeCodec {
        EncodeCodec::H265
    }

    /// Default to `Auto`: probe for a hardware encoder (NVENC/QSV/VAAPI/
    /// …) and use it, falling back to software when none is present.
    /// Measured ~3–4× faster encode than libx264 on an Intel iGPU
    /// (docs/libmvc-injection.md § 5c), which is the dominant lever once
    /// the 3D decode is the convert bottleneck. Software stays one click
    /// away in the UI for users who want libx264's rate-distortion.
    pub fn default_hw_backend() -> HwBackend {
        HwBackend::Auto
    }
}

/// What kind of stereo encoding the input carries. Drives which
/// conversion path runner uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StereoSource {
    /// MakeMKV-style modern MKV: AVC base track plus an `mvcC`
    /// BlockAdditionMapping carrying the dependent view per frame.
    /// Needs `libmvc` to decode.
    MvcWithBlockAdditions,
    /// Matroska "both eyes laced" stereo mode 13/14. The dependent view
    /// is inline in the H.264 stream. Same MVC decoder dependency.
    MvcInlineLaced,
    /// Already packed into a single frame (stereo modes 1, 2, 3 — side
    /// by side or over/under). Convertible via ffmpeg's `stereo3d`
    /// filter without an MVC decoder.
    AlreadyPacked { input_layout: PackedInputLayout },
    /// We don't think there's any 3D content in this file.
    NotStereo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackedInputLayout {
    /// Side-by-side, left first.
    Sbsl,
    /// Top-bottom, left first.
    Abl,
}

impl PackedInputLayout {
    pub fn ffmpeg_stereo3d_in(self) -> &'static str {
        match self {
            PackedInputLayout::Sbsl => "sbsl",
            PackedInputLayout::Abl => "abl",
        }
    }
}

impl ConversionPlan {
    /// Generate a default output path next to the input. For
    /// `/in/foo.mkv` and `FullSbs` returns `/in/foo.3d.fsbs.mkv`.
    ///
    /// The `3d` token satisfies Kodi's filename flagging (its default
    /// regexes require a separated `3d` token before the packing token)
    /// and marks the file for humans/other tools; the packing slug is a
    /// Jellyfin-native token (see [`OutputFormat::slug`]). When the stem
    /// already carries a separated `3d`/`3D` token (e.g.
    /// `Avatar (2009) - 3D`), it isn't duplicated.
    pub fn default_output_path(input: &Path, format: OutputFormat) -> PathBuf {
        let stem = input
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "output".to_string());
        let parent = input.parent().unwrap_or_else(|| Path::new("."));
        let three_d = if has_3d_token(&stem) { "" } else { "3d." };
        parent.join(format!("{stem}.{three_d}{}.mkv", format.slug()))
    }
}

/// Whether `stem` already contains a separated `3d` token (Kodi-style
/// delimiters: space, dot, dash, underscore, brackets, parens).
fn has_3d_token(stem: &str) -> bool {
    let low = stem.to_ascii_lowercase();
    let is_sep = |c: char| " ._-[]()".contains(c);
    low.split(is_sep).any(|tok| tok == "3d")
}

/// Detect the StereoSource flavour of an MKV at `path`. Returns
/// `None` when the file isn't an MKV we can read, has no 3D info,
/// or carries already-packed stereo modes only (modes 1/2/3 → an
/// `AlreadyPacked` variant could be returned by a richer detector,
/// but for now MakeMKV-produced MVC sources are the only thing this
/// helper is used for).
pub fn detect_stereo_source(path: &Path) -> Option<StereoSource> {
    use libmvc::ebml::EbmlReader;
    use libmvc::mvcc::scan_3d_info;
    let file = std::fs::File::open(path).ok()?;
    let mut reader = EbmlReader::new(file);
    let info = scan_3d_info(&mut reader).ok()?;
    if info.mvcc_bytes.is_some() {
        Some(StereoSource::MvcWithBlockAdditions)
    } else if matches!(info.stereo_mode, Some(13) | Some(14)) {
        Some(StereoSource::MvcInlineLaced)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn default_output_path_keeps_existing_3d_token() {
        // Stem already carries "- 3D": don't duplicate the token.
        let p = ConversionPlan::default_output_path(
            Path::new("/lib/Movies/Avatar (2009)/Avatar (2009) - 3D.mkv"),
            OutputFormat::FullSbs,
        );
        assert_eq!(
            p,
            Path::new("/lib/Movies/Avatar (2009)/Avatar (2009) - 3D.fsbs.mkv")
        );
    }

    #[test]
    fn default_output_path_adds_3d_token_and_slug() {
        let p = ConversionPlan::default_output_path(
            Path::new("/x.mkv"),
            OutputFormat::HalfSbs,
        );
        assert_eq!(p, Path::new("/x.3d.hsbs.mkv"));
    }

    #[test]
    fn default_output_path_uses_jellyfin_tab_tokens() {
        let p = ConversionPlan::default_output_path(
            Path::new("/x.mkv"),
            OutputFormat::HalfTab,
        );
        // htab (not the old hou): a Jellyfin Format3DParser token.
        assert_eq!(p, Path::new("/x.3d.htab.mkv"));
    }

    #[test]
    fn stray_3d_substrings_do_not_suppress_the_token() {
        // "3Delight" is not a separated 3d token.
        let p = ConversionPlan::default_output_path(
            Path::new("/lib/3Delight.mkv"),
            OutputFormat::FullSbs,
        );
        assert_eq!(p, Path::new("/lib/3Delight.3d.fsbs.mkv"));
    }
}
