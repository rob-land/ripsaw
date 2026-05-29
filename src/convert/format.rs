// Output stereo layouts we know how to produce.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// Full side-by-side. 1920×1080 input → 3840×1080 output, no
    /// downsampling. Highest quality but largest file size.
    FullSbs,
    /// Half side-by-side. Each view downsampled horizontally to 960×1080
    /// and packed into 1920×1080. Most-compatible 3D BD output format.
    HalfSbs,
    /// Full top/bottom. 1920×1080 → 1920×2160, no downsampling.
    FullTab,
    /// Half top/bottom. Each view downsampled vertically to 1920×540.
    HalfTab,
    /// Frame-sequential. Each view emitted as its own frame at double
    /// the source frame rate. Same dimensions as input.
    FrameSequential,
}

impl OutputFormat {
    pub fn label(self) -> &'static str {
        match self {
            OutputFormat::FullSbs => "Full side-by-side (3840×1080)",
            OutputFormat::HalfSbs => "Half side-by-side (1920×1080)",
            OutputFormat::FullTab => "Full over/under (1920×2160)",
            OutputFormat::HalfTab => "Half over/under (1920×1080)",
            OutputFormat::FrameSequential => "Frame-sequential (2× fps)",
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            OutputFormat::FullSbs => "fsbs",
            OutputFormat::HalfSbs => "hsbs",
            OutputFormat::FullTab => "fou",
            OutputFormat::HalfTab => "hou",
            OutputFormat::FrameSequential => "fs",
        }
    }

    /// Compute the packed output (width, height) for a given input view
    /// size. The input dimensions are *one* view's dimensions, not the
    /// packed source.
    pub fn output_dimensions(self, view_w: u32, view_h: u32) -> (u32, u32) {
        match self {
            OutputFormat::FullSbs => (view_w * 2, view_h),
            OutputFormat::HalfSbs => (view_w, view_h),
            OutputFormat::FullTab => (view_w, view_h * 2),
            OutputFormat::HalfTab => (view_w, view_h),
            OutputFormat::FrameSequential => (view_w, view_h),
        }
    }

    /// The output `-vf stereo3d=…` argument value that ffmpeg uses when
    /// the *input* is already a packed stereo frame and we're remapping
    /// to this output layout. Source layout is added by the caller.
    pub fn ffmpeg_stereo3d_out(self) -> &'static str {
        match self {
            OutputFormat::FullSbs => "sbsl",
            OutputFormat::HalfSbs => "sbs2l",
            OutputFormat::FullTab => "abl",
            OutputFormat::HalfTab => "ab2l",
            OutputFormat::FrameSequential => "al",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_dimensions_double_axis_per_layout() {
        assert_eq!(OutputFormat::FullSbs.output_dimensions(1920, 1080), (3840, 1080));
        assert_eq!(OutputFormat::HalfSbs.output_dimensions(1920, 1080), (1920, 1080));
        assert_eq!(OutputFormat::FullTab.output_dimensions(1920, 1080), (1920, 2160));
        assert_eq!(OutputFormat::HalfTab.output_dimensions(1920, 1080), (1920, 1080));
        assert_eq!(OutputFormat::FrameSequential.output_dimensions(1920, 1080), (1920, 1080));
    }

    #[test]
    fn slugs_are_unique() {
        let all = [
            OutputFormat::FullSbs,
            OutputFormat::HalfSbs,
            OutputFormat::FullTab,
            OutputFormat::HalfTab,
            OutputFormat::FrameSequential,
        ];
        let slugs: std::collections::HashSet<&str> = all.iter().map(|f| f.slug()).collect();
        assert_eq!(slugs.len(), all.len(), "slugs collided: {slugs:?}");
    }
}
