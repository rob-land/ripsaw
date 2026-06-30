// Sequence Parameter Set parsing — base SPS (§ 7.3.2.1.1) and the
// Subset SPS MVC extension (Annex G § 7.3.2.1.4 / § 7.4.2.1.4).
//
// The Subset SPS NAL (type 15) RBSP contains:
//
//   seq_parameter_set_data()                  // base SPS
//   if profile_idc in {118, 128, 134}:        // Multiview High profile family
//       bit_equal_to_one                f(1)  // must be 1
//       seq_parameter_set_mvc_extension()
//       mvc_vui_parameters_present_flag u(1)
//       ...
//
// `parse_seq_parameter_set_data` decodes the base SPS in full (scaling
// lists, POC, VUI incl. HRD) and derives the cropped luma dimensions, so
// libmvc no longer depends on an external decoder just to learn a
// stream's geometry. `parse_subset_sps_rbsp` chains it with
// `parse_sps_mvc_extension` for the type-15 subset SPS.

use super::bitstream::{BitReader, ReadError};
use super::rbsp::extract_rbsp;

/// A decoded base H.264 Sequence Parameter Set. Only the fields a
/// downstream slice/macroblock decoder actually consults are surfaced;
/// scaling lists, VUI and HRD are parsed for bit-exact positioning but
/// their values (beyond timing) are discarded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sps {
    pub profile_idc: u8,
    pub level_idc: u8,
    pub seq_parameter_set_id: u32,
    pub chroma_format_idc: u32,
    pub separate_colour_plane_flag: bool,
    pub bit_depth_luma_minus8: u32,
    pub bit_depth_chroma_minus8: u32,
    pub log2_max_frame_num_minus4: u32,
    pub pic_order_cnt_type: u32,
    pub log2_max_pic_order_cnt_lsb_minus4: u32,
    pub delta_pic_order_always_zero_flag: bool,
    pub max_num_ref_frames: u32,
    pub gaps_in_frame_num_value_allowed_flag: bool,
    pub pic_width_in_mbs: u32,
    pub pic_height_in_map_units: u32,
    pub frame_mbs_only_flag: bool,
    pub mb_adaptive_frame_field_flag: bool,
    pub direct_8x8_inference_flag: bool,
    /// (left, right, top, bottom) crop offsets in CropUnit steps.
    pub frame_crop: (u32, u32, u32, u32),
    /// Cropped luma width in samples (what a player displays).
    pub width: u32,
    /// Cropped luma height in samples.
    pub height: u32,
    pub vui_timing: Option<VuiTiming>,
    /// Resolved inverse-quant weight matrices when the SPS carries a scaling
    /// matrix (`seq_scaling_matrix_present_flag`); `None` means flat (16).
    pub scaling: Option<crate::mvc::scaling::ScalingLists>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VuiTiming {
    pub num_units_in_tick: u32,
    pub time_scale: u32,
    pub fixed_frame_rate_flag: bool,
}

impl Sps {
    /// `ChromaArrayType` (§ 7.4.2.1.1): 0 when colour planes are coded
    /// separately, else `chroma_format_idc`.
    pub fn chroma_array_type(&self) -> u32 {
        if self.separate_colour_plane_flag { 0 } else { self.chroma_format_idc }
    }

    /// MaxFrameNum = 2^(log2_max_frame_num_minus4 + 4).
    pub fn max_frame_num(&self) -> u32 {
        1u32 << (self.log2_max_frame_num_minus4 + 4)
    }
}

const HIGH_PROFILES: &[u8] = &[100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135];

/// Parse `seq_parameter_set_data()` (§ 7.3.2.1.1) from the current bit
/// position. `reader` must be positioned at `profile_idc` (i.e. just past
/// the NAL header), reading from the RBSP (emulation-prevention bytes
/// already removed).
pub fn parse_seq_parameter_set_data(reader: &mut BitReader<'_>) -> Result<Sps, ReadError> {
    let profile_idc = reader.read_u(8)? as u8;
    let _constraint_flags_and_reserved = reader.read_u(8)?;
    let level_idc = reader.read_u(8)? as u8;
    let seq_parameter_set_id = reader.read_ue()?;

    let mut chroma_format_idc = 1;
    let mut separate_colour_plane_flag = false;
    let mut bit_depth_luma_minus8 = 0;
    let mut bit_depth_chroma_minus8 = 0;
    let mut scaling = None;
    if HIGH_PROFILES.contains(&profile_idc) {
        chroma_format_idc = reader.read_ue()?;
        if chroma_format_idc == 3 {
            separate_colour_plane_flag = reader.read_bit()?;
        }
        bit_depth_luma_minus8 = reader.read_ue()?;
        bit_depth_chroma_minus8 = reader.read_ue()?;
        let _qpprime_y_zero_transform_bypass_flag = reader.read_bit()?;
        let seq_scaling_matrix_present_flag = reader.read_bit()?;
        if seq_scaling_matrix_present_flag {
            let count = if chroma_format_idc != 3 { 8 } else { 12 };
            scaling = Some(crate::mvc::scaling::parse_scaling_matrix(reader, count)?);
        }
    }

    let log2_max_frame_num_minus4 = reader.read_ue()?;
    let pic_order_cnt_type = reader.read_ue()?;
    let mut log2_max_pic_order_cnt_lsb_minus4 = 0;
    let mut delta_pic_order_always_zero_flag = false;
    match pic_order_cnt_type {
        0 => {
            log2_max_pic_order_cnt_lsb_minus4 = reader.read_ue()?;
        }
        1 => {
            delta_pic_order_always_zero_flag = reader.read_bit()?;
            let _offset_for_non_ref_pic = reader.read_se()?;
            let _offset_for_top_to_bottom_field = reader.read_se()?;
            let num_ref_frames_in_pic_order_cnt_cycle = reader.read_ue()?;
            for _ in 0..num_ref_frames_in_pic_order_cnt_cycle {
                let _offset_for_ref_frame = reader.read_se()?;
            }
        }
        _ => {}
    }

    let max_num_ref_frames = reader.read_ue()?;
    let gaps_in_frame_num_value_allowed_flag = reader.read_bit()?;
    let pic_width_in_mbs = reader.read_ue()? + 1;
    let pic_height_in_map_units = reader.read_ue()? + 1;
    let frame_mbs_only_flag = reader.read_bit()?;
    let mut mb_adaptive_frame_field_flag = false;
    if !frame_mbs_only_flag {
        mb_adaptive_frame_field_flag = reader.read_bit()?;
    }
    let direct_8x8_inference_flag = reader.read_bit()?;

    let frame_cropping_flag = reader.read_bit()?;
    let frame_crop = if frame_cropping_flag {
        (
            reader.read_ue()?,
            reader.read_ue()?,
            reader.read_ue()?,
            reader.read_ue()?,
        )
    } else {
        (0, 0, 0, 0)
    };

    let vui_parameters_present_flag = reader.read_bit()?;
    let vui_timing = if vui_parameters_present_flag {
        parse_vui(reader)?
    } else {
        None
    };

    let mut sps = Sps {
        profile_idc,
        level_idc,
        seq_parameter_set_id,
        chroma_format_idc,
        separate_colour_plane_flag,
        bit_depth_luma_minus8,
        bit_depth_chroma_minus8,
        log2_max_frame_num_minus4,
        pic_order_cnt_type,
        log2_max_pic_order_cnt_lsb_minus4,
        delta_pic_order_always_zero_flag,
        max_num_ref_frames,
        gaps_in_frame_num_value_allowed_flag,
        pic_width_in_mbs,
        pic_height_in_map_units,
        frame_mbs_only_flag,
        mb_adaptive_frame_field_flag,
        direct_8x8_inference_flag,
        frame_crop,
        width: 0,
        height: 0,
        vui_timing,
        scaling,
    };
    let (w, h) = derive_dimensions(&sps);
    sps.width = w;
    sps.height = h;
    Ok(sps)
}

/// Compute cropped luma dimensions per § 7.4.2.1.1 (eq. 7-18..7-21).
fn derive_dimensions(sps: &Sps) -> (u32, u32) {
    let width_mbs = sps.pic_width_in_mbs * 16;
    let height_map = sps.pic_height_in_map_units * 16;
    let frame_height = (2 - sps.frame_mbs_only_flag as u32) * height_map;

    let (sub_w, sub_h) = match sps.chroma_format_idc {
        1 => (2, 2), // 4:2:0
        2 => (2, 1), // 4:2:2
        3 => (1, 1), // 4:4:4
        _ => (1, 1), // monochrome — SubWidthC/SubHeightC undefined; crop units = 1
    };
    let chroma_array_type = sps.chroma_array_type();
    let (crop_unit_x, crop_unit_y) = if chroma_array_type == 0 {
        (1, 2 - sps.frame_mbs_only_flag as u32)
    } else {
        (sub_w, sub_h * (2 - sps.frame_mbs_only_flag as u32))
    };

    let (l, r, t, b) = sps.frame_crop;
    let width = width_mbs.saturating_sub(crop_unit_x * (l + r));
    let height = frame_height.saturating_sub(crop_unit_y * (t + b));
    (width, height)
}


/// `vui_parameters()` (Annex E § E.1.1). Returns timing info when present;
/// everything else is consumed to keep the reader aligned.
fn parse_vui(reader: &mut BitReader<'_>) -> Result<Option<VuiTiming>, ReadError> {
    if reader.read_bit()? {
        // aspect_ratio_info_present_flag
        let aspect_ratio_idc = reader.read_u(8)?;
        if aspect_ratio_idc == 255 {
            let _sar_width = reader.read_u(16)?;
            let _sar_height = reader.read_u(16)?;
        }
    }
    if reader.read_bit()? {
        // overscan_info_present_flag
        let _overscan_appropriate_flag = reader.read_bit()?;
    }
    if reader.read_bit()? {
        // video_signal_type_present_flag
        let _video_format = reader.read_u(3)?;
        let _video_full_range_flag = reader.read_bit()?;
        if reader.read_bit()? {
            // colour_description_present_flag
            let _colour_primaries = reader.read_u(8)?;
            let _transfer_characteristics = reader.read_u(8)?;
            let _matrix_coefficients = reader.read_u(8)?;
        }
    }
    if reader.read_bit()? {
        // chroma_loc_info_present_flag
        let _top = reader.read_ue()?;
        let _bottom = reader.read_ue()?;
    }

    let mut timing = None;
    if reader.read_bit()? {
        // timing_info_present_flag
        let num_units_in_tick = reader.read_u(32)?;
        let time_scale = reader.read_u(32)?;
        let fixed_frame_rate_flag = reader.read_bit()?;
        timing = Some(VuiTiming { num_units_in_tick, time_scale, fixed_frame_rate_flag });
    }

    let nal_hrd = reader.read_bit()?;
    if nal_hrd {
        parse_hrd(reader)?;
    }
    let vcl_hrd = reader.read_bit()?;
    if vcl_hrd {
        parse_hrd(reader)?;
    }
    if nal_hrd || vcl_hrd {
        let _low_delay_hrd_flag = reader.read_bit()?;
    }
    let _pic_struct_present_flag = reader.read_bit()?;
    if reader.read_bit()? {
        // bitstream_restriction_flag
        let _motion_vectors_over_pic_boundaries_flag = reader.read_bit()?;
        let _max_bytes_per_pic_denom = reader.read_ue()?;
        let _max_bits_per_mb_denom = reader.read_ue()?;
        let _log2_max_mv_length_horizontal = reader.read_ue()?;
        let _log2_max_mv_length_vertical = reader.read_ue()?;
        let _max_num_reorder_frames = reader.read_ue()?;
        let _max_dec_frame_buffering = reader.read_ue()?;
    }
    Ok(timing)
}

/// `hrd_parameters()` (Annex E § E.1.2). Consumed, not retained.
fn parse_hrd(reader: &mut BitReader<'_>) -> Result<(), ReadError> {
    let cpb_cnt_minus1 = reader.read_ue()?;
    let _bit_rate_scale = reader.read_u(4)?;
    let _cpb_size_scale = reader.read_u(4)?;
    for _ in 0..=cpb_cnt_minus1 {
        let _bit_rate_value_minus1 = reader.read_ue()?;
        let _cpb_size_value_minus1 = reader.read_ue()?;
        let _cbr_flag = reader.read_bit()?;
    }
    let _initial_cpb_removal_delay_length_minus1 = reader.read_u(5)?;
    let _cpb_removal_delay_length_minus1 = reader.read_u(5)?;
    let _dpb_output_delay_length_minus1 = reader.read_u(5)?;
    let _time_offset_length = reader.read_u(5)?;
    Ok(())
}

/// Parse a base SPS NAL's RBSP (type 7). `rbsp` is the bytes *after* the
/// 1-byte NAL header, with emulation-prevention bytes already removed.
pub fn parse_sps_rbsp(rbsp: &[u8]) -> Result<Sps, ReadError> {
    let mut reader = BitReader::new(rbsp);
    parse_seq_parameter_set_data(&mut reader)
}

/// A Subset SPS (NAL type 15): the base SPS plus the MVC extension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubsetSps {
    pub sps: Sps,
    pub mvc: SpsMvcExtension,
}

/// Parse a Subset SPS NAL's RBSP (type 15): `seq_parameter_set_data()`
/// then `bit_equal_to_one` then `seq_parameter_set_mvc_extension()`.
/// `rbsp` is the bytes after the 1-byte NAL header, emulation-prevention
/// already removed.
pub fn parse_subset_sps_rbsp(rbsp: &[u8]) -> Result<SubsetSps, ReadError> {
    let mut reader = BitReader::new(rbsp);
    let sps = parse_seq_parameter_set_data(&mut reader)?;
    let mvc = if HIGH_PROFILES.contains(&sps.profile_idc)
        && matches!(sps.profile_idc, 118 | 128 | 134)
    {
        let _bit_equal_to_one = reader.read_bit()?;
        parse_sps_mvc_extension(&mut reader)?
    } else {
        // Not a Multiview-family subset SPS; no extension to read.
        SpsMvcExtension {
            num_views_minus1: 0,
            view_id: vec![0],
            anchor_refs: vec![AnchorRefs::default()],
            non_anchor_refs: vec![NonAnchorRefs::default()],
            level_values: Vec::new(),
        }
    };
    Ok(SubsetSps { sps, mvc })
}

/// Convenience: strip the 1-byte NAL header and emulation-prevention
/// bytes from a raw type-7 SPS NAL, then parse. `nal` starts at the NAL
/// header byte (its low 5 bits should be 7).
pub fn parse_sps_nal(nal: &[u8]) -> Result<Sps, ReadError> {
    if nal.is_empty() {
        return Err(ReadError::Truncated);
    }
    let rbsp = extract_rbsp(&nal[1..]);
    parse_sps_rbsp(&rbsp)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpsMvcExtension {
    pub num_views_minus1: u32,
    pub view_id: Vec<u32>,
    pub anchor_refs: Vec<AnchorRefs>,
    pub non_anchor_refs: Vec<NonAnchorRefs>,
    pub level_values: Vec<LevelValue>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AnchorRefs {
    pub l0: Vec<u32>,
    pub l1: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NonAnchorRefs {
    pub l0: Vec<u32>,
    pub l1: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LevelValue {
    pub level_idc: u8,
    pub operating_points: Vec<OperatingPoint>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperatingPoint {
    pub temporal_id: u8,
    pub target_view_ids: Vec<u32>,
    pub num_views_minus1: u32,
}

const MAX_NUM_VIEWS: u32 = 1024;

/// Parse `seq_parameter_set_mvc_extension()` starting at the current bit
/// position of `reader`. Caller must have already consumed any preceding
/// fields (the base SPS + `bit_equal_to_one`).
pub fn parse_sps_mvc_extension(reader: &mut BitReader<'_>) -> Result<SpsMvcExtension, ReadError> {
    let num_views_minus1 = reader.read_ue()?;
    if num_views_minus1 >= MAX_NUM_VIEWS {
        return Err(ReadError::ExpGolombOverflow);
    }
    let view_count = num_views_minus1 as usize + 1;

    let mut view_id = Vec::with_capacity(view_count);
    for _ in 0..view_count {
        view_id.push(reader.read_ue()?);
    }

    // anchor refs are absent for view 0
    let mut anchor_refs = vec![AnchorRefs::default()];
    for _ in 1..view_count {
        let num_l0 = reader.read_ue()? as usize;
        let mut l0 = Vec::with_capacity(num_l0);
        for _ in 0..num_l0 {
            l0.push(reader.read_ue()?);
        }
        let num_l1 = reader.read_ue()? as usize;
        let mut l1 = Vec::with_capacity(num_l1);
        for _ in 0..num_l1 {
            l1.push(reader.read_ue()?);
        }
        anchor_refs.push(AnchorRefs { l0, l1 });
    }

    let mut non_anchor_refs = vec![NonAnchorRefs::default()];
    for _ in 1..view_count {
        let num_l0 = reader.read_ue()? as usize;
        let mut l0 = Vec::with_capacity(num_l0);
        for _ in 0..num_l0 {
            l0.push(reader.read_ue()?);
        }
        let num_l1 = reader.read_ue()? as usize;
        let mut l1 = Vec::with_capacity(num_l1);
        for _ in 0..num_l1 {
            l1.push(reader.read_ue()?);
        }
        non_anchor_refs.push(NonAnchorRefs { l0, l1 });
    }

    let num_level_values_signalled_minus1 = reader.read_ue()?;
    let mut level_values = Vec::with_capacity(num_level_values_signalled_minus1 as usize + 1);
    for _ in 0..=num_level_values_signalled_minus1 {
        let level_idc = reader.read_u(8)? as u8;
        let num_applicable_ops_minus1 = reader.read_ue()?;
        let mut operating_points =
            Vec::with_capacity(num_applicable_ops_minus1 as usize + 1);
        for _ in 0..=num_applicable_ops_minus1 {
            let temporal_id = reader.read_u(3)? as u8;
            let num_target_views_minus1 = reader.read_ue()?;
            let mut target_view_ids =
                Vec::with_capacity(num_target_views_minus1 as usize + 1);
            for _ in 0..=num_target_views_minus1 {
                target_view_ids.push(reader.read_ue()?);
            }
            let num_views_minus1 = reader.read_ue()?;
            operating_points.push(OperatingPoint {
                temporal_id,
                target_view_ids,
                num_views_minus1,
            });
        }
        level_values.push(LevelValue { level_idc, operating_points });
    }

    Ok(SpsMvcExtension {
        num_views_minus1,
        view_id,
        anchor_refs,
        non_anchor_refs,
        level_values,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pack a list of bit-string fragments into bytes, zero-padding the
    /// final byte. Shared by the SPS construction helpers below.
    fn bits_to_bytes(bits: &[&str]) -> Vec<u8> {
        let joined: String = bits.concat();
        let pad = (8 - (joined.len() % 8)) % 8;
        let mut padded = joined;
        padded.extend(std::iter::repeat('0').take(pad));
        padded
            .as_bytes()
            .chunks(8)
            .map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap(), 2).unwrap())
            .collect()
    }

    #[test]
    fn parses_base_sps_1080p_baseline() {
        // A baseline-profile (66) SPS for 1920x1080: 120x68 macroblocks
        // (1920x1088 coded) cropped by 4 CropUnitY (8 luma rows) to 1080.
        let bits = [
            "01000010",      // profile_idc = 66
            "00000000",      // constraint flags + reserved
            "00101000",      // level_idc = 40
            "1",             // seq_parameter_set_id ue(0)
            "1",             // log2_max_frame_num_minus4 ue(0)
            "1",             // pic_order_cnt_type ue(0)
            "1",             // log2_max_pic_order_cnt_lsb_minus4 ue(0)
            "010",           // max_num_ref_frames ue(1)
            "0",             // gaps_in_frame_num_value_allowed_flag
            "0000001111000", // pic_width_in_mbs_minus1 ue(119) -> 120 mbs
            "0000001000100", // pic_height_in_map_units_minus1 ue(67) -> 68
            "1",             // frame_mbs_only_flag
            "0",             // direct_8x8_inference_flag
            "1",             // frame_cropping_flag
            "1",             // frame_crop_left_offset ue(0)
            "1",             // frame_crop_right_offset ue(0)
            "1",             // frame_crop_top_offset ue(0)
            "00101",         // frame_crop_bottom_offset ue(4)
            "0",             // vui_parameters_present_flag
            "1",             // rbsp_stop_one_bit
        ];
        let bytes = bits_to_bytes(&bits);
        let sps = parse_sps_rbsp(&bytes).expect("parse base SPS");

        assert_eq!(sps.profile_idc, 66);
        assert_eq!(sps.level_idc, 40);
        assert_eq!(sps.chroma_format_idc, 1); // default (no high-profile block)
        assert_eq!(sps.max_num_ref_frames, 1);
        assert!(sps.frame_mbs_only_flag);
        assert_eq!(sps.pic_width_in_mbs, 120);
        assert_eq!(sps.pic_height_in_map_units, 68);
        assert_eq!(sps.frame_crop, (0, 0, 0, 4));
        assert_eq!((sps.width, sps.height), (1920, 1080));
        assert_eq!(sps.max_frame_num(), 16);
        assert!(sps.vui_timing.is_none());
    }

    /// Build a synthetic MVC SPS extension byte sequence for the canonical
    /// 3D-Blu-ray case: 2 views (base view + dependent view), the
    /// dependent view has the base view as an inter-view reference for
    /// both anchor and non-anchor pictures, one level value (4.1) with a
    /// single operating point covering both views at temporal_id 0.
    fn build_2view_extension() -> Vec<u8> {
        // We need to write Exp-Golomb codewords AND a u(8) for level_idc
        // and a u(3) for temporal_id. Construct bit-by-bit.
        //
        // Fields in order:
        //   num_views_minus1                = 1     -> ue = "010"
        //   view_id[0]                      = 0     -> ue = "1"
        //   view_id[1]                      = 1     -> ue = "010"
        //   num_anchor_refs_l0[1]           = 1     -> ue = "010"
        //   anchor_ref_l0[1][0]             = 0     -> ue = "1"
        //   num_anchor_refs_l1[1]           = 0     -> ue = "1"
        //   num_non_anchor_refs_l0[1]       = 1     -> ue = "010"
        //   non_anchor_ref_l0[1][0]         = 0     -> ue = "1"
        //   num_non_anchor_refs_l1[1]       = 0     -> ue = "1"
        //   num_level_values_signalled_m1   = 0     -> ue = "1"
        //   level_idc[0]                    = 41    -> u(8) = "00101001"
        //   num_applicable_ops_minus1[0]    = 0     -> ue = "1"
        //   applicable_op_temporal_id[0][0] = 0     -> u(3) = "000"
        //   applicable_op_num_target_views_minus1[0][0] = 1 -> ue = "010"
        //   applicable_op_target_view_id[0][0][0] = 0 -> ue = "1"
        //   applicable_op_target_view_id[0][0][1] = 1 -> ue = "010"
        //   applicable_op_num_views_minus1[0][0] = 1 -> ue = "010"
        //
        // Bit string concatenation:
        //   010 1 010 010 1 1 010 1 1 1 00101001 1 000 010 1 010 010
        //
        // Let me lay it out without spaces and then chunk into bytes:
        let bits = [
            "010", "1", "010", "010", "1", "1", "010", "1", "1", "1", "00101001",
            "1", "000", "010", "1", "010", "010",
        ];
        let joined: String = bits.concat();
        // Pad to byte boundary with zeros.
        let pad = (8 - (joined.len() % 8)) % 8;
        let mut padded = joined;
        padded.extend(std::iter::repeat('0').take(pad));

        padded
            .as_bytes()
            .chunks(8)
            .map(|c| {
                let s = std::str::from_utf8(c).unwrap();
                u8::from_str_radix(s, 2).unwrap()
            })
            .collect()
    }

    #[test]
    fn parses_two_view_blu_ray_pattern() {
        let bytes = build_2view_extension();
        let mut r = BitReader::new(&bytes);
        let sps = parse_sps_mvc_extension(&mut r).expect("parse");

        assert_eq!(sps.num_views_minus1, 1);
        assert_eq!(sps.view_id, vec![0, 1]);
        // anchor_refs[0] is empty by construction; anchor_refs[1] points at view 0.
        assert!(sps.anchor_refs[0].l0.is_empty());
        assert_eq!(sps.anchor_refs[1].l0, vec![0]);
        assert!(sps.anchor_refs[1].l1.is_empty());
        // non_anchor_refs[0] is empty; non_anchor_refs[1].l0 = [0].
        assert!(sps.non_anchor_refs[0].l0.is_empty());
        assert_eq!(sps.non_anchor_refs[1].l0, vec![0]);
        assert!(sps.non_anchor_refs[1].l1.is_empty());
        // One level value at Level 4.1 with one operating point covering both views.
        assert_eq!(sps.level_values.len(), 1);
        assert_eq!(sps.level_values[0].level_idc, 41);
        let op = &sps.level_values[0].operating_points[0];
        assert_eq!(op.temporal_id, 0);
        assert_eq!(op.target_view_ids, vec![0, 1]);
        assert_eq!(op.num_views_minus1, 1);
    }

    #[test]
    fn rejects_extension_claiming_too_many_views() {
        // num_views_minus1 encoded as a very long Exp-Golomb codeword
        // that decodes to a value exceeding MAX_NUM_VIEWS triggers the
        // overflow guard before we try to allocate a giant Vec.
        // ue(1023) is the bit string "00000000000_1_111111111" (11 zeros,
        // 1, then 10 more bits = 1023).
        // We want ue(MAX_NUM_VIEWS) = ue(1024), but that needs 22 bits
        // and decodes to >= MAX_NUM_VIEWS, so we hand-construct ue(1024):
        // codeNum 1024 -> k=10 leading zeros, value 1024 = 0b10000000001
        // Wait, 2^10 - 1 = 1023; codeNum 1024 needs k=11? Let's verify:
        //   k=10: range covered = [1023, 2046]
        //   so codeNum=1024 -> "00000000000 1 0000000001" (10 zeros, 1, 11 bits including the 1?)
        // The formula: leading zeros = k, then the next bit is 1, then
        // k more bits. Total = 2k+1 bits.
        // codeNum = (1<<k) - 1 + read_k_bits
        // For codeNum=1024: pick k=10 -> base 1023 -> remainder 1 -> bits "0000000001".
        // So encoded as 10 zeros + 1 + "0000000001" = "00000000001 0000000001"
        // Bit string (21 bits):
        //   "0000000000" + "1" + "0000000001" = "000000000010000000001"
        // Pad to 24 bits with 3 zeros, becomes 3 bytes:
        //   "00000000 00100000 00001000"  (note alignment)
        //   = 0x00 0x20 0x08
        // Hmm let me recount:
        //   "0000000000 1 0000000001"
        //    1234567890   1234567890 1
        //   total = 10 + 1 + 10 = 21 bits.
        //   pad with 3 zeros: 21 + 3 = 24 bits.
        //   Bits:  000000000010000000001000
        //   bytes: 00000000 10000000 00100000? Let me re-chunk carefully:
        //     bit 0..7  = 00000000 = 0x00
        //     bit 8..15 = 10000000 = 0x80
        //     bit 16..23 = 00100000 = 0x20
        //   But wait, bits 0..7 of "000000000010000000001000" are
        //   "00000000" -- yes 0x00
        //   bits 8..15 are "10000000" = 0x80
        //   bits 16..23 are "01000".  hmm only 5 left. Let me recount.
        //
        // OK the encoded ue(1024) has 2*10+1 = 21 bits. Padding to 24:
        //   "000000000010000000001" + "000"
        // Chunk into 8-bit groups:
        //   "00000000" "00100000" "00001000"
        // = 0x00, 0x20, 0x08
        let bytes = [0x00, 0x20, 0x08];
        let mut r = BitReader::new(&bytes);
        // ue should decode to 1024, then the parser rejects via MAX_NUM_VIEWS.
        let err = parse_sps_mvc_extension(&mut r).expect_err("must reject");
        assert_eq!(err, ReadError::ExpGolombOverflow);
    }
}
