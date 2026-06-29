// I-slice macroblock-header CABAC decoding (ITU-T H.264 § 7.3.5 +
// § 9.3): mb_type, transform_size_8x8_flag, intra prediction modes,
// intra_chroma_pred_mode, coded_block_pattern, mb_qp_delta. The first
// consumer of the CABAC engine, and the part whose context derivation
// ties macroblocks to their neighbours.
//
// The decode order, context selection, and neighbour conditions mirror JM
// ldecod exactly (readMB_typeInfo/transform_size/IntraPredMode/CIPredMode/
// CBP/dQuant), and the context-init (m, n) values are transcribed verbatim
// from JM's ctx_tables.h (the INIT_*_I tables) — so the result is checked
// element-by-element against JM's TRACE output (src/mvc/trace.rs). Only
// the intra-only (I_NxN / I_16x16) path is implemented; 4:2:0 chroma.

use super::cabac::{CabacEngine, CtxState};

/// Per-MB state the neighbour context derivation reads back.
#[derive(Debug, Clone, Copy)]
pub struct MbInfo {
    /// I_NxN (I_4x4 or I_8x8). For neighbour mb_type ctx: I_NxN contributes 0.
    pub i_nxn: bool,
    /// luma_transform_size_8x8_flag.
    pub transform8x8: bool,
    /// intra_chroma_pred_mode (c_ipred_mode).
    pub c_ipred: u8,
    /// coded_block_pattern (luma bits 0..3, chroma in 4..5).
    pub cbp: u8,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Neighbors {
    pub left: Option<MbInfo>,
    pub up: Option<MbInfo>,
}

/// CABAC context bank for the I-slice MB header, initialised per slice
/// from the verbatim JM INIT_*_I tables at the slice QP.
pub struct MbHeaderContexts {
    /// mb_type contexts (row 0 of INIT_MB_TYPE_I model 0); indices 0..2 are
    /// the bin-0 neighbour increments, 4..8 the I_16x16 bins (3,9,10 unused).
    mb_type: [CtxState; 11],
    transform: [CtxState; 3],
    ipr: [CtxState; 2],
    cipr: [CtxState; 4],
    cbp: [[CtxState; 4]; 3],
    delta_qp: [CtxState; 4],
}

const CTX_UNUSED: (i32, i32) = (0, 0);

#[rustfmt::skip]
const INIT_MB_TYPE_I: [(i32, i32); 11] = [
    (20,-15), (2,54), (3,74), CTX_UNUSED, (-28,127), (-23,104), (-6,53), (-1,54), (7,51), CTX_UNUSED, CTX_UNUSED,
];
const INIT_TRANSFORM_I: [(i32, i32); 3] = [(31, 21), (31, 31), (25, 50)];
const INIT_IPR_I: [(i32, i32); 2] = [(13, 41), (3, 62)];
const INIT_CIPR_I: [(i32, i32); 4] = [(-9, 83), (4, 86), (0, 97), (-7, 72)];
#[rustfmt::skip]
const INIT_CBP_I: [[(i32, i32); 4]; 3] = [
    [(-17,127), (-13,102), (0,82),  (-7,74)],
    [(-21,107), (-27,127), (-31,127), (-24,127)],
    [(-18,95),  (-27,127), (-21,114), (-30,127)],
];
const INIT_DELTA_QP_I: [(i32, i32); 4] = [(0, 41), (0, 63), (0, 63), (0, 63)];

impl MbHeaderContexts {
    pub fn new(slice_qp: i32) -> Self {
        let mk = |(m, n): (i32, i32)| CtxState::init(m, n, slice_qp);
        MbHeaderContexts {
            mb_type: INIT_MB_TYPE_I.map(mk),
            transform: INIT_TRANSFORM_I.map(mk),
            ipr: INIT_IPR_I.map(mk),
            cipr: INIT_CIPR_I.map(mk),
            cbp: INIT_CBP_I.map(|row| row.map(mk)),
            delta_qp: INIT_DELTA_QP_I.map(mk),
        }
    }
}

/// One emitted syntax element (name, value) — matches JM's trace strings so
/// `trace::first_divergence` can compare directly.
pub type Element = (String, i64);

/// Decode an I-slice macroblock header. Appends the decoded syntax elements
/// (in JM order) to `out`, returns the MB's `MbInfo` for neighbour use.
/// `last_dquant` carries the previous MB's mb_qp_delta (for the dquant
/// context); the caller resets it at slice start.
pub fn decode_mb_header(
    e: &mut CabacEngine,
    ctx: &mut MbHeaderContexts,
    neigh: &Neighbors,
    last_dquant: &mut i32,
    out: &mut Vec<Element>,
) -> MbInfo {
    // --- mb_type (I slice) ---
    let cond = |m: &Option<MbInfo>| m.as_ref().map_or(0, |i| if i.i_nxn { 0 } else { 1 });
    let a = cond(&neigh.left);
    let b = cond(&neigh.up);
    let bin0 = e.decode_decision(&mut ctx.mb_type[(a + b) as usize]);
    let i_nxn = bin0 == 0;
    let (mb_type_value, is_i16) = if i_nxn {
        (0i64, false)
    } else {
        decode_i16_mb_type(e, ctx)
    };
    out.push(("mb_type".into(), mb_type_value));

    let mut info = MbInfo { i_nxn, transform8x8: false, c_ipred: 0, cbp: 0 };

    if i_nxn {
        // --- transform_size_8x8_flag ---
        let a = neigh.left.map_or(0, |i| i.transform8x8 as i32);
        let b = neigh.up.map_or(0, |i| i.transform8x8 as i32);
        let t = e.decode_decision(&mut ctx.transform[(a + b) as usize]);
        info.transform8x8 = t == 1;
        out.push(("transform_size_8x8_flag".into(), t as i64));

        // --- intra prediction modes (4 luma 8x8 blocks, or 16 4x4) ---
        let blocks = if info.transform8x8 { 4 } else { 16 };
        for _ in 0..blocks {
            let prev = e.decode_decision(&mut ctx.ipr[0]);
            let value = if prev == 1 {
                -1
            } else {
                // rem_intra_pred_mode: 3 bins on ipr[1], LSB first.
                let b0 = e.decode_decision(&mut ctx.ipr[1]);
                let b1 = e.decode_decision(&mut ctx.ipr[1]);
                let b2 = e.decode_decision(&mut ctx.ipr[1]);
                (b0 | (b1 << 1) | (b2 << 2)) as i64
            };
            out.push(("intra4x4_pred_mode".into(), value));
        }
    }

    // --- intra_chroma_pred_mode ---
    let a = neigh.left.map_or(0, |i| (i.c_ipred != 0) as i32);
    let b = neigh.up.map_or(0, |i| (i.c_ipred != 0) as i32);
    let bin0 = e.decode_decision(&mut ctx.cipr[(a + b) as usize]);
    let c_ipred = if bin0 == 0 {
        0
    } else {
        // TU(cMax=3): bins 1,2 on cipr[3].
        let mut v = 1;
        if e.decode_decision(&mut ctx.cipr[3]) == 1 {
            v = 2;
            if e.decode_decision(&mut ctx.cipr[3]) == 1 {
                v = 3;
            }
        }
        v
    };
    info.c_ipred = c_ipred as u8;
    out.push(("intra_chroma_pred_mode".into(), c_ipred));

    // --- coded_block_pattern --- (only for I_NxN; I_16x16 derives it from
    // mb_type, not decoded here).
    if !is_i16 {
        let cbp = decode_cbp(e, ctx, neigh);
        info.cbp = cbp as u8;
        out.push(("coded_block_pattern".into(), cbp));
    }

    // --- mb_qp_delta --- only when there are coefficients (cbp != 0) or
    // I_16x16. JM resets last_dquant to 0 when cbp == 0.
    if info.cbp != 0 || is_i16 {
        let dq = decode_dquant(e, ctx, last_dquant);
        out.push(("mb_qp_delta".into(), dq as i64));
    } else {
        *last_dquant = 0;
    }

    info
}

/// I_16x16 mb_type bins (§ readMB_typeInfo_CABAC_i_slice, the act_sym!=0
/// branch). Returns (mb_type value 1..25, is_i16).
fn decode_i16_mb_type(e: &mut CabacEngine, ctx: &mut MbHeaderContexts) -> (i64, bool) {
    if e.decode_terminate() == 1 {
        return (25, false); // I_PCM
    }
    let mut act = 1i64;
    // AC / no-AC (ctx 4): adds 12.
    act += e.decode_decision(&mut ctx.mb_type[4]) as i64 * 12;
    // cbp 0,1,2 (ctx 5, then 6).
    if e.decode_decision(&mut ctx.mb_type[5]) != 0 {
        act += 4;
        if e.decode_decision(&mut ctx.mb_type[6]) != 0 {
            act += 4;
        }
    }
    // I_16x16 pred mode 0..3 (ctx 7 high bit, ctx 8 low bit).
    act += e.decode_decision(&mut ctx.mb_type[7]) as i64 * 2;
    act += e.decode_decision(&mut ctx.mb_type[8]) as i64;
    (act, true)
}

/// coded_block_pattern (§ read_CBP_CABAC), 4:2:0. Mirrors JM's bit-by-bit
/// luma derivation + the two chroma bins.
fn decode_cbp(e: &mut CabacEngine, ctx: &mut MbHeaderContexts, neigh: &Neighbors) -> i64 {
    let up = neigh.up;
    let left = neigh.left;
    let mut cbp: i64 = 0;

    // Luma: four 8x8 blocks at (mb_y, mb_x) in {0,2}×{0,2}.
    for mb_y in (0..4).step_by(2) {
        for mb_x in (0..4).step_by(2) {
            // top contribution b
            let b = if mb_y == 0 {
                up.map_or(0, |u| if (u.cbp as i64 & (1 << (2 + (mb_x >> 1)))) == 0 { 2 } else { 0 })
            } else {
                if (cbp & (1 << (mb_x / 2))) == 0 { 2 } else { 0 }
            };
            // left contribution a
            let a = if mb_x == 0 {
                left.map_or(0, |l| if (l.cbp as i64 & (1 << (2 * (mb_y / 2) + 1))) == 0 { 1 } else { 0 })
            } else {
                if (cbp & (1 << mb_y)) == 0 { 1 } else { 0 }
            };
            let bit = e.decode_decision(&mut ctx.cbp[0][(a + b) as usize]);
            if bit == 1 {
                cbp += 1 << (mb_y + (mb_x >> 1));
            }
        }
    }

    // Chroma (4:2:0): bin0 (any chroma), bin1 (chroma AC).
    let b = up.map_or(0, |u| if u.cbp > 15 { 2 } else { 0 });
    let a = left.map_or(0, |l| if l.cbp > 15 { 1 } else { 0 });
    if e.decode_decision(&mut ctx.cbp[1][(a + b) as usize]) == 1 {
        let b = up.map_or(0, |u| if (u.cbp >> 4) == 2 { 2 } else { 0 });
        let a = left.map_or(0, |l| if (l.cbp >> 4) == 2 { 1 } else { 0 });
        let bit = e.decode_decision(&mut ctx.cbp[2][(a + b) as usize]);
        cbp += if bit == 1 { 32 } else { 16 };
    }
    cbp
}

/// mb_qp_delta (§ read_dQuant_CABAC). Updates `last_dquant`.
fn decode_dquant(e: &mut CabacEngine, ctx: &mut MbHeaderContexts, last_dquant: &mut i32) -> i32 {
    let act_ctx = (*last_dquant != 0) as usize;
    let dquant = if e.decode_decision(&mut ctx.delta_qp[act_ctx]) != 0 {
        // unary_bin_decode(delta_qp+2, ctx_offset=1): first bin on ctx 2;
        // if non-zero, count further 1s on ctx 3 (the JM do/while).
        let unary = if e.decode_decision(&mut ctx.delta_qp[2]) == 0 {
            0u32
        } else {
            let mut s = 0u32;
            loop {
                let l = e.decode_decision(&mut ctx.delta_qp[3]);
                s += 1;
                if l == 0 {
                    break;
                }
            }
            s
        };
        let act_sym = unary + 1; // JM: ++act_sym
        let mut d = ((act_sym + 1) >> 1) as i32;
        if act_sym & 1 == 0 {
            d = -d; // lsb is the sign bit
        }
        d
    } else {
        0
    };
    *last_dquant = dquant;
    dquant
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_init_uses_verbatim_jm_values() {
        // Sanity: at SliceQP 0, mb_type ctx 0 = (20,-15) -> CtxState.
        let ctx = MbHeaderContexts::new(0);
        assert_eq!(ctx.mb_type[0], CtxState::init(20, -15, 0));
        assert_eq!(ctx.transform[2], CtxState::init(25, 50, 0));
        assert_eq!(ctx.cbp[0][0], CtxState::init(-17, 127, 0));
        assert_eq!(ctx.delta_qp[1], CtxState::init(0, 63, 0));
    }
}
