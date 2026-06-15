// Slice header parser (§ 7.3.3), including the MVC dependent-view case.
//
// The slice header is where base-view and dependent-view decode diverge:
// it carries frame_num / POC (for DPB ordering), the ref-idx overrides
// and ref_pic_list_modification() that Annex G § 8.2.4 extends with
// inter-view entries, and the QP/deblocking parameters the macroblock
// layer needs. Parsing it requires the referenced SPS and PPS.
//
// pred_weight_table() and dec_ref_pic_marking() are consumed but not
// retained (no field in our header struct needs them yet); their syntax
// must still be walked so the reader is positioned correctly for the
// fields that follow.

use super::bitstream::{BitReader, ReadError};
use super::pps::Pps;
use super::ref_pic_list_modification::{
    parse_ref_pic_list_modification, RefPicListModifications, SliceKind,
};
use super::sps::Sps;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SliceHeader {
    pub first_mb_in_slice: u32,
    pub slice_type: u32,
    pub slice_kind: SliceKind,
    pub pic_parameter_set_id: u32,
    pub colour_plane_id: Option<u8>,
    pub frame_num: u32,
    pub field_pic_flag: bool,
    pub bottom_field_flag: bool,
    pub idr_pic_id: Option<u32>,
    pub pic_order_cnt_lsb: Option<u32>,
    pub delta_pic_order_cnt_bottom: i32,
    pub delta_pic_order_cnt: [i32; 2],
    pub redundant_pic_cnt: u32,
    pub direct_spatial_mv_pred_flag: bool,
    pub num_ref_idx_l0_active_minus1: u32,
    pub num_ref_idx_l1_active_minus1: u32,
    pub ref_pic_list_modifications: RefPicListModifications,
    pub cabac_init_idc: Option<u32>,
    pub slice_qp_delta: i32,
    pub disable_deblocking_filter_idc: u32,
    pub slice_alpha_c0_offset_div2: i32,
    pub slice_beta_offset_div2: i32,
}

/// Parse `slice_header()` (§ 7.3.3) from the current bit position.
///
/// `idr_pic_flag` is the access unit's IdrPicFlag: for a base-view slice
/// that's `nal_unit_type == 5`; for a dependent-view slice
/// (`slice_layer_extension`, type 20) it's `!mvc_extension.non_idr_flag`.
/// `nal_ref_idc` comes from the NAL header and gates dec_ref_pic_marking.
pub fn parse_slice_header(
    reader: &mut BitReader<'_>,
    idr_pic_flag: bool,
    nal_ref_idc: u8,
    sps: &Sps,
    pps: &Pps,
) -> Result<SliceHeader, ReadError> {
    let first_mb_in_slice = reader.read_ue()?;
    let slice_type = reader.read_ue()?;
    let slice_kind = SliceKind::from_slice_type(slice_type);
    let pic_parameter_set_id = reader.read_ue()?;

    let colour_plane_id = if sps.separate_colour_plane_flag {
        Some(reader.read_u(2)? as u8)
    } else {
        None
    };

    let frame_num = reader.read_u(sps.log2_max_frame_num_minus4 + 4)?;

    let mut field_pic_flag = false;
    let mut bottom_field_flag = false;
    if !sps.frame_mbs_only_flag {
        field_pic_flag = reader.read_bit()?;
        if field_pic_flag {
            bottom_field_flag = reader.read_bit()?;
        }
    }

    let idr_pic_id = if idr_pic_flag { Some(reader.read_ue()?) } else { None };

    let mut pic_order_cnt_lsb = None;
    let mut delta_pic_order_cnt_bottom = 0;
    let mut delta_pic_order_cnt = [0, 0];
    if sps.pic_order_cnt_type == 0 {
        pic_order_cnt_lsb = Some(reader.read_u(sps.log2_max_pic_order_cnt_lsb_minus4 + 4)?);
        if pps.bottom_field_pic_order_in_frame_present_flag && !field_pic_flag {
            delta_pic_order_cnt_bottom = reader.read_se()?;
        }
    } else if sps.pic_order_cnt_type == 1 && !sps.delta_pic_order_always_zero_flag {
        delta_pic_order_cnt[0] = reader.read_se()?;
        if pps.bottom_field_pic_order_in_frame_present_flag && !field_pic_flag {
            delta_pic_order_cnt[1] = reader.read_se()?;
        }
    }

    let redundant_pic_cnt = if pps.redundant_pic_cnt_present_flag {
        reader.read_ue()?
    } else {
        0
    };

    let direct_spatial_mv_pred_flag =
        if matches!(slice_kind, SliceKind::B) { reader.read_bit()? } else { false };

    let mut num_ref_idx_l0_active_minus1 = pps.num_ref_idx_l0_default_active_minus1;
    let mut num_ref_idx_l1_active_minus1 = pps.num_ref_idx_l1_default_active_minus1;
    if matches!(slice_kind, SliceKind::P | SliceKind::Sp | SliceKind::B) {
        let num_ref_idx_active_override_flag = reader.read_bit()?;
        if num_ref_idx_active_override_flag {
            num_ref_idx_l0_active_minus1 = reader.read_ue()?;
            if matches!(slice_kind, SliceKind::B) {
                num_ref_idx_l1_active_minus1 = reader.read_ue()?;
            }
        }
    }

    // ref_pic_list_modification() — the existing parser already handles
    // the MVC inter-view modification IDCs 4/5 that dependent-view slices
    // use, so it serves both base and MVC slices.
    let ref_pic_list_modifications = parse_ref_pic_list_modification(reader, slice_kind)?;

    // pred_weight_table() — consume only.
    let weighted = (pps.weighted_pred_flag
        && matches!(slice_kind, SliceKind::P | SliceKind::Sp))
        || (pps.weighted_bipred_idc == 1 && matches!(slice_kind, SliceKind::B));
    if weighted {
        consume_pred_weight_table(
            reader,
            sps.chroma_array_type(),
            num_ref_idx_l0_active_minus1,
            num_ref_idx_l1_active_minus1,
            slice_kind,
        )?;
    }

    // dec_ref_pic_marking() — consume only.
    if nal_ref_idc != 0 {
        consume_dec_ref_pic_marking(reader, idr_pic_flag)?;
    }

    let cabac_init_idc = if pps.entropy_coding_mode_flag
        && !matches!(slice_kind, SliceKind::I | SliceKind::Si)
    {
        Some(reader.read_ue()?)
    } else {
        None
    };

    let slice_qp_delta = reader.read_se()?;

    if matches!(slice_kind, SliceKind::Sp | SliceKind::Si) {
        if matches!(slice_kind, SliceKind::Sp) {
            let _sp_for_switch_flag = reader.read_bit()?;
        }
        let _slice_qs_delta = reader.read_se()?;
    }

    let mut disable_deblocking_filter_idc = 0;
    let mut slice_alpha_c0_offset_div2 = 0;
    let mut slice_beta_offset_div2 = 0;
    if pps.deblocking_filter_control_present_flag {
        disable_deblocking_filter_idc = reader.read_ue()?;
        if disable_deblocking_filter_idc != 1 {
            slice_alpha_c0_offset_div2 = reader.read_se()?;
            slice_beta_offset_div2 = reader.read_se()?;
        }
    }

    // slice_group_change_cycle follows here when FMO is in use
    // (num_slice_groups_minus1 > 0 with map type 3..5). MVC/Blu-ray
    // streams never use FMO, and nothing past the header is parsed yet,
    // so it is intentionally not consumed.

    Ok(SliceHeader {
        first_mb_in_slice,
        slice_type,
        slice_kind,
        pic_parameter_set_id,
        colour_plane_id,
        frame_num,
        field_pic_flag,
        bottom_field_flag,
        idr_pic_id,
        pic_order_cnt_lsb,
        delta_pic_order_cnt_bottom,
        delta_pic_order_cnt,
        redundant_pic_cnt,
        direct_spatial_mv_pred_flag,
        num_ref_idx_l0_active_minus1,
        num_ref_idx_l1_active_minus1,
        ref_pic_list_modifications,
        cabac_init_idc,
        slice_qp_delta,
        disable_deblocking_filter_idc,
        slice_alpha_c0_offset_div2,
        slice_beta_offset_div2,
    })
}

/// `pred_weight_table()` (§ 7.3.3.2). Walked for positioning only.
fn consume_pred_weight_table(
    reader: &mut BitReader<'_>,
    chroma_array_type: u32,
    num_ref_idx_l0_active_minus1: u32,
    num_ref_idx_l1_active_minus1: u32,
    kind: SliceKind,
) -> Result<(), ReadError> {
    let _luma_log2_weight_denom = reader.read_ue()?;
    if chroma_array_type != 0 {
        let _chroma_log2_weight_denom = reader.read_ue()?;
    }
    consume_weight_list(reader, chroma_array_type, num_ref_idx_l0_active_minus1)?;
    if matches!(kind, SliceKind::B) {
        consume_weight_list(reader, chroma_array_type, num_ref_idx_l1_active_minus1)?;
    }
    Ok(())
}

fn consume_weight_list(
    reader: &mut BitReader<'_>,
    chroma_array_type: u32,
    num_ref_idx_active_minus1: u32,
) -> Result<(), ReadError> {
    for _ in 0..=num_ref_idx_active_minus1 {
        if reader.read_bit()? {
            // luma_weight_l[i], luma_offset_l[i]
            let _ = reader.read_se()?;
            let _ = reader.read_se()?;
        }
        if chroma_array_type != 0 && reader.read_bit()? {
            for _ in 0..2 {
                let _chroma_weight = reader.read_se()?;
                let _chroma_offset = reader.read_se()?;
            }
        }
    }
    Ok(())
}

/// `dec_ref_pic_marking()` (§ 7.3.3.3). Walked for positioning only.
fn consume_dec_ref_pic_marking(
    reader: &mut BitReader<'_>,
    idr_pic_flag: bool,
) -> Result<(), ReadError> {
    if idr_pic_flag {
        let _no_output_of_prior_pics_flag = reader.read_bit()?;
        let _long_term_reference_flag = reader.read_bit()?;
    } else {
        let adaptive_ref_pic_marking_mode_flag = reader.read_bit()?;
        if adaptive_ref_pic_marking_mode_flag {
            loop {
                let op = reader.read_ue()?;
                match op {
                    0 => break,
                    1 => {
                        let _difference_of_pic_nums_minus1 = reader.read_ue()?;
                    }
                    2 => {
                        let _long_term_pic_num = reader.read_ue()?;
                    }
                    3 => {
                        let _difference_of_pic_nums_minus1 = reader.read_ue()?;
                        let _long_term_frame_idx = reader.read_ue()?;
                    }
                    4 => {
                        let _max_long_term_frame_idx_plus1 = reader.read_ue()?;
                    }
                    5 => {}
                    6 => {
                        let _long_term_frame_idx = reader.read_ue()?;
                    }
                    _ => break,
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mvc::sps::Sps;

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

    /// A minimal SPS context for slice-header tests: progressive 4:2:0,
    /// POC type 0, log2_max_frame_num = 4 bits, log2_max_poc_lsb = 4 bits.
    fn test_sps() -> Sps {
        Sps {
            profile_idc: 100,
            level_idc: 41,
            seq_parameter_set_id: 0,
            chroma_format_idc: 1,
            separate_colour_plane_flag: false,
            bit_depth_luma_minus8: 0,
            bit_depth_chroma_minus8: 0,
            log2_max_frame_num_minus4: 0,
            pic_order_cnt_type: 0,
            log2_max_pic_order_cnt_lsb_minus4: 0,
            delta_pic_order_always_zero_flag: false,
            max_num_ref_frames: 4,
            gaps_in_frame_num_value_allowed_flag: false,
            pic_width_in_mbs: 120,
            pic_height_in_map_units: 68,
            frame_mbs_only_flag: true,
            mb_adaptive_frame_field_flag: false,
            direct_8x8_inference_flag: true,
            frame_crop: (0, 0, 0, 4),
            width: 1920,
            height: 1080,
            vui_timing: None,
        }
    }

    fn test_pps() -> Pps {
        Pps {
            pic_parameter_set_id: 0,
            seq_parameter_set_id: 0,
            entropy_coding_mode_flag: true, // CABAC
            bottom_field_pic_order_in_frame_present_flag: false,
            num_slice_groups_minus1: 0,
            num_ref_idx_l0_default_active_minus1: 0,
            num_ref_idx_l1_default_active_minus1: 0,
            weighted_pred_flag: false,
            weighted_bipred_idc: 0,
            pic_init_qp_minus26: 0,
            pic_init_qs_minus26: 0,
            chroma_qp_index_offset: 0,
            deblocking_filter_control_present_flag: true,
            constrained_intra_pred_flag: false,
            redundant_pic_cnt_present_flag: false,
            transform_8x8_mode_flag: false,
            second_chroma_qp_index_offset: 0,
        }
    }

    #[test]
    fn parses_idr_i_slice_header() {
        // IDR I-slice (base view, nal_unit_type 5, nal_ref_idc != 0):
        //   first_mb_in_slice  = 0   ue "1"
        //   slice_type         = 7   ue "0001000" (I, all-I variant)
        //   pic_parameter_set_id = 0 ue "1"
        //   frame_num (4 bits) = 0   u(4) "0000"
        //   idr_pic_id         = 0   ue "1"
        //   pic_order_cnt_lsb (4 bits) = 0  u(4) "0000"
        //   (I slice: no ref-idx override, ref_pic_list_modification for
        //    I slice is absent)
        //   dec_ref_pic_marking (idr): no_output "0", long_term "0"
        //   (CABAC but I slice -> no cabac_init_idc)
        //   slice_qp_delta     = 0   se "1"
        //   deblocking control present: disable_deblocking_filter_idc = 0 ue "1"
        //      idc != 1 -> alpha "1" (se 0), beta "1" (se 0)
        //   rbsp_stop_one_bit  = 1
        let bits = [
            "1",       // first_mb_in_slice ue(0)
            "0001000", // slice_type ue(7) -> I
            "1",       // pic_parameter_set_id ue(0)
            "0000",    // frame_num u(4)=0
            "1",       // idr_pic_id ue(0)
            "0000",    // pic_order_cnt_lsb u(4)=0
            "0",       // no_output_of_prior_pics_flag
            "0",       // long_term_reference_flag
            "1",       // slice_qp_delta se(0)
            "1",       // disable_deblocking_filter_idc ue(0)
            "1",       // slice_alpha_c0_offset_div2 se(0)
            "1",       // slice_beta_offset_div2 se(0)
            "1",       // rbsp stop bit
        ];
        let bytes = bits_to_bytes(&bits);
        let mut r = BitReader::new(&bytes);
        let sh = parse_slice_header(&mut r, true, 3, &test_sps(), &test_pps()).expect("parse");

        assert_eq!(sh.first_mb_in_slice, 0);
        assert_eq!(sh.slice_type, 7);
        assert!(matches!(sh.slice_kind, SliceKind::I));
        assert_eq!(sh.frame_num, 0);
        assert_eq!(sh.idr_pic_id, Some(0));
        assert_eq!(sh.pic_order_cnt_lsb, Some(0));
        assert!(sh.cabac_init_idc.is_none()); // I slice
        assert_eq!(sh.slice_qp_delta, 0);
        assert_eq!(sh.disable_deblocking_filter_idc, 0);
    }

    #[test]
    fn parses_p_slice_with_cabac_init_and_ref_override() {
        // Non-IDR P-slice (nal_ref_idc != 0):
        //   first_mb_in_slice  = 0   ue "1"
        //   slice_type         = 0   ue "1" (P)
        //   pic_parameter_set_id = 0 ue "1"
        //   frame_num (4 bits) = 3   u(4) "0011"
        //   pic_order_cnt_lsb (4 bits) = 6  u(4) "0110"
        //   num_ref_idx_active_override_flag = 1 "1"
        //     num_ref_idx_l0_active_minus1 = 1 ue "010"
        //   ref_pic_list_modification (P): flag=0 "0"
        //   dec_ref_pic_marking (non-idr): adaptive flag = 0 "0"
        //   cabac_init_idc = 0 ue "1"  (CABAC, P slice)
        //   slice_qp_delta = -1 se "011" (codeNum 2 -> -1)
        //   disable_deblocking_filter_idc = 1 ue "010" -> no offsets
        //   rbsp stop bit "1"
        let bits = [
            "1",    // first_mb_in_slice ue(0)
            "1",    // slice_type ue(0) -> P
            "1",    // pps id ue(0)
            "0011", // frame_num u(4)=3
            "0110", // pic_order_cnt_lsb u(4)=6
            "1",    // num_ref_idx_active_override_flag
            "010",  // num_ref_idx_l0_active_minus1 ue(1)
            "0",    // ref_pic_list_modification_flag_l0 = 0
            "0",    // adaptive_ref_pic_marking_mode_flag = 0
            "1",    // cabac_init_idc ue(0)
            "011",  // slice_qp_delta se(-1)
            "010",  // disable_deblocking_filter_idc ue(1)
            "1",    // rbsp stop bit
        ];
        let bytes = bits_to_bytes(&bits);
        let mut r = BitReader::new(&bytes);
        let sh = parse_slice_header(&mut r, false, 2, &test_sps(), &test_pps()).expect("parse");

        assert!(matches!(sh.slice_kind, SliceKind::P));
        assert_eq!(sh.frame_num, 3);
        assert_eq!(sh.pic_order_cnt_lsb, Some(6));
        assert_eq!(sh.num_ref_idx_l0_active_minus1, 1);
        assert_eq!(sh.cabac_init_idc, Some(0));
        assert_eq!(sh.slice_qp_delta, -1);
        assert_eq!(sh.disable_deblocking_filter_idc, 1);
        assert!(sh.idr_pic_id.is_none());
    }
}
