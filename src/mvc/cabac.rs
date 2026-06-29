// CABAC arithmetic decoding engine (ITU-T H.264 § 9.3.3.2).
//
// First building block of the libmvc decode core (docs/libmvc-poc.md):
// the context-adaptive binary arithmetic decoder that every High-profile
// (and thus Blu-ray 3D) slice is coded with. This module implements only
// the *engine* — the three decode primitives plus context state — not the
// macroblock syntax that drives it; that lands on top of this next.
//
//   - DecodeDecision  (§ 9.3.3.2.1): one context-coded bin.
//   - DecodeBypass    (§ 9.3.3.2.2): one equiprobable bin (no context).
//   - DecodeTerminate (§ 9.3.3.2.3): end_of_slice_flag / I_PCM escape.
//   - Initialisation  (§ 9.3.1.1/§ 9.3.1.2): context state from (m, n) +
//     SliceQP, engine range/offset.
//
// The normative state-transition tables (rangeTabLPS Table 9-46,
// transIdxLPS/transIdxMPS Table 9-47) are reproduced verbatim. Their
// transcription is validated against the spec only once a real frame
// decodes bit-exact (docs/libmvc-poc.md § Validation); the engine
// *algorithm* is validated here by a round-trip against a reference
// encoder (the standard CABAC coder is symmetric).

/// A single CABAC context model: 6-bit probability-state index and the
/// value of the most-probable symbol. Initialised per slice from the
/// context's (m, n) init pair and the slice QP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CtxState {
    pub pstate: u8, // pStateIdx, 0..=63
    pub mps: u8,    // valMPS, 0 or 1
}

impl CtxState {
    /// Initialise from the context's `(m, n)` init values and the slice
    /// QP (§ 9.3.1.1, eq. 9-5..9-7).
    pub fn init(m: i32, n: i32, slice_qp: i32) -> Self {
        let pre = (((m * slice_qp.clamp(0, 51)) >> 4) + n).clamp(1, 126);
        if pre <= 63 {
            CtxState { pstate: (63 - pre) as u8, mps: 0 }
        } else {
            CtxState { pstate: (pre - 64) as u8, mps: 1 }
        }
    }
}

/// CABAC arithmetic decoder over the byte-aligned slice data. The caller
/// passes the slice-data bytes starting at the `cabac_alignment_one_bit`
/// boundary; `new` performs the engine initialisation (§ 9.3.1.2).
pub struct CabacEngine<'a> {
    data: &'a [u8],
    bit_pos: usize, // absolute bit index, MSB-first within data
    range: u32,     // codIRange
    offset: u32,    // codIOffset
}

impl<'a> CabacEngine<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        let mut e = CabacEngine { data, bit_pos: 0, range: 510, offset: 0 };
        e.offset = e.read_bits(9);
        e
    }

    /// Read one bit, MSB-first. Reading past the end yields 0 — the engine
    /// legitimately looks a few bits beyond the last syntax element, and
    /// real bitstreams are padded; the terminate primitive bounds it.
    fn read_bit(&mut self) -> u32 {
        let byte = self.bit_pos >> 3;
        let bit = if byte < self.data.len() {
            ((self.data[byte] >> (7 - (self.bit_pos & 7))) & 1) as u32
        } else {
            0
        };
        self.bit_pos += 1;
        bit
    }

    fn read_bits(&mut self, n: u32) -> u32 {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.read_bit();
        }
        v
    }

    /// Renormalisation (§ 9.3.3.2.2): grow range back to ≥ 256, shifting a
    /// fresh bit into offset each step.
    fn renorm(&mut self) {
        while self.range < 256 {
            self.range <<= 1;
            self.offset = (self.offset << 1) | self.read_bit();
        }
    }

    /// Decode one context-coded bin (§ 9.3.3.2.1), updating `ctx`.
    pub fn decode_decision(&mut self, ctx: &mut CtxState) -> u32 {
        let q = ((self.range >> 6) & 3) as usize;
        let r_lps = RANGE_TAB_LPS[ctx.pstate as usize][q] as u32;
        self.range -= r_lps;
        let bin;
        if self.offset >= self.range {
            // Least-probable symbol.
            bin = (1 - ctx.mps) as u32;
            self.offset -= self.range;
            self.range = r_lps;
            if ctx.pstate == 0 {
                ctx.mps = 1 - ctx.mps;
            }
            ctx.pstate = TRANS_IDX_LPS[ctx.pstate as usize];
        } else {
            // Most-probable symbol.
            bin = ctx.mps as u32;
            ctx.pstate = TRANS_IDX_MPS[ctx.pstate as usize];
        }
        self.renorm();
        bin
    }

    /// Decode one equiprobable (bypass) bin (§ 9.3.3.2.2). No context,
    /// no renormalisation; range is left unchanged.
    pub fn decode_bypass(&mut self) -> u32 {
        self.offset = (self.offset << 1) | self.read_bit();
        if self.offset >= self.range {
            self.offset -= self.range;
            1
        } else {
            0
        }
    }

    /// Decode the terminating bin (§ 9.3.3.2.3). Returns 1 at end-of-slice
    /// (or an I_PCM escape), 0 otherwise — in which case the engine has
    /// renormalised and decoding continues.
    pub fn decode_terminate(&mut self) -> u32 {
        self.range -= 2;
        if self.offset >= self.range {
            1
        } else {
            self.renorm();
            0
        }
    }
}

/// rangeTabLPS — H.264 Table 9-46. Indexed `[pStateIdx][qCodIRangeIdx]`.
#[rustfmt::skip]
static RANGE_TAB_LPS: [[u8; 4]; 64] = [
    [128, 176, 208, 240], [128, 167, 197, 227], [128, 158, 187, 216], [123, 150, 178, 205],
    [116, 142, 169, 195], [111, 135, 160, 185], [105, 128, 152, 175], [100, 122, 144, 166],
    [ 95, 116, 137, 158], [ 90, 110, 130, 150], [ 85, 104, 123, 142], [ 81,  99, 117, 135],
    [ 77,  94, 111, 128], [ 73,  89, 105, 122], [ 69,  85, 100, 116], [ 66,  80,  95, 110],
    [ 62,  76,  90, 104], [ 59,  72,  86,  99], [ 56,  69,  81,  94], [ 53,  65,  77,  89],
    [ 51,  62,  73,  85], [ 48,  59,  69,  80], [ 46,  56,  66,  76], [ 43,  53,  63,  72],
    [ 41,  50,  59,  69], [ 39,  48,  56,  65], [ 37,  45,  54,  62], [ 35,  43,  51,  59],
    [ 33,  41,  48,  56], [ 32,  39,  46,  53], [ 30,  37,  43,  50], [ 28,  35,  41,  48],
    [ 27,  33,  39,  45], [ 26,  31,  37,  43], [ 24,  30,  35,  41], [ 23,  28,  33,  39],
    [ 22,  27,  32,  37], [ 21,  26,  30,  35], [ 20,  24,  29,  33], [ 19,  23,  27,  31],
    [ 18,  22,  26,  30], [ 17,  21,  25,  28], [ 16,  20,  23,  27], [ 15,  19,  22,  25],
    [ 14,  18,  21,  24], [ 14,  17,  20,  23], [ 13,  16,  19,  22], [ 12,  15,  18,  21],
    [ 12,  14,  17,  20], [ 11,  14,  16,  19], [ 11,  13,  15,  18], [ 10,  12,  15,  17],
    [ 10,  12,  14,  16], [  9,  11,  13,  15], [  9,  11,  12,  14], [  8,  10,  12,  14],
    [  8,   9,  11,  13], [  7,   9,  11,  12], [  7,   9,  10,  12], [  7,   8,  10,  11],
    [  6,   8,   9,  11], [  6,   7,   9,  10], [  6,   7,   8,   9], [  2,   2,   2,   2],
];

/// transIdxLPS — H.264 Table 9-47 (LPS state transition).
#[rustfmt::skip]
static TRANS_IDX_LPS: [u8; 64] = [
     0,  0,  1,  2,  2,  4,  4,  5,  6,  7,  8,  9,  9, 11, 11, 12,
    13, 13, 15, 15, 16, 16, 18, 18, 19, 19, 21, 21, 23, 22, 23, 24,
    24, 25, 26, 26, 27, 27, 28, 29, 29, 30, 30, 30, 31, 32, 32, 33,
    33, 33, 34, 34, 35, 35, 35, 36, 36, 36, 37, 37, 37, 38, 38, 63,
];

/// transIdxMPS — H.264 Table 9-47 (MPS state transition).
#[rustfmt::skip]
static TRANS_IDX_MPS: [u8; 64] = [
     1,  2,  3,  4,  5,  6,  7,  8,  9, 10, 11, 12, 13, 14, 15, 16,
    17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32,
    33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48,
    49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 62, 63,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_init_matches_spec_formula() {
        // (m, n) = (20, -15), SliceQP 26:
        //   preCtxState = clip3(1,126, ((20*26)>>4) + -15) = 17  (<= 63)
        //   pStateIdx = 63 - 17 = 46, valMPS = 0
        let c = CtxState::init(20, -15, 26);
        assert_eq!(c, CtxState { pstate: 46, mps: 0 });

        // A high preCtxState lands on the MPS=1 branch:
        //   (m, n) = (20, 30), QP 40: ((20*40)>>4)+30 = 50+30 = 80 (>63)
        //   pStateIdx = 80 - 64 = 16, valMPS = 1
        let c = CtxState::init(20, 30, 40);
        assert_eq!(c, CtxState { pstate: 16, mps: 1 });

        // QP is clamped to [0, 51].
        assert_eq!(CtxState::init(20, -15, 99), CtxState::init(20, -15, 51));
    }

    #[test]
    fn transition_tables_have_expected_shape() {
        // Normative fixed points / monotonicity sanity (catches gross
        // transcription slips; full correctness is the real-frame test).
        assert_eq!(TRANS_IDX_LPS[0], 0);
        assert_eq!(TRANS_IDX_LPS[63], 63);
        assert_eq!(TRANS_IDX_MPS[63], 63);
        assert_eq!(RANGE_TAB_LPS[63], [2, 2, 2, 2]);
        for i in 0..63 {
            assert!(TRANS_IDX_MPS[i] >= i as u8, "MPS transition climbs");
            assert!(TRANS_IDX_LPS[i] <= i as u8, "LPS transition falls or holds");
        }
    }

    // ---- A reference CABAC *encoder* (test-only), per § 9.3.4, used to
    // round-trip against the decoder. The coder is symmetric, so a clean
    // round-trip validates the decode engine's arithmetic.

    #[derive(Clone)]
    enum Op {
        Decision(usize, u8), // context slot, bin
        Bypass(u8),
        Terminate(u8),
    }

    struct RefEncoder {
        low: u32,
        range: u32,
        outstanding: u32,
        first: bool,
        bits: Vec<u8>,
    }

    impl RefEncoder {
        fn new() -> Self {
            RefEncoder { low: 0, range: 510, outstanding: 0, first: true, bits: Vec::new() }
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
        fn encode_decision(&mut self, ctx: &mut CtxState, bin: u8) {
            let q = ((self.range >> 6) & 3) as usize;
            let r_lps = RANGE_TAB_LPS[ctx.pstate as usize][q] as u32;
            self.range -= r_lps;
            if bin != ctx.mps {
                self.low += self.range;
                self.range = r_lps;
                if ctx.pstate == 0 {
                    ctx.mps = 1 - ctx.mps;
                }
                ctx.pstate = TRANS_IDX_LPS[ctx.pstate as usize];
            } else {
                ctx.pstate = TRANS_IDX_MPS[ctx.pstate as usize];
            }
            self.renorm();
        }
        fn encode_bypass(&mut self, bin: u8) {
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
        fn encode_terminate(&mut self, bin: u8) {
            self.range -= 2;
            if bin != 0 {
                self.low += self.range;
                self.flush();
            } else {
                self.renorm();
            }
        }
        fn flush(&mut self) {
            self.range = 2;
            self.renorm();
            self.put_bit(((self.low >> 9) & 1) as u8);
            let v = ((self.low >> 7) & 3) | 1;
            self.bits.push(((v >> 1) & 1) as u8);
            self.bits.push((v & 1) as u8);
        }
        fn into_bytes(mut self) -> Vec<u8> {
            while self.bits.len() % 8 != 0 {
                self.bits.push(0);
            }
            self.bits.chunks(8).map(|c| c.iter().fold(0u8, |a, &b| (a << 1) | b)).collect()
        }
    }

    /// Encode `ops` with `n_ctx` fresh contexts (all the same init), then
    /// decode and assert every produced bin matches. A real CABAC stream
    /// always ends with end_of_slice_flag = 1 (a terminate, which flushes
    /// the encoder), so the helper appends one when the caller hasn't.
    fn round_trip(ops_in: &[Op], n_ctx: usize, ctx_init: CtxState) {
        let mut ops: Vec<Op> = ops_in.iter().map(|o| o.clone()).collect();
        if !matches!(ops.last(), Some(Op::Terminate(_))) {
            ops.push(Op::Terminate(1));
        }
        let ops = &ops[..];
        let mut enc = RefEncoder::new();
        let mut enc_ctx = vec![ctx_init; n_ctx];
        for op in ops {
            match *op {
                Op::Decision(c, b) => enc.encode_decision(&mut enc_ctx[c], b),
                Op::Bypass(b) => enc.encode_bypass(b),
                Op::Terminate(b) => enc.encode_terminate(b),
            }
        }
        let bytes = enc.into_bytes();

        let mut dec = CabacEngine::new(&bytes);
        let mut dec_ctx = vec![ctx_init; n_ctx];
        for (i, op) in ops.iter().enumerate() {
            match *op {
                Op::Decision(c, b) => {
                    assert_eq!(dec.decode_decision(&mut dec_ctx[c]), b as u32, "decision @ {i}")
                }
                Op::Bypass(b) => assert_eq!(dec.decode_bypass(), b as u32, "bypass @ {i}"),
                Op::Terminate(b) => {
                    assert_eq!(dec.decode_terminate(), b as u32, "terminate @ {i}")
                }
            }
        }
    }

    #[test]
    fn round_trip_decisions() {
        let bins = [1u8, 0, 0, 1, 1, 1, 0, 1, 0, 0, 0, 1, 1, 0, 1, 0, 0, 1, 1, 1];
        let ops: Vec<Op> = bins.iter().map(|&b| Op::Decision(0, b)).collect();
        round_trip(&ops, 1, CtxState::init(20, -15, 26));
    }

    #[test]
    fn round_trip_multi_context() {
        // Interleave three contexts so state evolves independently.
        let mut ops = Vec::new();
        let pat = [(0u8, 1u8), (1, 0), (2, 1), (0, 0), (1, 1), (2, 0), (0, 1), (1, 1), (2, 0)];
        for _ in 0..6 {
            for &(c, b) in &pat {
                ops.push(Op::Decision(c as usize, b));
            }
        }
        round_trip(&ops, 3, CtxState::init(0, 41, 28));
    }

    #[test]
    fn round_trip_bypass_and_mixed() {
        let ops = vec![
            Op::Decision(0, 1),
            Op::Bypass(0),
            Op::Bypass(1),
            Op::Bypass(1),
            Op::Decision(0, 0),
            Op::Bypass(0),
            Op::Decision(0, 1),
            Op::Bypass(1),
            Op::Bypass(0),
            Op::Bypass(0),
            Op::Decision(0, 0),
        ];
        round_trip(&ops, 1, CtxState::init(-3, 50, 30));
    }

    #[test]
    fn round_trip_with_terminate() {
        // A realistic shape: a run of decisions, each followed by a
        // not-end-of-slice terminate (0), then the final terminate (1).
        let mut ops = Vec::new();
        for &b in &[1u8, 0, 1, 1, 0, 0, 1, 0, 1, 1] {
            ops.push(Op::Decision(0, b));
            ops.push(Op::Terminate(0));
        }
        ops.push(Op::Terminate(1));
        round_trip(&ops, 1, CtxState::init(20, -15, 26));
    }
}
