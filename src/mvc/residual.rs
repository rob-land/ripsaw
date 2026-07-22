// Residual block decoding for CABAC (ITU-T H.264 § 7.3.5.3.3 +
// § 9.3.3.1.1.9). Fifth building block of the libmvc decode core
// (docs/libmvc-poc.md): reads a transform block's coefficient *levels*
// from the CABAC engine — the significance map, the last-coefficient
// flag, and the run-context-adaptive absolute-level + sign decoding.
//
// The *logic* here (scan loop, significance map, level binarisation, and
// the numGt1/numEq1 context-derivation) is validated by a round-trip
// against a matching reference encoder, exactly like the CABAC engine.
// The context-model init values (m, n) live with the MB layer and are
// confirmed by the real-frame diff; the contexts are passed in, so this
// module's correctness is independent of those values.

use super::cabac::{CabacEngine, CtxState};

/// Context models for one coefficient-block category. `sig` and `last`
/// hold the significant_coeff_flag / last_significant_coeff_flag contexts;
/// they are indexed *through* a position→context map (§ 9.3.3.1.3): for
/// 4×4 blocks that map is the identity, but for 8×8 it aliases the 63 scan
/// positions onto 15 significance / 9 last contexts (JM `pos2ctx_map8x8` /
/// `pos2ctx_last8x8`). `level` holds the 10 coeff_abs_level_minus1 contexts
/// (0..=4 = one_contexts for bin 0, 5..=9 = abs_contexts for the bin≥1
/// prefix), matching JM's `read_significant_coefficients` c1/c2 model.
pub struct CoeffContexts {
    pub sig: Vec<CtxState>,
    pub last: Vec<CtxState>,
    pub level: [CtxState; 10],
}

/// Identity position→context map for the 4×4 categories, where
/// ctxIdxInc *is* the scan position (JM `pos2ctx_map4x4` /
/// `pos2ctx_last4x4` for the first 15 entries).
pub const POS2CTX_IDENTITY_4X4: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];

/// Decode one residual block of up to `max_num_coeff` coefficients,
/// returning them in **scan order** (the caller applies the inverse scan
/// to raster). Called only when the block is known to have coefficients
/// (coded_block_flag = 1).
///
/// `pos2ctx_map` / `pos2ctx_last` map each scan position to its
/// significance / last-flag context index (JM `pos2ctx_map[type]` /
/// `pos2ctx_last[type]`); both must have at least `max_num_coeff` entries.
/// `cat_gt1_cap` is the cap on `numDecodAbsLevelGt1` for the bin≥1 level
/// context — 4 for most categories, 3 for chroma DC.
/// Fixed-capacity scan-order coefficient buffer (≤ 64 = an 8×8 block). Returned
/// by value from [`decode_residual_block`] to avoid a heap allocation per coded
/// block on the hot residual path; derefs to the `len` (= `max_num_coeff`,
/// zero-padded) coefficient slice so callers use it exactly like the old `Vec`.
pub struct Coeffs {
    data: [i32; 64],
    len: usize,
}
impl std::ops::Deref for Coeffs {
    type Target = [i32];
    #[inline]
    fn deref(&self) -> &[i32] {
        &self.data[..self.len]
    }
}

pub fn decode_residual_block(
    e: &mut CabacEngine,
    ctx: &mut CoeffContexts,
    max_num_coeff: usize,
    pos2ctx_map: &[u8],
    pos2ctx_last: &[u8],
    cat_gt1_cap: u32,
) -> Coeffs {
    // Significance map — at most 64 coefficients (8×8 block), so a stack array
    // avoids a heap alloc per coded block on the hot residual path.
    let mut sig = [false; 64];
    let sig = &mut sig[..max_num_coeff];
    let mut num_coeff = max_num_coeff;

    // Significance map (§ 7.3.5.3.3). The last scanned coefficient is
    // significant by definition, so the loop stops one short of it.
    let mut i = 0;
    while i < num_coeff - 1 {
        if e.decode_decision(&mut ctx.sig[pos2ctx_map[i] as usize]) == 1 {
            sig[i] = true;
            if e.decode_decision(&mut ctx.last[pos2ctx_last[i] as usize]) == 1 {
                num_coeff = i + 1; // remaining positions are zero
                break;
            }
        }
        i += 1;
    }
    sig[num_coeff - 1] = true;

    // Levels, decoded in *reverse* scan order (§ 9.3.3.1.1.9). The context
    // for the first abs-level bin depends on how many ±1 levels have been
    // seen so far; subsequent bins on how many >1 levels.
    let mut coeffs = [0i32; 64];
    let mut num_eq1 = 0u32;
    let mut num_gt1 = 0u32;
    for pos in (0..num_coeff).rev() {
        if !sig[pos] {
            continue;
        }
        let abs_level_m1 = decode_abs_level_minus1(e, ctx, num_eq1, num_gt1, cat_gt1_cap);
        let abs_level = abs_level_m1 + 1;
        if abs_level == 1 {
            num_eq1 += 1;
        } else {
            num_gt1 += 1;
        }
        let sign = e.decode_bypass();
        coeffs[pos] = if sign == 1 { -(abs_level as i32) } else { abs_level as i32 };
    }
    Coeffs { data: coeffs, len: max_num_coeff }
}

/// coeff_abs_level_minus1: UEG0 binarisation (§ 9.3.2.3) — a truncated
/// unary prefix (cMax = 14) over the run-adaptive level contexts, plus an
/// EG0 bypass suffix when the prefix saturates.
fn decode_abs_level_minus1(
    e: &mut CabacEngine,
    ctx: &mut CoeffContexts,
    num_eq1: u32,
    num_gt1: u32,
    cat_gt1_cap: u32,
) -> u32 {
    const PREFIX_CMAX: u32 = 14;
    let ctx0 = if num_gt1 != 0 { 0 } else { (1 + num_eq1).min(4) } as usize;
    let ctx1 = (5 + num_gt1.min(cat_gt1_cap)) as usize;

    // Truncated-unary prefix. Bin 0 = 0 means the level is 1 (m1 = 0).
    if e.decode_decision(&mut ctx.level[ctx0]) == 0 {
        return 0;
    }
    let mut prefix = 1u32;
    while prefix < PREFIX_CMAX {
        if e.decode_decision(&mut ctx.level[ctx1]) == 0 {
            return prefix;
        }
        prefix += 1;
    }
    // Saturated prefix -> EG0 suffix.
    PREFIX_CMAX + e.decode_exp_golomb_bypass(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Reference encoder mirroring decode_residual_block, sharing the CABAC
    // reference encoder from the cabac module's tests is not possible
    // across modules, so a minimal one is reproduced here.
    mod refenc {
        use crate::mvc::cabac::CtxState;

        pub struct Enc {
            low: u32,
            range: u32,
            outstanding: u32,
            first: bool,
            pub bits: Vec<u8>,
        }
        // The transition tables are private to cabac; re-derive via the
        // public engine is not possible for encoding, so embed the same
        // normative tables here for the test encoder.
        #[rustfmt::skip]
        static RANGE_TAB_LPS: [[u8; 4]; 64] = [
            [128,176,208,240],[128,167,197,227],[128,158,187,216],[123,150,178,205],
            [116,142,169,195],[111,135,160,185],[105,128,152,175],[100,122,144,166],
            [95,116,137,158],[90,110,130,150],[85,104,123,142],[81,99,117,135],
            [77,94,111,128],[73,89,105,122],[69,85,100,116],[66,80,95,110],
            [62,76,90,104],[59,72,86,99],[56,69,81,94],[53,65,77,89],
            [51,62,73,85],[48,59,69,80],[46,56,66,76],[43,53,63,72],
            [41,50,59,69],[39,48,56,65],[37,45,54,62],[35,43,51,59],
            [33,41,48,56],[32,39,46,53],[30,37,43,50],[29,35,41,48],
            [27,33,39,45],[26,31,37,43],[24,30,35,41],[23,28,33,39],
            [22,27,32,37],[21,26,30,35],[20,24,29,33],[19,23,27,31],
            [18,22,26,30],[17,21,25,28],[16,20,23,27],[15,19,22,25],
            [14,18,21,24],[14,17,20,23],[13,16,19,22],[12,15,18,21],
            [12,14,17,20],[11,14,16,19],[11,13,15,18],[10,12,15,17],
            [10,12,14,16],[9,11,13,15],[9,11,12,14],[8,10,12,14],
            [8,9,11,13],[7,9,11,12],[7,9,10,12],[7,8,10,11],
            [6,8,9,11],[6,7,9,10],[6,7,8,9],[2,2,2,2],
        ];
        #[rustfmt::skip]
        static TRANS_IDX_LPS: [u8;64] = [
            0,0,1,2,2,4,4,5,6,7,8,9,9,11,11,12,13,13,15,15,16,16,18,18,19,19,21,21,22,22,23,24,
            24,25,26,26,27,27,28,29,29,30,30,30,31,32,32,33,33,33,34,34,35,35,35,36,36,36,37,37,37,38,38,63];
        #[rustfmt::skip]
        static TRANS_IDX_MPS: [u8;64] = [
            1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32,
            33,34,35,36,37,38,39,40,41,42,43,44,45,46,47,48,49,50,51,52,53,54,55,56,57,58,59,60,61,62,62,63];

        impl Enc {
            pub fn new() -> Self {
                Enc { low: 0, range: 510, outstanding: 0, first: true, bits: Vec::new() }
            }
            fn put_bit(&mut self, b: u8) {
                if self.first {
                    self.first = false;
                } else {
                    self.bits.push(b);
                }
                while self.outstanding > 0 {
                    self.bits.push(1 - b);
                    self.outstanding -= 1;
                }
            }
            fn renorm(&mut self) {
                while self.range < 256 {
                    if self.low < 256 {
                        self.put_bit(0);
                    } else if self.low >= 512 {
                        self.low -= 512;
                        self.put_bit(1);
                    } else {
                        self.low -= 256;
                        self.outstanding += 1;
                    }
                    self.range <<= 1;
                    self.low <<= 1;
                }
            }
            pub fn decision(&mut self, ctx: &mut CtxState, bin: u8) {
                let q = ((self.range >> 6) & 3) as usize;
                let r = RANGE_TAB_LPS[ctx.pstate as usize][q] as u32;
                self.range -= r;
                if bin != ctx.mps {
                    self.low += self.range;
                    self.range = r;
                    if ctx.pstate == 0 {
                        ctx.mps = 1 - ctx.mps;
                    }
                    ctx.pstate = TRANS_IDX_LPS[ctx.pstate as usize];
                } else {
                    ctx.pstate = TRANS_IDX_MPS[ctx.pstate as usize];
                }
                self.renorm();
            }
            pub fn bypass(&mut self, bin: u8) {
                self.low <<= 1;
                if bin != 0 {
                    self.low += self.range;
                }
                if self.low >= 1024 {
                    self.put_bit(1);
                    self.low -= 1024;
                } else if self.low < 512 {
                    self.put_bit(0);
                } else {
                    self.low -= 512;
                    self.outstanding += 1;
                }
            }
            pub fn exp_golomb(&mut self, mut v: u32, k: u32) {
                let mut kl = k;
                while v >= (1 << kl) {
                    self.bypass(1);
                    v -= 1 << kl;
                    kl += 1;
                }
                self.bypass(0);
                while kl > 0 {
                    kl -= 1;
                    self.bypass(((v >> kl) & 1) as u8);
                }
            }
            pub fn terminate1(&mut self) {
                self.range -= 2;
                self.low += self.range;
                self.range = 2;
                self.renorm();
                self.put_bit(((self.low >> 9) & 1) as u8);
                let v = ((self.low >> 7) & 3) | 1;
                self.bits.push(((v >> 1) & 1) as u8);
                self.bits.push((v & 1) as u8);
            }
            pub fn into_bytes(mut self) -> Vec<u8> {
                while self.bits.len() % 8 != 0 {
                    self.bits.push(0);
                }
                self.bits.chunks(8).map(|c| c.iter().fold(0u8, |a, &b| (a << 1) | b)).collect()
            }
        }
    }

    fn encode_residual(
        enc: &mut refenc::Enc,
        ctx: &mut CoeffContexts,
        coeffs: &[i32],
        map: &[u8],
        last_map: &[u8],
        cap: u32,
    ) {
        let max = coeffs.len();
        let last = coeffs.iter().rposition(|&c| c != 0).expect("at least one coeff");
        // Mirror the decoder: code sig flags over 0..max-1, breaking once
        // the last-significant flag is set. The final position (max-1) is
        // implied, never coded.
        let mut i = 0;
        while i < max - 1 {
            let s = (coeffs[i] != 0) as u8;
            enc.decision(&mut ctx.sig[map[i] as usize], s);
            if s == 1 {
                enc.decision(&mut ctx.last[last_map[i] as usize], (i == last) as u8);
                if i == last {
                    break;
                }
            }
            i += 1;
        }
        let mut num_eq1 = 0u32;
        let mut num_gt1 = 0u32;
        for pos in (0..=last).rev() {
            if coeffs[pos] == 0 {
                continue;
            }
            let abs = coeffs[pos].unsigned_abs();
            let m1 = abs - 1;
            let ctx0 = if num_gt1 != 0 { 0 } else { (1 + num_eq1).min(4) } as usize;
            let ctx1 = (5 + num_gt1.min(cap)) as usize;
            // TU prefix.
            if m1 == 0 {
                enc.decision(&mut ctx.level[ctx0], 0);
            } else {
                enc.decision(&mut ctx.level[ctx0], 1);
                let prefix = m1.min(14);
                for _ in 1..prefix {
                    enc.decision(&mut ctx.level[ctx1], 1);
                }
                if prefix < 14 {
                    enc.decision(&mut ctx.level[ctx1], 0);
                } else {
                    // saturated -> EG0 suffix
                    enc.exp_golomb(m1 - 14, 0);
                }
            }
            if abs == 1 {
                num_eq1 += 1;
            } else {
                num_gt1 += 1;
            }
            enc.bypass((coeffs[pos] < 0) as u8);
        }
    }

    fn fresh_ctx(n_sig: usize, n_last: usize) -> CoeffContexts {
        // Placeholder inits; round-trip only needs encoder/decoder to share
        // them, so use a spread of states.
        let mk = |i: usize| CtxState::init(((i as i32 * 7) % 60) - 20, ((i as i32 * 5) % 40) - 10, 26);
        CoeffContexts {
            sig: (0..n_sig).map(mk).collect(),
            last: (0..n_last).map(|i| mk(i + 3)).collect(),
            level: std::array::from_fn(|i| mk(i + 1)),
        }
    }

    /// Round-trip with an explicit position→context map (8×8 and chroma
    /// categories alias positions onto fewer contexts).
    fn round_trip_mapped(coeffs: &[i32], map: &[u8], last_map: &[u8], cap: u32) {
        let max = coeffs.len();
        let n_sig = map[..max].iter().copied().max().unwrap() as usize + 1;
        let n_last = last_map[..max].iter().copied().max().unwrap() as usize + 1;

        let mut ectx = fresh_ctx(n_sig, n_last);
        let mut enc = refenc::Enc::new();
        encode_residual(&mut enc, &mut ectx, coeffs, map, last_map, cap);
        enc.terminate1();
        let bytes = enc.into_bytes();

        let mut dctx = fresh_ctx(n_sig, n_last);
        let mut dec = CabacEngine::new(&bytes);
        let got = decode_residual_block(&mut dec, &mut dctx, max, map, last_map, cap);
        assert_eq!(&got[..], coeffs, "round-trip mismatch");
        assert_eq!(dec.decode_terminate(), 1);
    }

    /// 4×4 round-trip: identity position→context map.
    fn round_trip(coeffs: &[i32], cap: u32) {
        round_trip_mapped(coeffs, &POS2CTX_IDENTITY_4X4, &POS2CTX_IDENTITY_4X4, cap);
    }

    #[test]
    fn round_trip_single_coeff() {
        let mut c = [0i32; 16];
        c[0] = 1;
        round_trip(&c, 4);
        let mut c = [0i32; 16];
        c[5] = -3;
        round_trip(&c, 4);
    }

    #[test]
    fn round_trip_dense_block() {
        let c: [i32; 16] = [3, -1, 1, 2, -1, 1, 0, -2, 1, 0, 0, -1, 1, 0, 0, 1];
        round_trip(&c, 4);
    }

    #[test]
    fn round_trip_large_levels_use_eg0_suffix() {
        // Levels > 14 exercise the saturated TU prefix + EG0 suffix.
        let mut c = [0i32; 16];
        c[0] = 40;
        c[1] = -17;
        c[2] = 200;
        c[3] = 1;
        round_trip(&c, 4);
    }

    #[test]
    fn round_trip_last_coeff_significant() {
        // Significant at the final position (15): no last-flag is coded,
        // the implied-significance path is taken.
        let mut c = [0i32; 16];
        c[0] = 1;
        c[15] = -1;
        round_trip(&c, 4);
    }

    #[test]
    fn round_trip_chroma_dc_cap3_small_block() {
        // 4:2:0 chroma DC: 4 coefficients, gt1 cap = 3.
        let c = [2i32, -1, 1, -5];
        round_trip(&c, 3);
    }

    #[test]
    fn round_trip_full_run_of_ones() {
        let c = [1i32, -1, 1, -1, 1, -1, 1, -1, 1, -1, 1, -1, 1, -1, 1, -1];
        round_trip(&c, 4);
    }

    // JM pos2ctx_map8x8 / pos2ctx_last8x8: the 64-position significance and
    // last maps for an 8×8 luma block. Verbatim from ldecod cabac.c.
    #[rustfmt::skip]
    const POS2CTX_MAP8X8: [u8; 64] = [
        0,1,2,3,4,5,5,4,4,3,3,4,4,4,5,5, 4,4,4,4,3,3,6,7,7,7,8,9,10,9,8,7,
        7,6,11,12,13,11,6,7,8,9,14,10,9,8,6,11, 12,13,11,6,9,14,10,9,11,12,13,11,14,10,12,14];
    #[rustfmt::skip]
    const POS2CTX_LAST8X8: [u8; 64] = [
        0,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1, 2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,
        3,3,3,3,3,3,3,3,4,4,4,4,4,4,4,4, 5,5,5,5,6,6,6,6,7,7,7,7,8,8,8,8];

    #[test]
    fn round_trip_8x8_aliased_contexts() {
        // An 8×8 luma block (63 coded positions + implied last) whose
        // significance/last contexts are aliased through pos2ctx_map8x8 /
        // pos2ctx_last8x8 (15 sig, 9 last). Exercises that the decoder and
        // encoder agree on the *shared* contexts those positions collapse to.
        let mut c = [0i32; 64];
        c[0] = 5;
        c[1] = -1;
        c[3] = 1;
        c[7] = -2;
        c[12] = 1;
        c[20] = -1; // position 20 -> sig ctx 3, shared with positions 9,10,...
        c[33] = 3; // -> last ctx 3
        c[47] = -1;
        c[63] = 1; // final position, implied-significant
        round_trip_mapped(&c, &POS2CTX_MAP8X8, &POS2CTX_LAST8X8, 4);
    }

    #[test]
    fn round_trip_8x8_long_run() {
        // Sparse 8×8: a single early coefficient then a late one, so the
        // significance loop walks deep into the aliased context region.
        let mut c = [0i32; 64];
        c[2] = -4;
        c[40] = 7;
        round_trip_mapped(&c, &POS2CTX_MAP8X8, &POS2CTX_LAST8X8, 4);
    }
}
