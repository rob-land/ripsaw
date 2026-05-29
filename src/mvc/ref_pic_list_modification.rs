// Parse `ref_pic_list_modification()` from a slice header, including the
// MVC additions defined in H.264 Annex G § G.7.3.3.1.1. The base spec
// recognises `modification_of_pic_nums_idc` values 0, 1, 2, 3; Annex G
// adds 4 and 5 for inter-view reference modifications and introduces
// `abs_diff_view_idx_minus1`.
//
// The base slice header (slice_type, frame_num, ...) is much larger
// than this and needs an active SPS for parsing; that's full H.264
// territory we'll reuse from libavcodec. This module just handles the
// MVC-specific modification subroutine, which is testable in isolation
// given the slice type.

use super::bitstream::{BitReader, ReadError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefPicListModification {
    /// `modification_of_pic_nums_idc` == 0: subtract from the prediction
    /// value of the reference picture number.
    PicNumSub { abs_diff_pic_num_minus1: u32 },
    /// idc == 1: add to the prediction value.
    PicNumAdd { abs_diff_pic_num_minus1: u32 },
    /// idc == 2: pick a long-term reference picture by its long-term
    /// picture number.
    LongTerm { long_term_pic_num: u32 },
    /// idc == 4 (MVC, Annex G): subtract from the prediction value of
    /// the inter-view reference view index.
    InterViewSub { abs_diff_view_idx_minus1: u32 },
    /// idc == 5 (MVC, Annex G): add to the prediction value.
    InterViewAdd { abs_diff_view_idx_minus1: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RefPicListModifications {
    pub list0: Option<Vec<RefPicListModification>>,
    pub list1: Option<Vec<RefPicListModification>>,
}

/// Read a base-H.264 `slice_type` (the value before the `% 5` reduction)
/// and decide whether the slice has list-0 / list-1 modifications to
/// parse. P / SP / B are L0; B is also L1; I / SI have neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SliceKind {
    P,
    Sp,
    B,
    I,
    Si,
}

impl SliceKind {
    pub fn from_slice_type(slice_type: u32) -> Self {
        match slice_type % 5 {
            0 => SliceKind::P,
            1 => SliceKind::B,
            2 => SliceKind::I,
            3 => SliceKind::Sp,
            _ => SliceKind::Si,
        }
    }

    fn has_list0(&self) -> bool {
        matches!(self, SliceKind::P | SliceKind::Sp | SliceKind::B)
    }

    fn has_list1(&self) -> bool {
        matches!(self, SliceKind::B)
    }
}

const MAX_MODIFICATIONS_PER_LIST: usize = 64;

/// Parse `ref_pic_list_modification()` for the given slice type.
pub fn parse_ref_pic_list_modification(
    reader: &mut BitReader<'_>,
    kind: SliceKind,
) -> Result<RefPicListModifications, ReadError> {
    let list0 = if kind.has_list0() {
        Some(parse_one_list(reader)?)
    } else {
        None
    };
    let list1 = if kind.has_list1() {
        Some(parse_one_list(reader)?)
    } else {
        None
    };
    Ok(RefPicListModifications { list0, list1 })
}

fn parse_one_list(reader: &mut BitReader<'_>) -> Result<Vec<RefPicListModification>, ReadError> {
    let flag = reader.read_bit()?;
    if !flag {
        return Ok(Vec::new());
    }
    let mut mods = Vec::new();
    loop {
        if mods.len() > MAX_MODIFICATIONS_PER_LIST {
            // Defensive cap. Real bitstreams are bounded by
            // num_ref_idx_l*_active_minus1 + 1 which is small.
            return Err(ReadError::ExpGolombOverflow);
        }
        let idc = reader.read_ue()?;
        match idc {
            0 => mods.push(RefPicListModification::PicNumSub {
                abs_diff_pic_num_minus1: reader.read_ue()?,
            }),
            1 => mods.push(RefPicListModification::PicNumAdd {
                abs_diff_pic_num_minus1: reader.read_ue()?,
            }),
            2 => mods.push(RefPicListModification::LongTerm {
                long_term_pic_num: reader.read_ue()?,
            }),
            3 => return Ok(mods),
            4 => mods.push(RefPicListModification::InterViewSub {
                abs_diff_view_idx_minus1: reader.read_ue()?,
            }),
            5 => mods.push(RefPicListModification::InterViewAdd {
                abs_diff_view_idx_minus1: reader.read_ue()?,
            }),
            // Reserved -- terminate to avoid runaway parsing.
            _ => return Ok(mods),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bits_to_bytes(bits: &str) -> Vec<u8> {
        let mut s = bits.replace(|c: char| c == ' ' || c == '|', "");
        let pad = (8 - (s.len() % 8)) % 8;
        s.extend(std::iter::repeat('0').take(pad));
        s.as_bytes()
            .chunks(8)
            .map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap(), 2).unwrap())
            .collect()
    }

    #[test]
    fn slice_kind_classification() {
        assert_eq!(SliceKind::from_slice_type(0), SliceKind::P);
        assert_eq!(SliceKind::from_slice_type(5), SliceKind::P);
        assert_eq!(SliceKind::from_slice_type(1), SliceKind::B);
        assert_eq!(SliceKind::from_slice_type(2), SliceKind::I);
        assert_eq!(SliceKind::from_slice_type(3), SliceKind::Sp);
        assert_eq!(SliceKind::from_slice_type(4), SliceKind::Si);
    }

    #[test]
    fn i_slice_has_no_modification_data() {
        let bytes = [0u8; 1];
        let mut r = BitReader::new(&bytes);
        let mods = parse_ref_pic_list_modification(&mut r, SliceKind::I).unwrap();
        assert!(mods.list0.is_none());
        assert!(mods.list1.is_none());
    }

    #[test]
    fn p_slice_flag_zero_yields_empty_list() {
        // 1 bit: 0 (ref_pic_list_modification_flag_l0 == 0)
        let bytes = [0b0_0000000];
        let mut r = BitReader::new(&bytes);
        let mods = parse_ref_pic_list_modification(&mut r, SliceKind::P).unwrap();
        assert_eq!(mods.list0.as_deref(), Some(&[][..]));
        assert!(mods.list1.is_none());
    }

    #[test]
    fn p_slice_with_idc0_then_terminator() {
        // bits:
        //   ref_pic_list_modification_flag_l0 = 1
        //   idc = 0  (ue: "1")
        //   abs_diff_pic_num_minus1 = 0  (ue: "1")
        //   idc = 3  (ue: "00100")  -> terminator
        let bytes = bits_to_bytes("1 1 1 00100");
        let mut r = BitReader::new(&bytes);
        let mods = parse_ref_pic_list_modification(&mut r, SliceKind::P).unwrap();
        let l0 = mods.list0.expect("list0 present");
        assert_eq!(l0.len(), 1);
        assert_eq!(l0[0], RefPicListModification::PicNumSub { abs_diff_pic_num_minus1: 0 });
    }

    #[test]
    fn p_slice_with_mvc_inter_view_modification_idc4() {
        // bits:
        //   flag_l0 = 1
        //   idc = 4    (ue codeword for codeNum 4 is "00101")
        //   abs_diff_view_idx_minus1 = 0 (ue "1")
        //   idc = 3    (ue "00100")
        let bytes = bits_to_bytes("1 00101 1 00100");
        let mut r = BitReader::new(&bytes);
        let mods = parse_ref_pic_list_modification(&mut r, SliceKind::P).unwrap();
        let l0 = mods.list0.expect("list0 present");
        assert_eq!(l0.len(), 1);
        assert_eq!(
            l0[0],
            RefPicListModification::InterViewSub { abs_diff_view_idx_minus1: 0 }
        );
    }

    #[test]
    fn b_slice_with_two_lists() {
        // List 0: flag=1, idc=5 (codeNum 5 = "00110"), abs_diff_view_idx_m1=2 (ue codeNum 2 = "011"), idc=3 ("00100")
        // List 1: flag=0
        let bytes = bits_to_bytes("1 00110 011 00100 0");
        let mut r = BitReader::new(&bytes);
        let mods = parse_ref_pic_list_modification(&mut r, SliceKind::B).unwrap();
        let l0 = mods.list0.expect("list0 present");
        assert_eq!(l0.len(), 1);
        assert_eq!(
            l0[0],
            RefPicListModification::InterViewAdd { abs_diff_view_idx_minus1: 2 }
        );
        assert_eq!(mods.list1.as_deref(), Some(&[][..]));
    }

    #[test]
    fn mixed_base_and_mvc_modifications_in_one_list() {
        // flag=1, then:
        //   idc=0 ("1"), abs_diff_pic_num_minus1=1 ("010")
        //   idc=2 ("011"), long_term_pic_num=4 ("00101")
        //   idc=5 ("00110"), abs_diff_view_idx_minus1=0 ("1")
        //   idc=3 ("00100")
        let bytes = bits_to_bytes("1 1 010 011 00101 00110 1 00100");
        let mut r = BitReader::new(&bytes);
        let mods = parse_ref_pic_list_modification(&mut r, SliceKind::P).unwrap();
        let l0 = mods.list0.expect("list0 present");
        assert_eq!(l0.len(), 3);
        assert_eq!(l0[0], RefPicListModification::PicNumSub { abs_diff_pic_num_minus1: 1 });
        assert_eq!(l0[1], RefPicListModification::LongTerm { long_term_pic_num: 4 });
        assert_eq!(l0[2], RefPicListModification::InterViewAdd { abs_diff_view_idx_minus1: 0 });
    }
}
