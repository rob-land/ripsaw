// Scaling-list (quantisation weight matrix) parsing, H.264 § 7.3.2.1.1.1 +
// § 8.5.9. Streams may carry custom inverse-quant weight matrices in the
// SPS and/or PPS; the dequant (§ 8.5.12 / § 8.5.13) multiplies the normAdjust
// tables by these weights instead of the flat 16. The base SPS/PPS parsers
// used to skip the lists; this module captures them and resolves the
// fall-back rules so the dequant can apply the real weights.
//
// Discovered necessary the hard way: a real Blu-ray stream carried a custom
// 8×8 intra matrix (DC weight 6, not 16), so the flat assumption put MB 0's
// luma ~2.7× too dark. The resolved matrices here are validated against JM's
// `qmatrix` dump.

use super::bitstream::{BitReader, ReadError};

/// The eight (4:2:0) resolved weight matrices in raster order: lists 0..5 are
/// 4×4 (intra Y/Cb/Cr, inter Y/Cb/Cr), lists 6..7 are 8×8 (intra Y, inter Y).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScalingLists {
    pub list_4x4: [[[i32; 4]; 4]; 6],
    pub list_8x8: [[[i32; 8]; 8]; 2],
}

// Up-right diagonal zig-zag scan orders (scan index -> raster index). Scaling
// lists are transmitted in this order; the dequant uses raster matrices.
#[rustfmt::skip]
const ZIGZAG_4X4: [usize; 16] = [0, 1, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15];
#[rustfmt::skip]
const ZIGZAG_8X8: [usize; 64] = [
     0,  1,  8, 16,  9,  2,  3, 10, 17, 24, 32, 25, 18, 11,  4,  5,
    12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13,  6,  7, 14, 21, 28,
    35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51,
    58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

// Default scaling lists (H.264 Tables 7-3 / 7-4), in zig-zag scan order.
#[rustfmt::skip]
const DEFAULT_4X4_INTRA: [i32; 16] = [6,13,13,20,20,20,28,28,28,28,32,32,32,37,37,42];
#[rustfmt::skip]
const DEFAULT_4X4_INTER: [i32; 16] = [10,14,14,20,20,20,24,24,24,24,27,27,27,30,30,34];
#[rustfmt::skip]
const DEFAULT_8X8_INTRA: [i32; 64] = [
     6,10,10,13,11,13,16,16,16,16,18,18,18,18,18,23,
    23,23,23,23,23,25,25,25,25,25,25,25,27,27,27,27,
    27,27,27,27,29,29,29,29,29,29,29,31,31,31,31,33,
    33,33,36,36,36,38,38,40,40,42,45,45,47,47,48,57,
];
#[rustfmt::skip]
const DEFAULT_8X8_INTER: [i32; 64] = [
     9,13,13,15,13,15,17,17,17,17,19,19,19,19,19,21,
    21,21,21,21,21,22,22,22,22,22,22,22,24,24,24,24,
    24,24,24,24,25,25,25,25,25,25,25,27,27,27,27,28,
    28,28,30,30,30,32,32,33,33,35,35,38,38,40,40,42,
];

/// Parse one `scaling_list()` (§ 7.3.2.1.1.1): reconstruct the `size`
/// weights (scan order) from the signalled deltas. Returns the weights and
/// the `useDefaultScalingMatrixFlag` (set when the first delta zeroes the
/// scale → the caller substitutes the default list).
pub fn parse_scaling_list(reader: &mut BitReader<'_>, size: usize) -> Result<(Vec<i32>, bool), ReadError> {
    let mut scaling = vec![0i32; size];
    let mut last_scale = 8i32;
    let mut next_scale = 8i32;
    let mut use_default = false;
    for (j, slot) in scaling.iter_mut().enumerate() {
        if next_scale != 0 {
            let delta = reader.read_se()?;
            next_scale = (last_scale + delta + 256).rem_euclid(256);
            if j == 0 && next_scale == 0 {
                use_default = true;
            }
        }
        *slot = if next_scale == 0 { last_scale } else { next_scale };
        last_scale = *slot;
    }
    Ok((scaling, use_default))
}

fn scan_to_raster_4x4(scan: &[i32]) -> [[i32; 4]; 4] {
    let mut m = [[0i32; 4]; 4];
    for (k, &v) in scan.iter().enumerate() {
        let r = ZIGZAG_4X4[k];
        m[r / 4][r % 4] = v;
    }
    m
}

fn scan_to_raster_8x8(scan: &[i32]) -> [[i32; 8]; 8] {
    let mut m = [[0i32; 8]; 8];
    for (k, &v) in scan.iter().enumerate() {
        let r = ZIGZAG_8X8[k];
        m[r / 8][r % 8] = v;
    }
    m
}

/// Parse a `scaling_matrix` (the `count` lists after a
/// seq/pic_scaling_matrix_present_flag), applying the § 8.5.9 fall-back
/// rules (default-A: an absent or use-default list takes the default for
/// list 0/3/6/7, otherwise the previous list). `count` is 8 for 4:2:0,
/// 12 for 4:4:4 (the extra chroma 8×8 lists are parsed but not stored here).
pub fn parse_scaling_matrix(reader: &mut BitReader<'_>, count: usize) -> Result<ScalingLists, ReadError> {
    let mut l4: Vec<[[i32; 4]; 4]> = Vec::new();
    let mut l8: Vec<[[i32; 8]; 8]> = Vec::new();

    for i in 0..count {
        let is_8x8 = i >= 6;
        let size = if is_8x8 { 64 } else { 16 };
        let present = reader.read_bit()?;

        if !present {
            // Fall-back rule set A.
            if is_8x8 {
                let idx = i - 6;
                let mat = if idx == 0 {
                    scan_to_raster_8x8(&DEFAULT_8X8_INTRA)
                } else if idx == 1 {
                    scan_to_raster_8x8(&DEFAULT_8X8_INTER)
                } else {
                    // 4:4:4 chroma 8×8 lists fall back to the previous.
                    l8[idx - 2]
                };
                l8.push(mat);
            } else {
                let mat = match i {
                    0 => scan_to_raster_4x4(&DEFAULT_4X4_INTRA),
                    3 => scan_to_raster_4x4(&DEFAULT_4X4_INTER),
                    _ => l4[i - 1], // Cb/Cr take the previous list.
                };
                l4.push(mat);
            }
            continue;
        }

        if is_8x8 {
            let (scan, use_default) = parse_scaling_list(reader, size)?;
            let idx = i - 6;
            let mat = if use_default {
                if idx == 0 {
                    scan_to_raster_8x8(&DEFAULT_8X8_INTRA)
                } else {
                    scan_to_raster_8x8(&DEFAULT_8X8_INTER)
                }
            } else {
                scan_to_raster_8x8(&scan)
            };
            l8.push(mat);
        } else {
            let (scan, use_default) = parse_scaling_list(reader, size)?;
            let mat = if use_default {
                if i < 3 {
                    scan_to_raster_4x4(&DEFAULT_4X4_INTRA)
                } else {
                    scan_to_raster_4x4(&DEFAULT_4X4_INTER)
                }
            } else {
                scan_to_raster_4x4(&scan)
            };
            l4.push(mat);
        }
    }

    // Store the 4:2:0 set (6 × 4×4, 2 × 8×8); 4:4:4 extras are ignored.
    let mut list_4x4 = [[[0i32; 4]; 4]; 6];
    for (i, m) in l4.iter().take(6).enumerate() {
        list_4x4[i] = *m;
    }
    let mut list_8x8 = [[[0i32; 8]; 8]; 2];
    for (i, m) in l8.iter().take(2).enumerate() {
        list_8x8[i] = *m;
    }
    Ok(ScalingLists { list_4x4, list_8x8 })
}

impl ScalingLists {
    /// All-flat (weight 16) — the implied matrices when no scaling matrix is
    /// signalled at all.
    pub fn flat() -> Self {
        ScalingLists { list_4x4: [[[16; 4]; 4]; 6], list_8x8: [[[16; 8]; 8]; 2] }
    }

    /// The 8×8 intra-luma weight matrix (list 6) — used by I_8x8 dequant.
    pub fn intra_8x8_luma(&self) -> &[[i32; 8]; 8] {
        &self.list_8x8[0]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_lists_are_sixteen() {
        let f = ScalingLists::flat();
        assert_eq!(f.intra_8x8_luma(), &[[16; 8]; 8]);
        assert_eq!(f.list_4x4[0], [[16; 4]; 4]);
    }

    #[test]
    fn default_8x8_intra_dc_is_six() {
        // DC (raster [0][0]) of the default 8×8 intra list is 6.
        let m = scan_to_raster_8x8(&DEFAULT_8X8_INTRA);
        assert_eq!(m[0][0], 6);
    }

    #[test]
    fn zigzag_8x8_is_a_permutation() {
        let mut seen = [false; 64];
        for &r in &ZIGZAG_8X8 {
            assert!(!seen[r]);
            seen[r] = true;
        }
        assert!(seen.iter().all(|&b| b));
    }
}
