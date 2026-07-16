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

    /// The Matroska `StereoMode` tag value (as ffmpeg's matroska muxer names
    /// it) that declares this packing on the output video track, so a
    /// 3D-aware player (Kodi, VR players, 3D TVs) splits the frame per eye —
    /// and, crucially, renders soft/toggleable subtitles into *both* eyes with
    /// a depth offset instead of onto one half. Set via
    /// `-metadata:s:v:0 stereo_mode=<value>`.
    ///
    /// Both SBS layouts are left-eye-first side-by-side (`left_right`, Matroska
    /// value 1); both TAB layouts are left-eye-on-top (`top_bottom`, value 3) —
    /// the half variants only downsample, the packing declaration is the same.
    /// Frame-sequential is a *temporal* interleave, not a spatial packing, so
    /// there is no meaningful `StereoMode` for it (returns `None`; left
    /// untagged).
    pub fn matroska_stereo_mode(self) -> Option<&'static str> {
        match self {
            OutputFormat::FullSbs | OutputFormat::HalfSbs => Some("left_right"),
            OutputFormat::FullTab | OutputFormat::HalfTab => Some("top_bottom"),
            OutputFormat::FrameSequential => None,
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
    fn matroska_stereo_mode_matches_packing() {
        // Both SBS variants declare left-first side-by-side; both TAB variants
        // declare left-on-top top-bottom; frame-sequential has no spatial tag.
        assert_eq!(OutputFormat::FullSbs.matroska_stereo_mode(), Some("left_right"));
        assert_eq!(OutputFormat::HalfSbs.matroska_stereo_mode(), Some("left_right"));
        assert_eq!(OutputFormat::FullTab.matroska_stereo_mode(), Some("top_bottom"));
        assert_eq!(OutputFormat::HalfTab.matroska_stereo_mode(), Some("top_bottom"));
        assert_eq!(OutputFormat::FrameSequential.matroska_stereo_mode(), None);
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
