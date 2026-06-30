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

/// CABAC contexts for inter-MB header syntax, built once per slice from the
/// cabac_init_idc model at the slice QP.
pub struct InterContexts {
    /// mb_type/skip sub-array (`mot_ctx->mb_type_contexts[1]`).
    pub mb_type: [CtxState; 11],
}

impl InterContexts {
    pub fn new(cabac_init_idc: u32, slice_qp: i32) -> Self {
        let row = &INIT_MB_TYPE_P[cabac_init_idc as usize % 3];
        InterContexts {
            mb_type: std::array::from_fn(|i| CtxState::init(row[i].0, row[i].1, slice_qp)),
        }
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
