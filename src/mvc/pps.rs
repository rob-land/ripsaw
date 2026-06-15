// Picture Parameter Set parser (§ 7.3.2.2).
//
// The slice header needs several PPS fields (entropy mode, weighted-pred
// flags, default ref-idx counts, deblocking-control presence), so the
// decoder front-end has to parse the PPS as well as the SPS. The optional
// trailing extension (transform_8x8 / scaling matrix / second chroma QP
// offset) is gated on `more_rbsp_data()`; the scaling-matrix list count
// depends on the referenced SPS's `chroma_format_idc`, which the caller
// supplies.

use super::bitstream::{BitReader, ReadError};
use super::sps::skip_scaling_list;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pps {
    pub pic_parameter_set_id: u32,
    pub seq_parameter_set_id: u32,
    pub entropy_coding_mode_flag: bool,
    pub bottom_field_pic_order_in_frame_present_flag: bool,
    pub num_slice_groups_minus1: u32,
    pub num_ref_idx_l0_default_active_minus1: u32,
    pub num_ref_idx_l1_default_active_minus1: u32,
    pub weighted_pred_flag: bool,
    pub weighted_bipred_idc: u8,
    pub pic_init_qp_minus26: i32,
    pub pic_init_qs_minus26: i32,
    pub chroma_qp_index_offset: i32,
    pub deblocking_filter_control_present_flag: bool,
    pub constrained_intra_pred_flag: bool,
    pub redundant_pic_cnt_present_flag: bool,
    pub transform_8x8_mode_flag: bool,
    pub second_chroma_qp_index_offset: i32,
}

/// Parse `pic_parameter_set_rbsp()` (§ 7.3.2.2) from the current bit
/// position. `chroma_format_idc` comes from the referenced SPS and is
/// only consulted for the optional scaling-matrix list count; pass 1
/// (4:2:0) when the SPS isn't known and no scaling matrix is expected.
pub fn parse_pic_parameter_set(
    reader: &mut BitReader<'_>,
    chroma_format_idc: u32,
) -> Result<Pps, ReadError> {
    let pic_parameter_set_id = reader.read_ue()?;
    let seq_parameter_set_id = reader.read_ue()?;
    let entropy_coding_mode_flag = reader.read_bit()?;
    let bottom_field_pic_order_in_frame_present_flag = reader.read_bit()?;
    let num_slice_groups_minus1 = reader.read_ue()?;
    if num_slice_groups_minus1 > 0 {
        consume_slice_group_map(reader, num_slice_groups_minus1)?;
    }
    let num_ref_idx_l0_default_active_minus1 = reader.read_ue()?;
    let num_ref_idx_l1_default_active_minus1 = reader.read_ue()?;
    let weighted_pred_flag = reader.read_bit()?;
    let weighted_bipred_idc = reader.read_u(2)? as u8;
    let pic_init_qp_minus26 = reader.read_se()?;
    let pic_init_qs_minus26 = reader.read_se()?;
    let chroma_qp_index_offset = reader.read_se()?;
    let deblocking_filter_control_present_flag = reader.read_bit()?;
    let constrained_intra_pred_flag = reader.read_bit()?;
    let redundant_pic_cnt_present_flag = reader.read_bit()?;

    let mut transform_8x8_mode_flag = false;
    // Per § 7.4.2.2, when the extension is absent second_chroma_qp_index_offset
    // defaults to chroma_qp_index_offset.
    let mut second_chroma_qp_index_offset = chroma_qp_index_offset;
    if reader.more_rbsp_data() {
        transform_8x8_mode_flag = reader.read_bit()?;
        let pic_scaling_matrix_present_flag = reader.read_bit()?;
        if pic_scaling_matrix_present_flag {
            let extra = if chroma_format_idc != 3 { 2 } else { 6 };
            let count = 6 + extra * (transform_8x8_mode_flag as usize);
            for i in 0..count {
                if reader.read_bit()? {
                    let size = if i < 6 { 16 } else { 64 };
                    skip_scaling_list(reader, size)?;
                }
            }
        }
        second_chroma_qp_index_offset = reader.read_se()?;
    }

    Ok(Pps {
        pic_parameter_set_id,
        seq_parameter_set_id,
        entropy_coding_mode_flag,
        bottom_field_pic_order_in_frame_present_flag,
        num_slice_groups_minus1,
        num_ref_idx_l0_default_active_minus1,
        num_ref_idx_l1_default_active_minus1,
        weighted_pred_flag,
        weighted_bipred_idc,
        pic_init_qp_minus26,
        pic_init_qs_minus26,
        chroma_qp_index_offset,
        deblocking_filter_control_present_flag,
        constrained_intra_pred_flag,
        redundant_pic_cnt_present_flag,
        transform_8x8_mode_flag,
        second_chroma_qp_index_offset,
    })
}

/// Parse a PPS NAL's RBSP (type 8). `rbsp` is the bytes after the 1-byte
/// NAL header, emulation-prevention already removed.
pub fn parse_pps_rbsp(rbsp: &[u8], chroma_format_idc: u32) -> Result<Pps, ReadError> {
    let mut reader = BitReader::new(rbsp);
    parse_pic_parameter_set(&mut reader, chroma_format_idc)
}

/// Consume the slice-group map syntax (FMO, § 7.3.2.2). MVC/Blu-ray
/// content never uses more than one slice group, but parse it anyway so
/// the reader stays aligned on the rare stream that does.
fn consume_slice_group_map(
    reader: &mut BitReader<'_>,
    num_slice_groups_minus1: u32,
) -> Result<(), ReadError> {
    let slice_group_map_type = reader.read_ue()?;
    match slice_group_map_type {
        0 => {
            for _ in 0..=num_slice_groups_minus1 {
                let _run_length_minus1 = reader.read_ue()?;
            }
        }
        2 => {
            for _ in 0..num_slice_groups_minus1 {
                let _top_left = reader.read_ue()?;
                let _bottom_right = reader.read_ue()?;
            }
        }
        3 | 4 | 5 => {
            let _slice_group_change_direction_flag = reader.read_bit()?;
            let _slice_group_change_rate_minus1 = reader.read_ue()?;
        }
        6 => {
            let pic_size_in_map_units_minus1 = reader.read_ue()?;
            let bits = ceil_log2(num_slice_groups_minus1 + 1);
            for _ in 0..=pic_size_in_map_units_minus1 {
                if bits > 0 {
                    let _slice_group_id = reader.read_u(bits)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// Ceil(log2(n)) — the bit width needed to index `n` distinct values.
fn ceil_log2(n: u32) -> u32 {
    if n <= 1 {
        0
    } else {
        32 - (n - 1).leading_zeros()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pack bit-string fragments into bytes, zero-padding the last byte.
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
    fn ceil_log2_matches_table() {
        assert_eq!(ceil_log2(1), 0);
        assert_eq!(ceil_log2(2), 1);
        assert_eq!(ceil_log2(3), 2);
        assert_eq!(ceil_log2(4), 2);
        assert_eq!(ceil_log2(5), 3);
        assert_eq!(ceil_log2(8), 3);
        assert_eq!(ceil_log2(9), 4);
    }

    #[test]
    fn parses_minimal_cabac_pps() {
        // A typical single-slice-group High-profile PPS:
        //   pic_parameter_set_id          = 0   ue "1"
        //   seq_parameter_set_id          = 0   ue "1"
        //   entropy_coding_mode_flag      = 1   "1"  (CABAC)
        //   bottom_field_...present_flag  = 0   "0"
        //   num_slice_groups_minus1       = 0   ue "1"
        //   num_ref_idx_l0_default_m1     = 1   ue "010"
        //   num_ref_idx_l1_default_m1     = 0   ue "1"
        //   weighted_pred_flag            = 1   "1"
        //   weighted_bipred_idc           = 2   u(2) "10"
        //   pic_init_qp_minus26           = 0   se "1"
        //   pic_init_qs_minus26           = 0   se "1"
        //   chroma_qp_index_offset        = -2  se "00101" (codeNum 4 -> -2)
        //   deblocking_filter_control...  = 1   "1"
        //   constrained_intra_pred_flag   = 0   "0"
        //   redundant_pic_cnt_present     = 0   "0"
        //   rbsp_stop_one_bit             = 1   "1"  (no extension)
        let bits = [
            "1", "1", "1", "0", "1", "010", "1", "1", "10", "1", "1", "00101", "1", "0", "0",
            "1",
        ];
        let bytes = bits_to_bytes(&bits);
        let pps = parse_pps_rbsp(&bytes, 1).expect("parse PPS");

        assert_eq!(pps.pic_parameter_set_id, 0);
        assert_eq!(pps.seq_parameter_set_id, 0);
        assert!(pps.entropy_coding_mode_flag);
        assert_eq!(pps.num_slice_groups_minus1, 0);
        assert_eq!(pps.num_ref_idx_l0_default_active_minus1, 1);
        assert_eq!(pps.num_ref_idx_l1_default_active_minus1, 0);
        assert!(pps.weighted_pred_flag);
        assert_eq!(pps.weighted_bipred_idc, 2);
        assert_eq!(pps.pic_init_qp_minus26, 0);
        assert_eq!(pps.chroma_qp_index_offset, -2);
        assert!(pps.deblocking_filter_control_present_flag);
        assert!(!pps.transform_8x8_mode_flag);
        // No extension present, so it mirrors chroma_qp_index_offset.
        assert_eq!(pps.second_chroma_qp_index_offset, -2);
    }

    #[test]
    fn parses_pps_with_transform_8x8_extension() {
        // Same prefix as above but with the trailing extension:
        //   ... redundant_pic_cnt_present = 0 "0"
        //   transform_8x8_mode_flag       = 1 "1"
        //   pic_scaling_matrix_present    = 0 "0"
        //   second_chroma_qp_index_offset = 3 se "00110" (codeNum 5 -> 3)
        //   rbsp_stop_one_bit             = 1 "1"
        let bits = [
            "1", "1", "1", "0", "1", "010", "1", "1", "10", "1", "1", "00101", "1", "0", "0",
            "1", "0", "00110", "1",
        ];
        let bytes = bits_to_bytes(&bits);
        let pps = parse_pps_rbsp(&bytes, 1).expect("parse PPS");
        assert!(pps.transform_8x8_mode_flag);
        assert_eq!(pps.second_chroma_qp_index_offset, 3);
    }
}
