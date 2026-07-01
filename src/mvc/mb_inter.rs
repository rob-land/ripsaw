// Inter (P/B) macroblock CABAC decoding (§ 7.3.5 + § 9.3). The first piece
// of the inter arc (docs/libmvc-inter.md): mb_skip_flag and the inter
// mb_type, with contexts initialised from JM's INIT_MB_TYPE_P tables for the
// slice's cabac_init_idc model. Motion vectors, sub-MB types, ref indices,
// reference-list construction, the DPB and motion compensation follow.
//
// Context-init (m, n) values are transcribed verbatim from JM ctx_tables.h.

use super::cabac::{CabacEngine, CtxState};

/// `INIT_MB_TYPE_P[model][1]` — the P-slice mb_type/skip context sub-array
/// (11 entries; index 3 is unused). Indices 0..2 are mb_skip_flag (ctxIdxInc
/// = a+b), 4..7 the inter mb_type bins, 8..10 the I_16x16 suffix. One row per
/// cabac_init_idc model (0..2).
#[rustfmt::skip]
const INIT_MB_TYPE_P: [[(i32, i32); 11]; 3] = [
    // model 0
    [(23,33),(23,2),(21,0),(0,0),(1,9),(0,49),(-37,118),(5,57),(-13,78),(-11,65),(1,62)],
    // model 1
    [(22,25),(34,0),(16,0),(0,0),(-2,9),(4,41),(-29,118),(2,65),(-6,71),(-13,79),(5,52)],
    // model 2
    [(29,16),(25,0),(14,0),(0,0),(-10,51),(-3,62),(-27,99),(26,16),(-4,85),(-24,102),(5,57)],
];

/// `INIT_MV_RES_P[model][2][10]` — mvd contexts: `[0]` is the bin-0 context
/// (indexed `5*k + ctxIdxInc`, k = component), `[1]` the UEGk suffix.
#[rustfmt::skip]
const INIT_MV_RES_P: [[[(i32, i32); 10]; 2]; 3] = [
    [[(-3,69),(0,0),(-6,81),(-11,96),(0,0),(0,58),(0,0),(-3,76),(-10,94),(0,0)],
     [(6,55),(7,67),(-5,86),(2,88),(0,0),(5,54),(4,69),(-3,81),(0,88),(0,0)]],
    [[(0,65),(0,0),(-3,79),(-11,97),(0,0),(-1,69),(0,0),(-2,78),(-9,93),(0,0)],
     [(5,64),(11,53),(-4,87),(3,90),(0,0),(2,68),(3,73),(-3,84),(1,92),(0,0)]],
    [[(-1,70),(0,0),(-7,86),(-13,103),(0,0),(0,60),(0,0),(-3,80),(-10,98),(0,0)],
     [(8,55),(11,58),(-7,93),(3,93),(0,0),(7,53),(8,63),(-4,86),(2,95),(0,0)]],
];

/// `INIT_CBP_P[model][3][4]`, `INIT_TRANSFORM_SIZE_P[model][3]`,
/// `INIT_DELTA_QP_P[model][4]` — the inter cbp / transform-flag / mb_qp_delta
/// contexts (same algorithms as intra, different init).
#[rustfmt::skip]
const INIT_CBP_P: [[[(i32, i32); 4]; 3]; 3] = [
    [[(-27,126),(-28,98),(-25,101),(-23,67)],[(-28,82),(-20,94),(-16,83),(-22,110)],[(-21,91),(-18,102),(-13,93),(-29,127)]],
    [[(-39,127),(-18,91),(-17,96),(-26,81)],[(-35,98),(-24,102),(-23,97),(-27,119)],[(-24,99),(-21,110),(-18,102),(-36,127)]],
    [[(-36,127),(-17,91),(-14,95),(-25,84)],[(-25,86),(-12,89),(-17,91),(-31,127)],[(-14,76),(-18,103),(-13,90),(-37,127)]],
];
#[rustfmt::skip]
const INIT_TRANSFORM_SIZE_P: [[(i32, i32); 3]; 3] = [
    [(12,40),(11,51),(14,59)],
    [(25,32),(21,49),(21,54)],
    [(21,33),(19,50),(17,61)],
];
#[rustfmt::skip]
const INIT_DELTA_QP_P: [[(i32, i32); 4]; 3] = [
    [(0,41),(0,63),(0,63),(0,63)],
    [(0,41),(0,63),(0,63),(0,63)],
    [(0,41),(0,63),(0,63),(0,63)],
];
/// `INIT_B8_TYPE_P[model][0][0..5]` — sub_mb_type contexts (P).
#[rustfmt::skip]
const INIT_B8_TYPE_P: [[(i32, i32); 5]; 3] = [
    [(0,0),(12,49),(0,0),(-4,73),(17,50)],
    [(0,0),(9,50),(0,0),(-3,70),(10,54)],
    [(0,0),(6,57),(0,0),(-17,73),(14,57)],
];

/// `INIT_MB_TYPE_B` = `INIT_MB_TYPE[model][2]` (the B mb_type/skip sub-array,
/// `mot_ctx->mb_type_contexts[2]`). Indices: 0..2 = inter-tree first bin
/// (ctxIdxInc a+b), 4..6 = inter-tree bins, 7..9 = mb_skip_flag (ctxIdxInc
/// 7 + a+b). The I_16x16 suffix reuses `mb_type_contexts[1]` (= `mb_type`).
#[rustfmt::skip]
const INIT_MB_TYPE_B: [[(i32, i32); 11]; 3] = [
    [(26,67),(16,90),(9,104),(0,0),(-46,127),(-20,104),(1,67),(18,64),(9,43),(29,0),(0,0)],
    [(57,2),(41,36),(26,69),(0,0),(-45,127),(-15,101),(-4,76),(26,34),(19,22),(40,0),(0,0)],
    [(54,0),(37,42),(12,97),(0,0),(-32,127),(-22,117),(-2,74),(20,40),(20,10),(29,0),(0,0)],
];
/// `INIT_B8_TYPE_B` = `INIT_B8_TYPE_P[model][1][0..4]` — B sub_mb_type ctx.
#[rustfmt::skip]
const INIT_B8_TYPE_B: [[(i32, i32); 4]; 3] = [
    [(-6,86),(-17,95),(-6,61),(9,45)],
    [(6,69),(-13,90),(0,52),(8,43)],
    [(-6,93),(-14,88),(-6,44),(4,55)],
];

/// `INIT_REF_NO_P[model][0]` — ref_idx contexts (6): [0..3] the bin-0 ctx
/// (ctxIdxInc = a + b), [4]/[5] the unary suffix. Same table for P and B.
#[rustfmt::skip]
const INIT_REF_NO_P: [[(i32, i32); 6]; 3] = [
    [(-7,67),(-5,74),(-4,74),(-5,80),(-7,72),(1,58)],
    [(-1,66),(-1,77),(1,70),(-2,86),(-5,72),(0,61)],
    [(3,55),(-4,79),(-2,75),(-12,97),(-7,50),(1,60)],
];

/// CABAC contexts for inter-MB header syntax, built once per slice from the
/// cabac_init_idc model at the slice QP.
pub struct InterContexts {
    /// mb_type/skip sub-array (`mot_ctx->mb_type_contexts[1]`).
    pub mb_type: [CtxState; 11],
    /// mvd contexts: `[0]` bin-0 (10), `[1]` UEGk suffix (10).
    pub mv_res: [[CtxState; 10]; 2],
    /// coded_block_pattern (3 sub-arrays × 4 ctx).
    pub cbp: [[CtxState; 4]; 3],
    /// transform_size_8x8_flag (3 ctx).
    pub transform: [CtxState; 3],
    /// mb_qp_delta (4 ctx).
    pub delta_qp: [CtxState; 4],
    /// sub_mb_type (b8_type_contexts[0], 5 ctx).
    pub b8_type: [CtxState; 5],
    /// B mb_type/skip sub-array (`mot_ctx->mb_type_contexts[2]`, 11 ctx).
    pub mb_type_b: [CtxState; 11],
    /// B sub_mb_type (b8_type_contexts[1], 4 ctx).
    pub b8_type_b: [CtxState; 4],
    /// ref_idx (ref_no_contexts, 6 ctx).
    pub ref_no: [CtxState; 6],
}

impl InterContexts {
    pub fn new(cabac_init_idc: u32, slice_qp: i32) -> Self {
        let m = cabac_init_idc as usize % 3;
        let row = &INIT_MB_TYPE_P[m];
        let mvr = &INIT_MV_RES_P[m];
        let cbp = &INIT_CBP_P[m];
        let ts = &INIT_TRANSFORM_SIZE_P[m];
        let dq = &INIT_DELTA_QP_P[m];
        let b8 = &INIT_B8_TYPE_P[m];
        let mtb = &INIT_MB_TYPE_B[m];
        let b8b = &INIT_B8_TYPE_B[m];
        let mk = |(p, q): (i32, i32)| CtxState::init(p, q, slice_qp);
        InterContexts {
            mb_type: std::array::from_fn(|i| mk(row[i])),
            mv_res: std::array::from_fn(|j| std::array::from_fn(|i| mk(mvr[j][i]))),
            cbp: std::array::from_fn(|j| std::array::from_fn(|i| mk(cbp[j][i]))),
            transform: std::array::from_fn(|i| mk(ts[i])),
            delta_qp: std::array::from_fn(|i| mk(dq[i])),
            b8_type: std::array::from_fn(|i| mk(b8[i])),
            mb_type_b: std::array::from_fn(|i| mk(mtb[i])),
            b8_type_b: std::array::from_fn(|i| mk(b8b[i])),
            ref_no: std::array::from_fn(|i| mk(INIT_REF_NO_P[m][i])),
        }
    }
}

/// Decode a ref_idx_lX (JM readRefFrame_CABAC). `a`/`b` = left/up neighbour
/// ref_idx > 0 (as 1 / 2 for the ctxIdxInc). bin-0 on `ref_no[a+b]`, then a
/// truncated-unary suffix on `ref_no[4]` (first) / `ref_no[5]` (rest).
pub fn decode_ref_idx(e: &mut CabacEngine, ctx: &mut InterContexts, a: u32, b: u32) -> i64 {
    if e.decode_decision(&mut ctx.ref_no[(a + b) as usize]) == 0 {
        return 0;
    }
    if e.decode_decision(&mut ctx.ref_no[4]) == 0 {
        return 1;
    }
    let mut sym = 2;
    while e.decode_decision(&mut ctx.ref_no[5]) == 1 {
        sym += 1;
    }
    sym
}

/// Decode a P-slice sub_mb_type (JM readB8_typeInfo_CABAC_p_slice):
/// 0 = P_L0_8x8, 1 = P_L0_8x4, 2 = P_L0_4x8, 3 = P_L0_4x4.
pub fn decode_sub_mb_type(e: &mut CabacEngine, ctx: &mut InterContexts) -> i64 {
    if e.decode_decision(&mut ctx.b8_type[1]) == 1 {
        0
    } else if e.decode_decision(&mut ctx.b8_type[3]) == 1 {
        if e.decode_decision(&mut ctx.b8_type[4]) == 1 {
            2
        } else {
            3
        }
    } else {
        1
    }
}

/// Decode a B-slice mb_skip_flag (JM read_skip_flag_CABAC_b_slice). Context
/// `mb_type_b[7 + a + b]`, a/b = left/up MB not-skipped. Traced value is
/// `(bin != 1)`: 0 = skipped (B_Skip), 1 = coded. Returns (is_skip, value1).
pub fn decode_mb_skip_flag_b(e: &mut CabacEngine, ctx: &mut InterContexts, left_not_skip: u32, up_not_skip: u32) -> (bool, i64) {
    let bin = e.decode_decision(&mut ctx.mb_type_b[(7 + left_not_skip + up_not_skip) as usize]);
    let value1 = (bin != 1) as i64;
    (value1 == 0, value1)
}

/// Decode a B-slice mb_type (JM readMB_typeInfo_CABAC_b_slice). Returns the
/// traced act_sym / curr_mb_type: 0 = B_Direct_16x16, 1..3 = B_{L0,L1,Bi}_16x16,
/// 4..21 = the 16x8/8x16 bi-pred modes, 22 = B_8x8, 23 = I_NxN, 24..47 =
/// I_16x16 expanded, 48 = I_PCM. `a`/`b` = left/up MB not-direct (mb_type != 0).
pub fn decode_b_mb_type(e: &mut CabacEngine, ctx: &mut InterContexts, a: u32, b: u32) -> i64 {
    let mut act_sym: i64;
    {
        let mt = &mut ctx.mb_type_b;
        if e.decode_decision(&mut mt[(a + b) as usize]) != 0 {
            if e.decode_decision(&mut mt[4]) != 0 {
                if e.decode_decision(&mut mt[5]) != 0 {
                    act_sym = 12;
                    if e.decode_decision(&mut mt[6]) != 0 { act_sym += 8; }
                    if e.decode_decision(&mut mt[6]) != 0 { act_sym += 4; }
                    if e.decode_decision(&mut mt[6]) != 0 { act_sym += 2; }
                    if act_sym == 24 {
                        act_sym = 11;
                    } else if act_sym == 26 {
                        act_sym = 22;
                    } else {
                        if act_sym == 22 { act_sym = 23; }
                        if e.decode_decision(&mut mt[6]) != 0 { act_sym += 1; }
                    }
                } else {
                    act_sym = 3;
                    if e.decode_decision(&mut mt[6]) != 0 { act_sym += 4; }
                    if e.decode_decision(&mut mt[6]) != 0 { act_sym += 2; }
                    if e.decode_decision(&mut mt[6]) != 0 { act_sym += 1; }
                }
            } else if e.decode_decision(&mut mt[6]) != 0 {
                act_sym = 2;
            } else {
                act_sym = 1;
            }
        } else {
            act_sym = 0;
        }
    }
    if act_sym <= 23 {
        return act_sym;
    }
    // I_16x16 suffix (act_sym 24): I_PCM final bin, else AC/cbp/pred bins on
    // mb_type_contexts[1] (= mb_type) indices 8,9,10.
    if e.decode_terminate() == 1 {
        return 48;
    }
    let p = &mut ctx.mb_type;
    if e.decode_decision(&mut p[8]) != 0 { act_sym += 12; }
    if e.decode_decision(&mut p[9]) != 0 {
        act_sym += 4;
        if e.decode_decision(&mut p[9]) != 0 { act_sym += 4; }
    }
    if e.decode_decision(&mut p[10]) != 0 { act_sym += 2; }
    if e.decode_decision(&mut p[10]) != 0 { act_sym += 1; }
    act_sym
}

/// Decode a B-slice sub_mb_type (JM readB8_typeInfo_CABAC_b_slice) →
/// 0 = B_Direct_8x8, 1 = B_L0_8x8, 2 = B_L1_8x8, 3 = B_Bi_8x8, 4 = B_L0_8x4,
/// 5 = B_L0_4x8, 6 = B_L1_8x4, 7 = B_L1_4x8, 8 = B_Bi_8x4, 9 = B_Bi_4x8,
/// 10 = B_L0_4x4, 11 = B_L1_4x4, 12 = B_Bi_4x4.
pub fn decode_b_sub_mb_type(e: &mut CabacEngine, ctx: &mut InterContexts) -> i64 {
    let b8 = &mut ctx.b8_type_b;
    if e.decode_decision(&mut b8[0]) == 0 {
        return 0;
    }
    if e.decode_decision(&mut b8[1]) == 0 {
        return if e.decode_decision(&mut b8[3]) != 0 { 2 } else { 1 };
    }
    let mut act_sym: i64;
    if e.decode_decision(&mut b8[2]) != 0 {
        if e.decode_decision(&mut b8[3]) != 0 {
            act_sym = 10;
            if e.decode_decision(&mut b8[3]) != 0 { act_sym += 1; }
        } else {
            act_sym = 6;
            if e.decode_decision(&mut b8[3]) != 0 { act_sym += 2; }
            if e.decode_decision(&mut b8[3]) != 0 { act_sym += 1; }
        }
    } else {
        act_sym = 2;
        if e.decode_decision(&mut b8[3]) != 0 { act_sym += 2; }
        if e.decode_decision(&mut b8[3]) != 0 { act_sym += 1; }
    }
    act_sym + 1
}

/// B-slice mb_type → (partition shape, per-partition prediction direction).
/// Shape: `(num_parts, part_w4, part_h4)` in 4×4 units. pdir: 0 = L0, 1 = L1,
/// 2 = Bi. From JM `interpret_mb_mode_B`. Only inter modes 0..22 (no intra).
pub fn interpret_b_mb_type(mb_type: i64) -> (usize, usize, usize, [u8; 4]) {
    const PDIR16X16: [u8; 4] = [0, 0, 1, 2]; // index by mb_type (0..3)
    #[rustfmt::skip]
    const PDIR16X8: [[u8; 2]; 22] = [[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[1,1],[0,0],[0,1],[0,0],[1,0],
                                     [0,0],[0,2],[0,0],[1,2],[0,0],[2,0],[0,0],[2,1],[0,0],[2,2],[0,0]];
    #[rustfmt::skip]
    const PDIR8X16: [[u8; 2]; 22] = [[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[1,1],[0,0],[0,1],[0,0],
                                     [1,0],[0,0],[0,2],[0,0],[1,2],[0,0],[2,0],[0,0],[2,1],[0,0],[2,2]];
    match mb_type {
        0 => (1, 4, 4, [2, 2, 2, 2]),        // B_Direct_16x16 (whole-MB direct)
        1..=3 => (1, 4, 4, [PDIR16X16[mb_type as usize]; 4]),
        22 => (4, 2, 2, [0, 0, 0, 0]),       // B_8x8 (pdir per sub_mb_type)
        _ if mb_type & 1 == 0 => {           // even → 16x8
            let p = PDIR16X8[mb_type as usize];
            (2, 4, 2, [p[0], p[1], 0, 0])
        }
        _ => {                               // odd → 8x16
            let p = PDIR8X16[mb_type as usize];
            (2, 2, 4, [p[0], p[1], 0, 0])
        }
    }
}

/// Decode the inter mb_type (JM read_MB_typeInfo_CABAC_p_slice). Returns the
/// traced mb_type value: 1=P_16x16, 2=P_16x8, 3=P_8x16, 4=P_8x8, ≥6 = intra
/// (6 = I_NxN; 7.. = I_16x16 expanded). `act_sym` here matches the trace.
pub fn decode_inter_mb_type(e: &mut CabacEngine, ctx: &mut InterContexts) -> i64 {
    let c = &mut ctx.mb_type;
    let mut act_sym = if e.decode_decision(&mut c[4]) == 1 {
        if e.decode_decision(&mut c[7]) == 1 { 7 } else { 6 }
    } else if e.decode_decision(&mut c[5]) == 1 {
        if e.decode_decision(&mut c[7]) == 1 { 2 } else { 3 }
    } else if e.decode_decision(&mut c[6]) == 1 {
        4
    } else {
        1
    };
    if act_sym == 7 {
        // I_16x16 in a P slice: terminate bin = I_PCM, else AC/cbp/pred bins.
        if e.decode_terminate() == 1 {
            return 31; // I_PCM (P-slice numbering)
        }
        let ac = e.decode_decision(&mut c[8]);
        act_sym += (ac as i64) * 12;
        if e.decode_decision(&mut c[9]) != 0 {
            act_sym += 4;
            if e.decode_decision(&mut c[9]) != 0 {
                act_sym += 4;
            }
        }
        act_sym += (e.decode_decision(&mut c[10]) as i64) * 2;
        act_sym += e.decode_decision(&mut c[10]) as i64;
    }
    act_sym
}

/// Truncated-unary + EGk (UEG3) suffix for a mvd magnitude, over the 4
/// contexts `ctx[0..4]` (JM unary_exp_golomb_mv_decode, exp_start = 8).
fn unary_exp_golomb_mv(e: &mut CabacEngine, ctx: &mut [CtxState], max_bin: usize) -> u32 {
    if e.decode_decision(&mut ctx[0]) == 0 {
        return 0;
    }
    let mut symbol = 0u32;
    let mut bin = 1usize;
    let mut idx = 1usize;
    let mut k = 1u32;
    loop {
        let l = e.decode_decision(&mut ctx[idx]);
        bin += 1;
        if bin == 2 {
            idx += 1;
        }
        if bin == max_bin {
            idx += 1;
        }
        symbol += 1;
        k += 1;
        if l == 0 || k == 8 {
            if l != 0 {
                symbol += e.decode_exp_golomb_bypass(3) + 1;
            }
            return symbol;
        }
    }
}

/// Decode one mvd component (JM read_mvd_CABAC). `k` = 0 (x) / 1 (y);
/// `inc` is the ctxIdxInc from the neighbour-|mvd| sum (0 if absSum<3,
/// 3 if >32, else 2).
pub fn decode_mvd_component(e: &mut CabacEngine, ctx: &mut InterContexts, k: usize, inc: usize) -> i64 {
    if e.decode_decision(&mut ctx.mv_res[0][5 * k + inc]) == 0 {
        return 0;
    }
    // Suffix over mv_res[1][5*k .. 5*k+4].
    let base = 5 * k;
    let mag = unary_exp_golomb_mv(e, &mut ctx.mv_res[1][base..base + 4], 3) + 1;
    let sign = e.decode_bypass();
    if sign == 1 { -(mag as i64) } else { mag as i64 }
}

/// ctxIdxInc for a mvd component from the neighbour |mvd| sum.
pub fn mvd_ctx_inc(abs_sum: i32) -> usize {
    if abs_sum < 3 {
        0
    } else if abs_sum > 32 {
        3
    } else {
        2
    }
}

/// Decode `mb_skip_flag` for a P-slice MB (JM read_skip_flag_CABAC_p_slice).
/// `left_not_skip` / `up_not_skip` are 1 when that neighbour exists and was
/// *not* skipped (0 otherwise) — the ctxIdxInc = their sum. Returns
/// `is_skip`: true when the MB is a P_Skip.
///
/// Note the JM inversion: the decoded bin is the *skip* indicator, but the
/// traced value is `value1 = (bin != 1)` — i.e. trace `0` ⇒ skipped,
/// `1` ⇒ coded.
pub fn decode_mb_skip_flag(e: &mut CabacEngine, ctx: &mut InterContexts, left_not_skip: u32, up_not_skip: u32) -> (bool, i64) {
    let bin = e.decode_decision(&mut ctx.mb_type[(left_not_skip + up_not_skip) as usize]);
    let value1 = (bin != 1) as i64; // traced value
    let is_skip = value1 == 0;
    (is_skip, value1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skip_contexts_from_model0() {
        // mb_skip_flag ctx 0 (no neighbours) = INIT_MB_TYPE_P[0][1][0] =
        // (23, 33); the inter mb_type bin0 ctx = index 4 = (1, 9).
        let c = InterContexts::new(0, 26);
        assert_eq!(c.mb_type[0], CtxState::init(23, 33, 26));
        assert_eq!(c.mb_type[4], CtxState::init(1, 9, 26));
    }
}
