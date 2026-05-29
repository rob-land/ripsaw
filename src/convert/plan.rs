// Conversion plan: input file + chosen output layout + output path.

use std::path::{Path, PathBuf};

use crate::convert::format::OutputFormat;

#[derive(Debug, Clone)]
pub struct ConversionPlan {
    pub input: PathBuf,
    pub output: PathBuf,
    pub format: OutputFormat,
    pub source: StereoSource,
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
    /// `/in/foo.mkv` and `FullSbs` returns `/in/foo.fsbs.mkv`.
    pub fn default_output_path(input: &Path, format: OutputFormat) -> PathBuf {
        let stem = input
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "output".to_string());
        let parent = input.parent().unwrap_or_else(|| Path::new("."));
        parent.join(format!("{stem}.{}.mkv", format.slug()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn default_output_path_adds_format_slug_before_extension() {
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
    fn default_output_path_for_top_level_input() {
        let p = ConversionPlan::default_output_path(
            Path::new("/x.mkv"),
            OutputFormat::HalfSbs,
        );
        assert_eq!(p, Path::new("/x.hsbs.mkv"));
    }
}
