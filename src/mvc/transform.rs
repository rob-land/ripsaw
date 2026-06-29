// Inverse scaling (dequantisation) and inverse transforms
// (ITU-T H.264 § 8.5). Second building block of the libmvc decode core
// (docs/libmvc-poc.md): once CABAC has produced coefficient *levels*,
// these turn them into spatial residual samples.
//
// Scope here is the 4×4 residual path used by I_NxN luma + chroma AC, plus
// the DC transforms for Intra_16x16 luma (4×4 Hadamard) and 4:2:0 chroma
// (2×2 Hadamard). 8×8 lands in a follow-up. Custom scaling lists are not
// handled — flat weightScale (=16) only, which is what asserts elsewhere
// bail on; Blu-ray streams in the corpus use flat scaling.

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
