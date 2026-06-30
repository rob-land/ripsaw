// Inverse scaling (dequantisation) and inverse transforms
// (ITU-T H.264 § 8.5). Second building block of the libmvc decode core
// (docs/libmvc-poc.md): once CABAC has produced coefficient *levels*,
// these turn them into spatial residual samples.
//
// Scope here is the 4×4 residual path used by I_NxN luma + chroma AC, the
// DC transforms for Intra_16x16 luma (4×4 Hadamard) and 4:2:0 chroma (2×2
// Hadamard), and the 8×8 residual path used by I_8x8 luma. Custom scaling
// lists are not handled — flat weightScale (=16) only, which is what
// asserts elsewhere bail on; Blu-ray streams in the corpus use flat scaling.

/// `normAdjust4x4[m][class]` — H.264 § 8.5.9 (the "V" matrix). `class` is
/// 0 for even/even positions, 1 for odd/odd, 2 otherwise.
#[rustfmt::skip]
static NORM_ADJUST_4X4: [[i32; 3]; 6] = [
    [10, 16, 13],
    [11, 18, 14],
    [13, 20, 16],
    [14, 23, 18],
    [16, 25, 20],
    [18, 29, 23],
];

fn class_4x4(i: usize, j: usize) -> usize {
    if i % 2 == 0 && j % 2 == 0 {
        0
    } else if i % 2 == 1 && j % 2 == 1 {
        1
    } else {
        2
    }
}

/// Inverse-scale (dequantise) a 4×4 block of coefficient levels in place,
/// for the residual path (§ 8.5.12.1), assuming flat scaling (weightScale
/// = 16). `qp` is the block's QP (0..=51). Every position — including DC —
/// is scaled; the separate-DC handling applies only to Intra_16x16 luma
/// and chroma, which call the DC helpers below instead.
pub fn dequant_4x4(levels: &mut [[i32; 4]; 4], qp: i32) {
    let m = (qp % 6) as usize;
    let e = qp / 6;
    for i in 0..4 {
        for j in 0..4 {
            let scale = 16 * NORM_ADJUST_4X4[m][class_4x4(i, j)];
            let c = levels[i][j];
            levels[i][j] = if qp >= 24 {
                (c * scale) << (e - 4)
            } else {
                (c * scale + (1 << (3 - e))) >> (4 - e)
            };
        }
    }
}

/// Inverse 4×4 residual transform (§ 8.5.12.2). Input is the dequantised
/// block `d`; output is the residual sample block `r`, `r = (H + 32) >> 6`.
pub fn idct_4x4(d: &[[i32; 4]; 4]) -> [[i32; 4]; 4] {
    let mut f = [[0i32; 4]; 4];
    // Horizontal (rows).
    for i in 0..4 {
        let e0 = d[i][0] + d[i][2];
        let e1 = d[i][0] - d[i][2];
        let e2 = (d[i][1] >> 1) - d[i][3];
        let e3 = d[i][1] + (d[i][3] >> 1);
        f[i][0] = e0 + e3;
        f[i][1] = e1 + e2;
        f[i][2] = e1 - e2;
        f[i][3] = e0 - e3;
    }
    // Vertical (columns) + round/shift.
    let mut r = [[0i32; 4]; 4];
    for j in 0..4 {
        let g0 = f[0][j] + f[2][j];
        let g1 = f[0][j] - f[2][j];
        let g2 = (f[1][j] >> 1) - f[3][j];
        let g3 = f[1][j] + (f[3][j] >> 1);
        r[0][j] = ((g0 + g3) + 32) >> 6;
        r[1][j] = ((g1 + g2) + 32) >> 6;
        r[2][j] = ((g1 - g2) + 32) >> 6;
        r[3][j] = ((g0 - g3) + 32) >> 6;
    }
    r
}

/// Convenience: dequantise then inverse-transform a 4×4 residual block.
pub fn reconstruct_residual_4x4(levels: &[[i32; 4]; 4], qp: i32) -> [[i32; 4]; 4] {
    let mut d = *levels;
    dequant_4x4(&mut d, qp);
    idct_4x4(&d)
}

/// `dequant_coef8[m][row][col]` — the 8×8 inverse-scaling weight matrix
/// (JM `dequant_coef8`, == normAdjust8x8 of H.264 § 8.5.13.1), indexed by
/// `m = qP % 6`. With flat weightScale (=16) the level scale is `16 ×` this.
#[rustfmt::skip]
static DEQUANT_COEF8: [[[i32; 8]; 8]; 6] = [
    [[20,19,25,19,20,19,25,19],[19,18,24,18,19,18,24,18],[25,24,32,24,25,24,32,24],[19,18,24,18,19,18,24,18],
     [20,19,25,19,20,19,25,19],[19,18,24,18,19,18,24,18],[25,24,32,24,25,24,32,24],[19,18,24,18,19,18,24,18]],
    [[22,21,28,21,22,21,28,21],[21,19,26,19,21,19,26,19],[28,26,35,26,28,26,35,26],[21,19,26,19,21,19,26,19],
     [22,21,28,21,22,21,28,21],[21,19,26,19,21,19,26,19],[28,26,35,26,28,26,35,26],[21,19,26,19,21,19,26,19]],
    [[26,24,33,24,26,24,33,24],[24,23,31,23,24,23,31,23],[33,31,42,31,33,31,42,31],[24,23,31,23,24,23,31,23],
     [26,24,33,24,26,24,33,24],[24,23,31,23,24,23,31,23],[33,31,42,31,33,31,42,31],[24,23,31,23,24,23,31,23]],
    [[28,26,35,26,28,26,35,26],[26,25,33,25,26,25,33,25],[35,33,45,33,35,33,45,33],[26,25,33,25,26,25,33,25],
     [28,26,35,26,28,26,35,26],[26,25,33,25,26,25,33,25],[35,33,45,33,35,33,45,33],[26,25,33,25,26,25,33,25]],
    [[32,30,40,30,32,30,40,30],[30,28,38,28,30,28,38,28],[40,38,51,38,40,38,51,38],[30,28,38,28,30,28,38,28],
     [32,30,40,30,32,30,40,30],[30,28,38,28,30,28,38,28],[40,38,51,38,40,38,51,38],[30,28,38,28,30,28,38,28]],
    [[36,34,46,34,36,34,46,34],[34,32,43,32,34,32,43,32],[46,43,58,43,46,43,58,43],[34,32,43,32,34,32,43,32],
     [36,34,46,34,36,34,46,34],[34,32,43,32,34,32,43,32],[46,43,58,43,46,43,58,43],[34,32,43,32,34,32,43,32]],
];

/// Inverse-scale (dequantise) an 8×8 block of coefficient levels in place
/// (§ 8.5.13.1). `weight` is the inverse-quant weight matrix (raster order):
/// flat 16 for default scaling, or the stream's 8×8 scaling list. Mirrors JM
/// exactly: `d = (level · normAdjust8x8[m]·weight << (qP/6) + 32) >> 6`.
pub fn dequant_8x8(levels: &mut [[i32; 8]; 8], qp: i32, weight: &[[i32; 8]; 8]) {
    let m = (qp % 6) as usize;
    let shift = qp / 6;
    for i in 0..8 {
        for j in 0..8 {
            let scale = (DEQUANT_COEF8[m][i][j] * weight[i][j]) as i64;
            let x = ((levels[i][j] as i64) * scale) << shift;
            levels[i][j] = ((x + 32) >> 6) as i32;
        }
    }
}

/// The flat (weight 16) 8×8 inverse-quant matrix — default scaling.
pub const FLAT_WEIGHT_8X8: [[i32; 8]; 8] = [[16; 8]; 8];

/// Inverse 8×8 residual transform (§ 8.5.13.2), the JM `inverse8x8`
/// butterfly applied to rows then columns. No final rounding/shift — the
/// caller applies `(H + 32) >> 6` when adding to the prediction (JM
/// `recon8x8`, `DQ_BITS_8 = 6`). `reconstruct_residual_8x8` does both.
pub fn inverse_8x8(block: &[[i32; 8]; 8]) -> [[i32; 8]; 8] {
    fn pass(p: [i32; 8]) -> [i32; 8] {
        let (p0, p1, p2, p3, p4, p5, p6, p7) = (p[0], p[1], p[2], p[3], p[4], p[5], p[6], p[7]);
        let a0 = p0 + p4;
        let a1 = p0 - p4;
        let a2 = p6 - (p2 >> 1);
        let a3 = p2 + (p6 >> 1);
        let b0 = a0 + a3;
        let b2 = a1 - a2;
        let b4 = a1 + a2;
        let b6 = a0 - a3;
        let a0 = -p3 + p5 - p7 - (p7 >> 1);
        let a1 = p1 + p7 - p3 - (p3 >> 1);
        let a2 = -p1 + p7 + p5 + (p5 >> 1);
        let a3 = p3 + p5 + p1 + (p1 >> 1);
        let b1 = a0 + (a3 >> 2);
        let b3 = a1 + (a2 >> 2);
        let b5 = a2 - (a1 >> 2);
        let b7 = a3 - (a0 >> 2);
        [b0 + b7, b2 - b5, b4 + b3, b6 + b1, b6 - b1, b4 - b3, b2 + b5, b0 - b7]
    }
    // Horizontal (each row).
    let mut tmp = [[0i32; 8]; 8];
    for i in 0..8 {
        tmp[i] = pass(block[i]);
    }
    // Vertical (each column).
    let mut out = [[0i32; 8]; 8];
    for j in 0..8 {
        let col = pass([tmp[0][j], tmp[1][j], tmp[2][j], tmp[3][j], tmp[4][j], tmp[5][j], tmp[6][j], tmp[7][j]]);
        for i in 0..8 {
            out[i][j] = col[i];
        }
    }
    out
}

/// Dequantise, inverse-transform, and round an 8×8 residual block: returns
/// the residual samples `(H + 32) >> 6` to be added to the prediction.
/// `weight` is the 8×8 inverse-quant weight matrix (see [`dequant_8x8`]).
pub fn reconstruct_residual_8x8(levels: &[[i32; 8]; 8], qp: i32, weight: &[[i32; 8]; 8]) -> [[i32; 8]; 8] {
    let mut d = *levels;
    dequant_8x8(&mut d, qp, weight);
    let h = inverse_8x8(&d);
    let mut r = [[0i32; 8]; 8];
    for i in 0..8 {
        for j in 0..8 {
            r[i][j] = (h[i][j] + 32) >> 6;
        }
    }
    r
}

/// 8×8 zig-zag scan (frame), JM `SNGL_SCAN8x8`: scan index → (row, col).
#[rustfmt::skip]
static SCAN_8X8: [(usize, usize); 64] = [
    (0,0),(0,1),(1,0),(2,0),(1,1),(0,2),(0,3),(1,2),(2,1),(3,0),(4,0),(3,1),(2,2),(1,3),(0,4),(0,5),
    (1,4),(2,3),(3,2),(4,1),(5,0),(6,0),(5,1),(4,2),(3,3),(2,4),(1,5),(0,6),(0,7),(1,6),(2,5),(3,4),
    (4,3),(5,2),(6,1),(7,0),(7,1),(6,2),(5,3),(4,4),(3,5),(2,6),(1,7),(2,7),(3,6),(4,5),(5,4),(6,3),
    (7,2),(7,3),(6,4),(5,5),(4,6),(3,7),(4,7),(5,6),(6,5),(7,4),(7,5),(6,6),(5,7),(6,7),(7,6),(7,7),
];

/// Place 64 scan-order coefficients into an 8×8 raster block `[row][col]`.
pub fn inverse_scan_8x8(scan: &[i32; 64]) -> [[i32; 8]; 8] {
    let mut out = [[0i32; 8]; 8];
    for (k, &(row, col)) in SCAN_8X8.iter().enumerate() {
        out[row][col] = scan[k];
    }
    out
}

/// Inverse 4×4 Hadamard transform of the Intra_16x16 luma DC coefficients
/// (§ 8.5.10.2), followed by DC scaling (§ 8.5.10.3). Returns the 16
/// dequantised DC values, to be slotted into each 4×4 block's `[0][0]`
/// before its residual transform.
pub fn luma_dc_4x4(c: &[[i32; 4]; 4], qp: i32) -> [[i32; 4]; 4] {
    // Hadamard (separable, same butterfly both directions).
    let mut f = [[0i32; 4]; 4];
    for i in 0..4 {
        let a0 = c[i][0] + c[i][2];
        let a1 = c[i][0] - c[i][2];
        let a2 = c[i][1] - c[i][3];
        let a3 = c[i][1] + c[i][3];
        f[i][0] = a0 + a3;
        f[i][1] = a1 + a2;
        f[i][2] = a1 - a2;
        f[i][3] = a0 - a3;
    }
    let mut g = [[0i32; 4]; 4];
    for j in 0..4 {
        let a0 = f[0][j] + f[2][j];
        let a1 = f[0][j] - f[2][j];
        let a2 = f[1][j] - f[3][j];
        let a3 = f[1][j] + f[3][j];
        g[0][j] = a0 + a3;
        g[1][j] = a1 + a2;
        g[2][j] = a1 - a2;
        g[3][j] = a0 - a3;
    }
    // DC scaling.
    let m = (qp % 6) as usize;
    let e = qp / 6;
    let scale = 16 * NORM_ADJUST_4X4[m][0];
    let mut out = [[0i32; 4]; 4];
    for i in 0..4 {
        for j in 0..4 {
            out[i][j] = if qp >= 36 {
                (g[i][j] * scale) << (e - 6)
            } else {
                (g[i][j] * scale + (1 << (5 - e))) >> (6 - e)
            };
        }
    }
    out
}

/// Inverse 2×2 Hadamard transform + scaling of the 4:2:0 chroma DC
/// coefficients (§ 8.5.11.1/2). `c` is `[[c00, c01], [c10, c11]]`.
pub fn chroma_dc_2x2(c: &[[i32; 2]; 2], qp: i32) -> [[i32; 2]; 2] {
    // 2×2 Hadamard.
    let f00 = c[0][0] + c[0][1] + c[1][0] + c[1][1];
    let f01 = c[0][0] - c[0][1] + c[1][0] - c[1][1];
    let f10 = c[0][0] + c[0][1] - c[1][0] - c[1][1];
    let f11 = c[0][0] - c[0][1] - c[1][0] + c[1][1];
    let m = (qp % 6) as usize;
    let e = qp / 6;
    let scale = 16 * NORM_ADJUST_4X4[m][0];
    let s = |v: i32| -> i32 { ((v * scale) << e) >> 5 };
    [[s(f00), s(f01)], [s(f10), s(f11)]]
}

/// Inverse zig-zag scan for a 4×4 block (§ 8.5.6, frame scan, Table 8-13):
/// place coefficients given in scan order into their raster (row-major)
/// positions. Bridges `residual::decode_residual_block` (scan order) to
/// the transform (raster).
#[rustfmt::skip]
static ZIGZAG_4X4: [usize; 16] = [
    0, 1, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15,
];

pub fn inverse_scan_4x4(scan: &[i32; 16]) -> [[i32; 4]; 4] {
    let mut r = [[0i32; 4]; 4];
    for (s, &raster) in ZIGZAG_4X4.iter().enumerate() {
        r[raster / 4][raster % 4] = scan[s];
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inverse_scan_places_first_coeffs_in_zigzag_order() {
        let mut scan = [0i32; 16];
        scan[0] = 1; // -> raster (0,0)
        scan[1] = 2; // -> raster (0,1)
        scan[2] = 3; // -> raster (1,0)
        scan[3] = 4; // -> raster (2,0)
        let r = inverse_scan_4x4(&scan);
        assert_eq!(r[0][0], 1);
        assert_eq!(r[0][1], 2);
        assert_eq!(r[1][0], 3);
        assert_eq!(r[2][0], 4);
        // Last scan position maps to the bottom-right corner.
        let mut scan = [0i32; 16];
        scan[15] = 9;
        assert_eq!(inverse_scan_4x4(&scan)[3][3], 9);
    }

    #[test]
    fn inverse_scan_is_a_permutation() {
        // Each raster cell is written exactly once (no clobbering / gaps).
        let scan: [i32; 16] = std::array::from_fn(|i| i as i32 + 1);
        let r = inverse_scan_4x4(&scan);
        let mut seen = [false; 16];
        for row in r {
            for v in row {
                seen[(v - 1) as usize] = true;
            }
        }
        assert!(seen.iter().all(|&b| b));
    }

    #[test]
    fn dc_only_block_reconstructs_to_constant() {
        // Only the DC level set. qP = 24: class(0,0)=0, normAdjust[0][0]=10,
        // LevelScale = 160; qP>=24 -> d00 = level*160 << 0 = 160.
        // The inverse transform of a DC-only block is constant
        // (160 + 32) >> 6 = 3 everywhere.
        let mut levels = [[0i32; 4]; 4];
        levels[0][0] = 1;
        let r = reconstruct_residual_4x4(&levels, 24);
        assert_eq!(r, [[3; 4]; 4]);
    }

    #[test]
    fn dequant_4x4_dc_value() {
        let mut d = [[0i32; 4]; 4];
        d[0][0] = 1;
        dequant_4x4(&mut d, 24);
        assert_eq!(d[0][0], 160);
        // Position class drives the scale: (1,1) is class 1 -> normAdjust
        // [0][1] = 16 -> 16*16 = 256.
        let mut d = [[0i32; 4]; 4];
        d[1][1] = 1;
        dequant_4x4(&mut d, 24);
        assert_eq!(d[1][1], 256);
    }

    #[test]
    fn idct_4x4_is_linear_and_dc_flat() {
        // A pure-DC dequantised block (all energy in d[0][0]) yields a flat
        // residual; verify the (h + 32) >> 6 rounding for a known value.
        let mut d = [[0i32; 4]; 4];
        d[0][0] = 64; // -> (64 + 32) >> 6 = 1
        assert_eq!(idct_4x4(&d), [[1; 4]; 4]);
        d[0][0] = 256; // -> (256 + 32) >> 6 = 4
        assert_eq!(idct_4x4(&d), [[4; 4]; 4]);
    }

    #[test]
    fn idct_4x4_zero_is_zero() {
        assert_eq!(idct_4x4(&[[0; 4]; 4]), [[0; 4]; 4]);
    }

    #[test]
    fn luma_dc_constant_input_is_dc_only_output() {
        // A constant DC field has energy only at the Hadamard DC (0,0);
        // all other transformed DCs are zero.
        let c = [[5i32; 4]; 4];
        let out = luma_dc_4x4(&c, 30);
        for i in 0..4 {
            for j in 0..4 {
                if i == 0 && j == 0 {
                    assert!(out[0][0] != 0);
                } else {
                    assert_eq!(out[i][j], 0, "non-DC at ({i},{j})");
                }
            }
        }
    }

    #[test]
    fn chroma_dc_hadamard_separates_constant() {
        // Constant input -> only the (0,0) Hadamard term is non-zero.
        let out = chroma_dc_2x2(&[[7, 7], [7, 7]], 30);
        assert!(out[0][0] != 0);
        assert_eq!(out[0][1], 0);
        assert_eq!(out[1][0], 0);
        assert_eq!(out[1][1], 0);
    }

    #[test]
    fn inverse_8x8_dc_only_is_uniform() {
        // A pure-DC 8×8 block (only [0][0] set) inverse-transforms to a flat
        // block — every sample equals the DC value (definition of DC).
        let mut block = [[0i32; 8]; 8];
        block[0][0] = 64;
        let out = inverse_8x8(&block);
        for row in &out {
            for &v in row {
                assert_eq!(v, 64);
            }
        }
    }

    #[test]
    fn inverse_8x8_is_linear() {
        // The transform is linear: T(a) + T(b) == T(a + b).
        let mut a = [[0i32; 8]; 8];
        let mut b = [[0i32; 8]; 8];
        a[0][0] = 100;
        a[1][3] = -8;
        a[7][7] = 5;
        b[0][0] = -20;
        b[2][1] = 12;
        b[4][4] = 30;
        let ta = inverse_8x8(&a);
        let tb = inverse_8x8(&b);
        let mut sum = [[0i32; 8]; 8];
        for i in 0..8 {
            for j in 0..8 {
                sum[i][j] = a[i][j] + b[i][j];
            }
        }
        let tsum = inverse_8x8(&sum);
        for i in 0..8 {
            for j in 0..8 {
                assert_eq!(ta[i][j] + tb[i][j], tsum[i][j]);
            }
        }
    }

    #[test]
    fn dequant_8x8_matches_jm_formula() {
        // JM: d = (level · weight·dequant_coef8[m][i][j] << (qp/6) + 32) >> 6.
        // Flat DC at qp 0: dequant_coef8[0][0][0] = 20 -> (level·320 + 32) >> 6.
        let mut levels = [[0i32; 8]; 8];
        levels[0][0] = -3823;
        dequant_8x8(&mut levels, 0, &FLAT_WEIGHT_8X8);
        assert_eq!(levels[0][0], ((-3823i64 * 320 + 32) >> 6) as i32);
        // qp 6 doubles the per-step shift (qp/6 = 1): scale << 1.
        let mut l2 = [[0i32; 8]; 8];
        l2[2][2] = 3; // coef8[0][2][2] = 32
        dequant_8x8(&mut l2, 6, &FLAT_WEIGHT_8X8);
        assert_eq!(l2[2][2], (((3i64 * 32 * 16) << 1) + 32) as i32 >> 6);
    }

    #[test]
    fn dequant_8x8_with_scaling_list_matches_jm_mb0() {
        // MB 0 of the real stream: level −3823 at scan pos 0 (DC), qp 0,
        // with the stream's 8×8 intra scaling weight 6 at DC (not flat 16).
        // JM's dequantised DC was −7168; the residual (inverse spreads DC
        // uniformly, then (H+32)>>6) is −112, so pred 128 → pixel 16.
        let mut weight = [[16i32; 8]; 8];
        weight[0][0] = 6;
        let mut levels = [[0i32; 8]; 8];
        levels[0][0] = -3823;
        dequant_8x8(&mut levels, 0, &weight);
        assert_eq!(levels[0][0], -7168);

        let mut lv = [[0i32; 8]; 8];
        lv[0][0] = -3823;
        let r = reconstruct_residual_8x8(&lv, 0, &weight);
        assert_eq!(r[0][0], -112);
        // Pred (DC, no neighbours) = 128; reconstructed pixel = clip(128−112).
        assert_eq!((128 + r[0][0]).clamp(0, 255), 16);
    }

    #[test]
    fn reconstruct_residual_8x8_dc_spreads_uniformly() {
        // DC-only level -> dequant DC, inverse spreads it flat, final
        // (H+32)>>6 applied uniformly.
        let mut levels = [[0i32; 8]; 8];
        levels[0][0] = 12;
        let r = reconstruct_residual_8x8(&levels, 18, &FLAT_WEIGHT_8X8);
        let expected = r[0][0];
        for row in &r {
            for &v in row {
                assert_eq!(v, expected);
            }
        }
        assert!(expected != 0);
    }

    #[test]
    fn inverse_scan_8x8_places_diagonal() {
        // Scan positions 0,1,2 land at (0,0),(0,1),(1,0) per SNGL_SCAN8x8.
        let mut scan = [0i32; 64];
        scan[0] = 1;
        scan[1] = 2;
        scan[2] = 3;
        scan[63] = 9;
        let b = inverse_scan_8x8(&scan);
        assert_eq!(b[0][0], 1);
        assert_eq!(b[0][1], 2);
        assert_eq!(b[1][0], 3);
        assert_eq!(b[7][7], 9);
    }
}
