// Subset SPS MVC extension parser.
//
// The Subset SPS NAL (type 15) RBSP contains:
//
//   seq_parameter_set_data()                  // base SPS, large
//   if profile_idc in {118, 128, 134}:        // Multiview High profile family
//       bit_equal_to_one                f(1)  // must be 1
//       seq_parameter_set_mvc_extension()
//       mvc_vui_parameters_present_flag u(1)
//       ...
//
// This module implements only `seq_parameter_set_mvc_extension()` —
// the rest of Subset SPS (the base SPS + VUI) is full H.264 SPS
// territory which we will reuse from libavcodec when integration lands.
// Tests can feed the MVC extension subsection directly, which is what
// the forward-port codepath will produce.
//
// See H.264 § G.7.3.2.1.4 and § G.7.4.2.1.4.

use super::bitstream::{BitReader, ReadError};

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
