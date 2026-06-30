// Residual CABAC context banks and the per-category descriptors that drive
// decode_residual_block (docs/libmvc-poc.md). The context-init (m, n) values
// are transcribed verbatim from JM's ctx_tables.h I-slice tables
// (INIT_BCBP_I / INIT_MAP_I / INIT_LAST_I / INIT_ONE_I / INIT_ABS_I); the
// position->context maps and the per-block-category parameters (maxpos,
// c1isdc, max_c2, type2ctx_*) come from ldecod cabac.c. Together they let the
// decoder build, for any residual block category, the exact context state JM
// uses — so the per-coefficient output can be diffed against the trace.
//
// Only the 4:2:0 categories an I_8x8 frame actually uses are wired up here
// (luma 8x8, chroma DC, chroma AC); the init tables carry rows 0..7 so the
// remaining luma categories (16DC/16AC/4x4) are a one-line descriptor away.

use super::cabac::CtxState;
use super::residual::CoeffContexts;

// ---- position -> context maps (JM cabac.c) ----

/// significant_coeff_flag context per scan position, 8×8 block (15 ctx).
#[rustfmt::skip]
pub const POS2CTX_MAP8X8: [u8; 64] = [
    0,1,2,3,4,5,5,4,4,3,3,4,4,4,5,5, 4,4,4,4,3,3,6,7,7,7,8,9,10,9,8,7,
    7,6,11,12,13,11,6,7,8,9,14,10,9,8,6,11, 12,13,11,6,9,14,10,9,11,12,13,11,14,10,12,14];
/// last_significant_coeff_flag context per scan position, 8×8 block (9 ctx).
#[rustfmt::skip]
pub const POS2CTX_LAST8X8: [u8; 64] = [
    0,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1, 2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,
    3,3,3,3,3,3,3,3,4,4,4,4,4,4,4,4, 5,5,5,5,6,6,6,6,7,7,7,7,8,8,8,8];
/// 4×4 significance / last map — ctxIdxInc is the scan position (last entry
/// repeats per JM). Chroma DC slices [0..4); chroma AC slices [1..16).
#[rustfmt::skip]
pub const POS2CTX_MAP4X4: [u8; 16] = [0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,14];
#[rustfmt::skip]
pub const POS2CTX_LAST4X4: [u8; 16] = [0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15];

// ---- I-slice context-init tables (ctx_tables.h, model 0, rows 0..7) ----
// Rows are JM block-category-context indices (type2ctx_*). UNUSED entries are
// (0,0) placeholders — never indexed for the categories wired up here.
const U: (i32, i32) = (0, 0);

/// coded_block_flag, 4 contexts/row (INIT_BCBP_I).
const BCBP_I: [[(i32, i32); 4]; 8] = [
    [(-17, 123), (-12, 115), (-16, 122), (-11, 115)],
    [(-12, 63), (-2, 68), (-15, 84), (-13, 104)],
    [(-3, 70), (-8, 93), (-10, 90), (-30, 127)],
    [U, U, U, U],
    [(-3, 70), (-8, 93), (-10, 90), (-30, 127)],
    [(-1, 74), (-6, 97), (-7, 91), (-20, 127)],
    [(-4, 56), (-5, 82), (-7, 76), (-22, 125)],
    [U, U, U, U],
];

/// significant_coeff_flag, 15 contexts/row (INIT_MAP_I).
#[rustfmt::skip]
const MAP_I: [[(i32, i32); 15]; 8] = [
    [(-7,93),(-11,87),(-3,77),(-5,71),(-4,63),(-4,68),(-12,84),(-7,62),(-7,65),(8,61),(5,56),(-2,66),(1,64),(0,61),(-2,78)],
    [U,(1,50),(7,52),(10,35),(0,44),(11,38),(1,45),(0,46),(5,44),(31,17),(1,51),(7,50),(28,19),(16,33),(14,62)],
    [(-17,120),(-20,112),(-18,114),(-11,85),(-15,92),(-14,89),(-26,71),(-15,81),(-14,80),(0,68),(-14,70),(-24,56),(-23,68),(-24,50),(-11,74)],
    [U,U,U,U,U,U,U,U,U,U,U,U,U,U,U],
    [U,U,U,U,U,U,U,U,U,U,U,U,U,U,U],
    [(-13,108),(-15,100),(-13,101),(-13,91),(-12,94),(-10,88),(-16,84),(-10,86),(-7,83),(-13,87),(-19,94),(1,70),(0,72),(-5,74),(18,59)],
    [(-8,102),(-15,100),(0,95),U,U,U,U,U,U,U,U,U,U,U,U],
    [U,(-4,75),(2,72),(-11,75),(-3,71),(15,46),(-13,69),(0,62),(0,65),(21,37),(-15,72),(9,57),(16,54),(0,62),(12,72)],
];

/// last_significant_coeff_flag, 15 contexts/row (INIT_LAST_I).
#[rustfmt::skip]
const LAST_I: [[(i32, i32); 15]; 8] = [
    [(24,0),(15,9),(8,25),(13,18),(15,9),(13,19),(10,37),(12,18),(6,29),(20,33),(15,30),(4,45),(1,58),(0,62),(7,61)],
    [U,(12,38),(11,45),(15,39),(11,42),(13,44),(16,45),(12,41),(10,49),(30,34),(18,42),(10,55),(17,51),(17,46),(0,89)],
    [(23,-13),(26,-13),(40,-15),(49,-14),(44,3),(45,6),(44,34),(33,54),(19,82),U,U,U,U,U,U],
    [U,U,U,U,U,U,U,U,U,U,U,U,U,U,U],
    [U,U,U,U,U,U,U,U,U,U,U,U,U,U,U],
    [(26,-19),(22,-17),(26,-17),(30,-25),(28,-20),(33,-23),(37,-27),(33,-23),(40,-28),(38,-17),(33,-11),(40,-15),(41,-6),(38,1),(41,17)],
    [(30,-6),(27,3),(26,22),U,U,U,U,U,U,U,U,U,U,U,U],
    [U,(37,-16),(35,-4),(38,-8),(38,-3),(37,3),(38,5),(42,0),(35,16),(39,22),(14,48),(27,37),(21,60),(12,68),(2,97)],
];

/// coeff_abs_level_minus1 bin 0 (one_contexts), 5 contexts/row (INIT_ONE_I).
const ONE_I: [[(i32, i32); 5]; 8] = [
    [(-3, 71), (-6, 42), (-5, 50), (-3, 54), (-2, 62)],
    [(-5, 67), (-5, 27), (-3, 39), (-2, 44), (0, 46)],
    [(-3, 75), (-1, 23), (1, 34), (1, 43), (0, 54)],
    [U, U, U, U, U],
    [(-12, 92), (-15, 55), (-10, 60), (-6, 62), (-4, 65)],
    [(-11, 97), (-20, 84), (-11, 79), (-6, 73), (-4, 74)],
    [(-8, 78), (-5, 33), (-4, 48), (-2, 53), (-3, 62)],
    [U, U, U, U, U],
];

/// coeff_abs_level_minus1 bin≥1 (abs_contexts), 5 contexts/row (INIT_ABS_I).
const ABS_I: [[(i32, i32); 5]; 8] = [
    [(0, 58), (1, 63), (-2, 72), (-1, 74), (-9, 91)],
    [(-16, 64), (-8, 68), (-10, 78), (-6, 77), (-10, 86)],
    [(-2, 55), (0, 61), (1, 64), (0, 68), (-9, 92)],
    [U, U, U, U, U],
    [(-12, 73), (-8, 76), (-7, 80), (-9, 88), (-17, 110)],
    [(-13, 86), (-13, 96), (-11, 97), (-19, 117), U],
    [(-13, 71), (-10, 79), (-12, 86), (-13, 90), (-14, 97)],
    [U, U, U, U, U],
];

// ---- P-slice (inter) context-init tables, cabac_init_idc model 0
// (the test stream's model; extracted verbatim from ctx_tables.h INIT_*_P).
#[rustfmt::skip]
const BCBP_P: [[(i32, i32); 4]; 8] = [
    [(-7,92),(-5,89),(-7,96),(-13,108)],
    [(-3,46),(-1,65),(-1,57),(-9,93)],
    [(-3,74),(-9,92),(-8,87),(-23,126)],
    [U,U,U,U],
    [(-3,74),(-9,92),(-8,87),(-23,126)],
    [(5,54),(6,60),(6,59),(6,69)],
    [(-1,48),(0,68),(-4,69),(-8,88)],
    [U,U,U,U],
];
#[rustfmt::skip]
const MAP_P: [[(i32, i32); 15]; 8] = [
    [(-2,85),(-6,78),(-1,75),(-7,77),(2,54),(5,50),(-3,68),(1,50),(6,42),(-4,81),(1,63),(-4,70),(0,67),(2,57),(-2,76)],
    [U,(11,35),(4,64),(1,61),(11,35),(18,25),(12,24),(13,29),(13,36),(-10,93),(-7,73),(-2,73),(13,46),(9,49),(-7,100)],
    [(-4,79),(-7,71),(-5,69),(-9,70),(-8,66),(-10,68),(-19,73),(-12,69),(-16,70),(-15,67),(-20,62),(-19,70),(-16,66),(-22,65),(-20,63)],
    [U,U,U,U,U,U,U,U,U,U,U,U,U,U,U],
    [U,U,U,U,U,U,U,U,U,U,U,U,U,U,U],
    [(9,53),(2,53),(5,53),(-2,61),(0,56),(0,56),(-13,63),(-5,60),(-1,62),(4,57),(-6,69),(4,57),(14,39),(4,51),(13,68)],
    [(3,64),(1,61),(9,63),U,U,U,U,U,U,U,U,U,U,U,U],
    [U,(7,50),(16,39),(5,44),(4,52),(11,48),(-5,60),(-1,59),(0,59),(22,33),(5,44),(14,43),(-1,78),(0,60),(9,69)],
];
#[rustfmt::skip]
const LAST_P: [[(i32, i32); 15]; 8] = [
    [(11,28),(2,40),(3,44),(0,49),(0,46),(2,44),(2,51),(0,47),(4,39),(2,62),(6,46),(0,54),(3,54),(2,58),(4,63)],
    [U,(6,51),(6,57),(7,53),(6,52),(6,55),(11,45),(14,36),(8,53),(-1,82),(7,55),(-3,78),(15,46),(22,31),(-1,84)],
    [(9,-2),(26,-9),(33,-9),(39,-7),(41,-2),(45,3),(49,9),(45,27),(36,59),U,U,U,U,U,U],
    [U,U,U,U,U,U,U,U,U,U,U,U,U,U,U],
    [U,U,U,U,U,U,U,U,U,U,U,U,U,U,U],
    [(25,7),(30,-7),(28,3),(28,4),(32,0),(34,-1),(30,6),(30,6),(32,9),(31,19),(26,27),(26,30),(37,20),(28,34),(17,70)],
    [(1,67),(5,59),(9,67),U,U,U,U,U,U,U,U,U,U,U,U],
    [U,(16,30),(18,32),(18,35),(22,29),(24,31),(23,38),(18,43),(20,41),(11,63),(9,59),(9,64),(-1,94),(-2,89),(-9,108)],
];
#[rustfmt::skip]
const ONE_P: [[(i32, i32); 5]; 8] = [
    [(-6,76),(-2,44),(0,45),(0,52),(-3,64)],
    [(-9,77),(3,24),(0,42),(0,48),(0,55)],
    [(-6,66),(-7,35),(-7,42),(-8,45),(-5,48)],
    [U,U,U,U,U],
    [(1,58),(-3,29),(-1,36),(1,38),(2,43)],
    [(0,70),(-4,29),(5,31),(7,42),(1,59)],
    [(0,58),(8,5),(10,14),(14,18),(13,27)],
    [U,U,U,U,U],
];
#[rustfmt::skip]
const ABS_P: [[(i32, i32); 5]; 8] = [
    [(-2,59),(-4,70),(-4,75),(-8,82),(-17,102)],
    [(-6,59),(-7,71),(-12,83),(-11,87),(-30,119)],
    [(-12,56),(-6,60),(-5,62),(-8,66),(-8,76)],
    [U,U,U,U,U],
    [(-6,55),(0,58),(0,64),(-3,74),(-10,90)],
    [(-2,58),(-3,72),(-3,81),(-11,97),U],
    [(2,40),(0,58),(-3,70),(-6,79),(-8,85)],
    [U,U,U,U,U],
];

/// A residual block category, parameterising the significance/level decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResidualCat {
    /// Intra_16x16 luma DC, the 4×4 Hadamard coeffs (JM LUMA_16DC = type 0).
    Luma16Dc,
    /// Intra_16x16 luma AC (15 AC coeffs at scan positions 1..16; type 1).
    Luma16Ac,
    /// Luma 4×4 transform block (JM LUMA_4x4 = type 5).
    Luma4x4,
    /// Luma 8×8 transform block (JM LUMA_8x8 = type 2). No coded_block_flag —
    /// presence is inferred from the coded_block_pattern.
    Luma8x8,
    /// Chroma DC, 4:2:0 (2×2 → 4 coeffs; JM CHROMA_DC = type 6).
    ChromaDc,
    /// Chroma AC (15 AC coeffs at scan positions 1..16; JM CHROMA_AC = 7).
    ChromaAc,
}

/// Static parameters for decode_residual_block, derived from JM's per-type
/// arrays (maxpos, c1isdc, max_c2) and the pos2ctx_map/last tables.
pub struct CatDesc {
    /// Number of coefficients fed to decode_residual_block.
    pub max_num_coeff: usize,
    /// Position→significance-context map (already sliced/shifted for the
    /// category — chroma AC starts at scan position 1).
    pub pos2ctx_map: &'static [u8],
    /// Position→last-context map (same slicing).
    pub pos2ctx_last: &'static [u8],
    /// numDecodAbsLevelGt1 cap — max_c2[type] (3 for chroma DC, else 4).
    pub gt1_cap: u32,
}

impl ResidualCat {
    /// JM context-row indices (type2ctx_bcbp / _map / _last / _one / _abs)
    /// for this category: (bcbp, map==last, one==abs).
    const fn rows(self) -> (usize, usize, usize) {
        match self {
            // LUMA_16DC = type 0: bcbp 0, map/last 0, one/abs 0.
            ResidualCat::Luma16Dc => (0, 0, 0),
            // LUMA_16AC = type 1: bcbp 1, map/last 1, one/abs 1.
            ResidualCat::Luma16Ac => (1, 1, 1),
            // LUMA_4x4 = type 5: bcbp 4, map/last 5, one/abs 4.
            ResidualCat::Luma4x4 => (4, 5, 4),
            // LUMA_8x8 = type 2: bcbp 2, map/last 2, one/abs 2.
            ResidualCat::Luma8x8 => (2, 2, 2),
            // CHROMA_DC = type 6: bcbp 5, map/last 6, one/abs 5.
            ResidualCat::ChromaDc => (5, 6, 5),
            // CHROMA_AC = type 7: bcbp 6, map/last 7, one/abs 6.
            ResidualCat::ChromaAc => (6, 7, 6),
        }
    }

    pub fn desc(self) -> CatDesc {
        match self {
            // 4×4 DC/luma: full 16 positions, identity 4×4 maps.
            ResidualCat::Luma16Dc | ResidualCat::Luma4x4 => CatDesc {
                max_num_coeff: 16,
                pos2ctx_map: &POS2CTX_MAP4X4,
                pos2ctx_last: &POS2CTX_LAST4X4,
                gt1_cap: 4,
            },
            // 16AC: scan positions 1..16, so shift the 4×4 maps by one.
            ResidualCat::Luma16Ac => CatDesc {
                max_num_coeff: 15,
                pos2ctx_map: &POS2CTX_MAP4X4[1..16],
                pos2ctx_last: &POS2CTX_LAST4X4[1..16],
                gt1_cap: 4,
            },
            ResidualCat::Luma8x8 => CatDesc {
                max_num_coeff: 64,
                pos2ctx_map: &POS2CTX_MAP8X8,
                pos2ctx_last: &POS2CTX_LAST8X8,
                gt1_cap: 4,
            },
            ResidualCat::ChromaDc => CatDesc {
                max_num_coeff: 4,
                pos2ctx_map: &POS2CTX_MAP4X4[..4],
                pos2ctx_last: &POS2CTX_LAST4X4[..4],
                gt1_cap: 3,
            },
            // AC: scan positions 1..16, so shift the 4×4 maps by one.
            ResidualCat::ChromaAc => CatDesc {
                max_num_coeff: 15,
                pos2ctx_map: &POS2CTX_MAP4X4[1..16],
                pos2ctx_last: &POS2CTX_LAST4X4[1..16],
                gt1_cap: 4,
            },
        }
    }

    /// Build the significance/last/level context bank for this category at
    /// `slice_qp`. `inter` selects the P-slice init tables (cabac_init_idc
    /// model 0) instead of the I-slice ones. The bcbp (coded_block_flag)
    /// contexts are built separately via [`bcbp_contexts`].
    pub fn coeff_contexts(self, slice_qp: i32, inter: bool) -> CoeffContexts {
        let mk = |(m, n): (i32, i32)| CtxState::init(m, n, slice_qp);
        let (_, ml_row, oa_row) = self.rows();
        let (map, last, one_t, abs_t): (&[[(i32, i32); 15]; 8], _, &[[(i32, i32); 5]; 8], _) =
            if inter { (&MAP_P, &LAST_P, &ONE_P, &ABS_P) } else { (&MAP_I, &LAST_I, &ONE_I, &ABS_I) };
        let one = one_t[oa_row];
        let abs = abs_t[oa_row];
        CoeffContexts {
            sig: map[ml_row].iter().copied().map(mk).collect(),
            last: last[ml_row].iter().copied().map(mk).collect(),
            level: std::array::from_fn(|i| if i < 5 { mk(one[i]) } else { mk(abs[i - 5]) }),
        }
    }

    /// The 4 coded_block_flag contexts for this category at `slice_qp`
    /// (indexed by 2*upper_bit + left_bit). `inter` selects the P tables.
    pub fn bcbp_contexts(self, slice_qp: i32, inter: bool) -> [CtxState; 4] {
        let bcbp = if inter { &BCBP_P } else { &BCBP_I };
        let (bcbp_row, _, _) = self.rows();
        bcbp[bcbp_row].map(|(m, n)| CtxState::init(m, n, slice_qp))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptors_match_jm_per_type_arrays() {
        // maxpos[2]=63 -> 64 coeffs; maxpos[6]=3 -> 4; chroma AC -> 15.
        assert_eq!(ResidualCat::Luma8x8.desc().max_num_coeff, 64);
        assert_eq!(ResidualCat::ChromaDc.desc().max_num_coeff, 4);
        assert_eq!(ResidualCat::ChromaAc.desc().max_num_coeff, 15);
        // max_c2: chroma DC capped at 3, others 4.
        assert_eq!(ResidualCat::ChromaDc.desc().gt1_cap, 3);
        assert_eq!(ResidualCat::Luma8x8.desc().gt1_cap, 4);
        // Chroma AC significance map is shifted to start at scan position 1.
        assert_eq!(ResidualCat::ChromaAc.desc().pos2ctx_map[0], 1);
        assert_eq!(ResidualCat::ChromaAc.desc().pos2ctx_last[0], 1);
    }

    #[test]
    fn coeff_contexts_built_from_correct_rows() {
        // Luma 8×8 sig context 0 comes from MAP_I row 2 = (-17, 120).
        let c = ResidualCat::Luma8x8.coeff_contexts(26, false);
        assert_eq!(c.sig.len(), 15);
        assert_eq!(c.last.len(), 15);
        // Confirm the init matches a direct CtxState::init from row 2 entry 0.
        assert_eq!(c.sig[0], CtxState::init(-17, 120, 26));
        // last row 2 entry 0 = (23, -13); level one row 2 entry 0 = (-3, 75);
        // abs row 2 entry 0 = (-2, 55) lands at level[5].
        assert_eq!(c.last[0], CtxState::init(23, -13, 26));
        assert_eq!(c.level[0], CtxState::init(-3, 75, 26));
        assert_eq!(c.level[5], CtxState::init(-2, 55, 26));
    }

    #[test]
    fn intra16x16_and_4x4_categories_use_correct_rows() {
        // LUMA_16DC (type 0): map/last row 0 entry 0 = (-7, 93); one row 0
        // entry 0 = (-3, 71); bcbp row 0 entry 0 = (-17, 123).
        let dc = ResidualCat::Luma16Dc.coeff_contexts(26, false);
        assert_eq!(dc.sig[0], CtxState::init(-7, 93, 26));
        assert_eq!(dc.level[0], CtxState::init(-3, 71, 26));
        assert_eq!(ResidualCat::Luma16Dc.bcbp_contexts(26, false)[0], CtxState::init(-17, 123, 26));
        // LUMA_4x4 (type 5): map/last row 5 entry 0 = (-13, 108); one/abs
        // row 4 entry 0 = (-12, 92); bcbp row 4 entry 0 = (-3, 70).
        let l4 = ResidualCat::Luma4x4.coeff_contexts(26, false);
        assert_eq!(l4.sig[0], CtxState::init(-13, 108, 26));
        assert_eq!(l4.level[0], CtxState::init(-12, 92, 26));
        assert_eq!(ResidualCat::Luma4x4.bcbp_contexts(26, false)[0], CtxState::init(-3, 70, 26));
        // 16AC starts at scan position 1.
        assert_eq!(ResidualCat::Luma16Ac.desc().max_num_coeff, 15);
        assert_eq!(ResidualCat::Luma16Ac.desc().pos2ctx_map[0], 1);
    }

    #[test]
    fn chroma_dc_uses_one_abs_row_5_and_bcbp_row_5() {
        // CHROMA_DC: type2ctx_one[6]=5 -> ONE_I row 5 entry 0 = (-11, 97);
        // type2ctx_bcbp[6]=5 -> BCBP_I row 5 entry 0 = (-1, 74).
        let c = ResidualCat::ChromaDc.coeff_contexts(26, false);
        assert_eq!(c.level[0], CtxState::init(-11, 97, 26));
        let b = ResidualCat::ChromaDc.bcbp_contexts(26, false);
        assert_eq!(b[0], CtxState::init(-1, 74, 26));
    }
}
