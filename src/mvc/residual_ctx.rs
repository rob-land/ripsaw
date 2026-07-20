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

// ---- P-slice (inter) context-init tables, per cabac_init_idc model
// (0/1/2), extracted verbatim from JM ctx_tables.h INIT_*_P[model][0..8].
// Model 0 alone was transcribed originally; models 1/2 are needed for any
// P/B slice coded with cabac_init_idc != 0.
#[rustfmt::skip]
const BCBP_P: [[[(i32, i32); 4]; 8]; 3] = [
    // cabac_init_idc model 0
    [
        [(-7,92),(-5,89),(-7,96),(-13,108)],
        [(-3,46),(-1,65),(-1,57),(-9,93)],
        [(-3,74),(-9,92),(-8,87),(-23,126)],
        [U,U,U,U],
        [(-3,74),(-9,92),(-8,87),(-23,126)],
        [(5,54),(6,60),(6,59),(6,69)],
        [(-1,48),(0,68),(-4,69),(-8,88)],
        [U,U,U,U],
    ],
    // cabac_init_idc model 1
    [
        [(0,80),(-5,89),(-7,94),(-4,92)],
        [(0,39),(0,65),(-15,84),(-35,127)],
        [(-2,73),(-12,104),(-9,91),(-31,127)],
        [U,U,U,U],
        [(-2,73),(-12,104),(-9,91),(-31,127)],
        [(3,55),(7,56),(7,55),(8,61)],
        [(-3,53),(0,68),(-7,74),(-9,88)],
        [U,U,U,U],
    ],
    // cabac_init_idc model 2
    [
        [(11,80),(5,76),(2,84),(5,78)],
        [(-6,55),(4,61),(-14,83),(-37,127)],
        [(-5,79),(-11,104),(-11,91),(-30,127)],
        [U,U,U,U],
        [(-5,79),(-11,104),(-11,91),(-30,127)],
        [(0,65),(-2,79),(0,72),(-4,92)],
        [(-6,56),(3,68),(-8,71),(-13,98)],
        [U,U,U,U],
    ],
];
#[rustfmt::skip]
const MAP_P: [[[(i32, i32); 15]; 8]; 3] = [
    // cabac_init_idc model 0
    [
        [(-2,85),(-6,78),(-1,75),(-7,77),(2,54),(5,50),(-3,68),(1,50),(6,42),(-4,81),(1,63),(-4,70),(0,67),(2,57),(-2,76)],
        [U,(11,35),(4,64),(1,61),(11,35),(18,25),(12,24),(13,29),(13,36),(-10,93),(-7,73),(-2,73),(13,46),(9,49),(-7,100)],
        [(-4,79),(-7,71),(-5,69),(-9,70),(-8,66),(-10,68),(-19,73),(-12,69),(-16,70),(-15,67),(-20,62),(-19,70),(-16,66),(-22,65),(-20,63)],
        [U,U,U,U,U,U,U,U,U,U,U,U,U,U,U],
        [U,U,U,U,U,U,U,U,U,U,U,U,U,U,U],
        [(9,53),(2,53),(5,53),(-2,61),(0,56),(0,56),(-13,63),(-5,60),(-1,62),(4,57),(-6,69),(4,57),(14,39),(4,51),(13,68)],
        [(3,64),(1,61),(9,63),U,U,U,U,U,U,U,U,U,U,U,U],
        [U,(7,50),(16,39),(5,44),(4,52),(11,48),(-5,60),(-1,59),(0,59),(22,33),(5,44),(14,43),(-1,78),(0,60),(9,69)],
    ],
    // cabac_init_idc model 1
    [
        [(-13,103),(-13,91),(-9,89),(-14,92),(-8,76),(-12,87),(-23,110),(-24,105),(-10,78),(-20,112),(-17,99),(-78,127),(-70,127),(-50,127),(-46,127)],
        [U,(-4,66),(-5,78),(-4,71),(-8,72),(2,59),(-1,55),(-7,70),(-6,75),(-8,89),(-34,119),(-3,75),(32,20),(30,22),(-44,127)],
        [(-5,85),(-6,81),(-10,77),(-7,81),(-17,80),(-18,73),(-4,74),(-10,83),(-9,71),(-9,67),(-1,61),(-8,66),(-14,66),(0,59),(2,59)],
        [U,U,U,U,U,U,U,U,U,U,U,U,U,U,U],
        [U,U,U,U,U,U,U,U,U,U,U,U,U,U,U],
        [(0,54),(-5,61),(0,58),(-1,60),(-3,61),(-8,67),(-25,84),(-14,74),(-5,65),(5,52),(2,57),(0,61),(-9,69),(-11,70),(18,55)],
        [(-4,71),(0,58),(7,61),U,U,U,U,U,U,U,U,U,U,U,U],
        [U,(9,41),(18,25),(9,32),(5,43),(9,47),(0,44),(0,51),(2,46),(19,38),(-4,66),(15,38),(12,42),(9,34),(0,89)],
    ],
    // cabac_init_idc model 2
    [
        [(-4,86),(-12,88),(-5,82),(-3,72),(-4,67),(-8,72),(-16,89),(-9,69),(-1,59),(5,66),(4,57),(-4,71),(-2,71),(2,58),(-1,74)],
        [U,(-4,44),(-1,69),(0,62),(-7,51),(-4,47),(-6,42),(-3,41),(-6,53),(8,76),(-9,78),(-11,83),(9,52),(0,67),(-5,90)],
        [(-3,78),(-8,74),(-9,72),(-10,72),(-18,75),(-12,71),(-11,63),(-5,70),(-17,75),(-14,72),(-16,67),(-8,53),(-14,59),(-9,52),(-11,68)],
        [U,U,U,U,U,U,U,U,U,U,U,U,U,U,U],
        [U,U,U,U,U,U,U,U,U,U,U,U,U,U,U],
        [(1,67),(-15,72),(-5,75),(-8,80),(-21,83),(-21,64),(-13,31),(-25,64),(-29,94),(9,75),(17,63),(-8,74),(-5,35),(-2,27),(13,91)],
        [(3,65),(-7,69),(8,77),U,U,U,U,U,U,U,U,U,U,U,U],
        [U,(-10,66),(3,62),(-3,68),(-20,81),(0,30),(1,7),(-3,23),(-21,74),(16,66),(-23,124),(17,37),(44,-18),(50,-34),(-22,127)],
    ],
];
#[rustfmt::skip]
const LAST_P: [[[(i32, i32); 15]; 8]; 3] = [
    // cabac_init_idc model 0
    [
        [(11,28),(2,40),(3,44),(0,49),(0,46),(2,44),(2,51),(0,47),(4,39),(2,62),(6,46),(0,54),(3,54),(2,58),(4,63)],
        [U,(6,51),(6,57),(7,53),(6,52),(6,55),(11,45),(14,36),(8,53),(-1,82),(7,55),(-3,78),(15,46),(22,31),(-1,84)],
        [(9,-2),(26,-9),(33,-9),(39,-7),(41,-2),(45,3),(49,9),(45,27),(36,59),U,U,U,U,U,U],
        [U,U,U,U,U,U,U,U,U,U,U,U,U,U,U],
        [U,U,U,U,U,U,U,U,U,U,U,U,U,U,U],
        [(25,7),(30,-7),(28,3),(28,4),(32,0),(34,-1),(30,6),(30,6),(32,9),(31,19),(26,27),(26,30),(37,20),(28,34),(17,70)],
        [(1,67),(5,59),(9,67),U,U,U,U,U,U,U,U,U,U,U,U],
        [U,(16,30),(18,32),(18,35),(22,29),(24,31),(23,38),(18,43),(20,41),(11,63),(9,59),(9,64),(-1,94),(-2,89),(-9,108)],
    ],
    // cabac_init_idc model 1
    [
        [(4,45),(10,28),(10,31),(33,-11),(52,-43),(18,15),(28,0),(35,-22),(38,-25),(34,0),(39,-18),(32,-12),(102,-94),U,(56,-15)],
        [U,(33,-4),(29,10),(37,-5),(51,-29),(39,-9),(52,-34),(69,-58),(67,-63),(44,-5),(32,7),(55,-29),(32,1),U,(27,36)],
        [(17,-10),(32,-13),(42,-9),(49,-5),(53,0),(64,3),(68,10),(66,27),(47,57),U,U,U,U,U,U],
        [U,U,U,U,U,U,U,U,U,U,U,U,U,U,U],
        [U,U,U,U,U,U,U,U,U,U,U,U,U,U,U],
        [(33,-25),(34,-30),(36,-28),(38,-28),(38,-27),(34,-18),(35,-16),(34,-14),(32,-8),(37,-6),(35,0),(30,10),(28,18),(26,25),(29,41)],
        [(0,75),(2,72),(8,77),U,U,U,U,U,U,U,U,U,U,U,U],
        [U,(14,35),(18,31),(17,35),(21,30),(17,45),(20,42),(18,45),(27,26),(16,54),(7,66),(16,56),(11,73),(10,67),(-10,116)],
    ],
    // cabac_init_idc model 2
    [
        [(4,39),(0,42),(7,34),(11,29),(8,31),(6,37),(7,42),(3,40),(8,33),(13,43),(13,36),(4,47),(3,55),(2,58),(6,60)],
        [U,(8,44),(11,44),(14,42),(7,48),(4,56),(4,52),(13,37),(9,49),(19,58),(10,48),(12,45),(0,69),(20,33),(8,63)],
        [(9,-2),(30,-10),(31,-4),(33,-1),(33,7),(31,12),(37,23),(31,38),(20,64),U,U,U,U,U,U],
        [U,U,U,U,U,U,U,U,U,U,U,U,U,U,U],
        [U,U,U,U,U,U,U,U,U,U,U,U,U,U,U],
        [(35,-18),(33,-25),(28,-3),(24,10),(27,0),(34,-14),(52,-44),(39,-24),(19,17),(31,25),(36,29),(24,33),(34,15),(30,20),(22,73)],
        [(20,34),(19,31),(27,44),U,U,U,U,U,U,U,U,U,U,U,U],
        [U,(19,16),(15,36),(15,36),(21,28),(25,21),(30,20),(31,12),(27,16),(24,42),(0,93),(14,56),(15,57),(26,38),(-24,127)],
    ],
];
#[rustfmt::skip]
const ONE_P: [[[(i32, i32); 5]; 8]; 3] = [
    // cabac_init_idc model 0
    [
        [(-6,76),(-2,44),(0,45),(0,52),(-3,64)],
        [(-9,77),(3,24),(0,42),(0,48),(0,55)],
        [(-6,66),(-7,35),(-7,42),(-8,45),(-5,48)],
        [U,U,U,U,U],
        [(1,58),(-3,29),(-1,36),(1,38),(2,43)],
        [(0,70),(-4,29),(5,31),(7,42),(1,59)],
        [(0,58),(8,5),(10,14),(14,18),(13,27)],
        [U,U,U,U,U],
    ],
    // cabac_init_idc model 1
    [
        [(-23,112),(-15,71),(-7,61),(0,53),(-5,66)],
        [(-21,101),(-3,39),(-5,53),(-7,61),(-11,75)],
        [(-5,71),(0,24),(-1,36),(-2,42),(-2,52)],
        [U,U,U,U,U],
        [(-11,76),(-10,44),(-10,52),(-10,57),(-9,58)],
        [(2,66),(-9,34),(1,32),(11,31),(5,52)],
        [(3,52),(7,4),(10,8),(17,8),(16,19)],
        [U,U,U,U,U],
    ],
    // cabac_init_idc model 2
    [
        [(-24,115),(-22,82),(-9,62),(0,53),(0,59)],
        [(-21,100),(-14,57),(-12,67),(-11,71),(-10,77)],
        [(-9,71),(-7,37),(-8,44),(-11,49),(-10,56)],
        [U,U,U,U,U],
        [(-10,82),(-8,48),(-8,61),(-8,66),(-7,70)],
        [(-4,79),(-22,69),(-16,75),(-2,58),(1,58)],
        [(-13,81),(-6,38),(-13,62),(-6,58),(-2,59)],
        [U,U,U,U,U],
    ],
];
#[rustfmt::skip]
const ABS_P: [[[(i32, i32); 5]; 8]; 3] = [
    // cabac_init_idc model 0
    [
        [(-2,59),(-4,70),(-4,75),(-8,82),(-17,102)],
        [(-6,59),(-7,71),(-12,83),(-11,87),(-30,119)],
        [(-12,56),(-6,60),(-5,62),(-8,66),(-8,76)],
        [U,U,U,U,U],
        [(-6,55),(0,58),(0,64),(-3,74),(-10,90)],
        [(-2,58),(-3,72),(-3,81),(-11,97),U],
        [(2,40),(0,58),(-3,70),(-6,79),(-8,85)],
        [U,U,U,U,U],
    ],
    // cabac_init_idc model 1
    [
        [(-11,77),(-9,80),(-9,84),(-10,87),(-34,127)],
        [(-15,77),(-17,91),(-25,107),(-25,111),(-28,122)],
        [(-9,57),(-6,63),(-4,65),(-4,67),(-7,82)],
        [U,U,U,U,U],
        [(-16,72),(-7,69),(-4,69),(-5,74),(-9,86)],
        [(-2,55),(-2,67),(0,73),(-8,89),U],
        [(3,37),(-1,61),(-5,73),(-1,70),(-4,78)],
        [U,U,U,U,U],
    ],
    // cabac_init_idc model 2
    [
        [(-14,85),(-13,89),(-13,94),(-11,92),(-29,127)],
        [(-21,85),(-16,88),(-23,104),(-15,98),(-37,127)],
        [(-12,59),(-8,63),(-9,67),(-6,68),(-10,79)],
        [U,U,U,U,U],
        [(-14,75),(-10,79),(-9,83),(-12,92),(-18,108)],
        [(-13,78),(-9,83),(-4,81),(-13,99),U],
        [(-16,73),(-10,76),(-13,86),(-9,83),(-10,87)],
        [U,U,U,U,U],
    ],
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
    /// `slice_qp`. `inter` selects the P-slice init tables (indexed by the
    /// slice's cabac_init_idc `model`, 0..2) instead of the I-slice ones (which
    /// are always model 0). The bcbp (coded_block_flag) contexts are built
    /// separately via [`bcbp_contexts`].
    pub fn coeff_contexts(self, slice_qp: i32, inter: bool, model: usize) -> CoeffContexts {
        let mk = |(m, n): (i32, i32)| CtxState::init(m, n, slice_qp);
        let (_, ml_row, oa_row) = self.rows();
        let (map, last, one_t, abs_t): (&[[(i32, i32); 15]; 8], _, &[[(i32, i32); 5]; 8], _) =
            if inter { (&MAP_P[model], &LAST_P[model], &ONE_P[model], &ABS_P[model]) } else { (&MAP_I, &LAST_I, &ONE_I, &ABS_I) };
        let one = one_t[oa_row];
        let abs = abs_t[oa_row];
        CoeffContexts {
            sig: map[ml_row].iter().copied().map(mk).collect(),
            last: last[ml_row].iter().copied().map(mk).collect(),
            level: std::array::from_fn(|i| if i < 5 { mk(one[i]) } else { mk(abs[i - 5]) }),
        }
    }

    /// The 4 coded_block_flag contexts for this category at `slice_qp`
    /// (indexed by 2*upper_bit + left_bit). `inter` selects the P tables for the
    /// slice's cabac_init_idc `model`.
    pub fn bcbp_contexts(self, slice_qp: i32, inter: bool, model: usize) -> [CtxState; 4] {
        let bcbp = if inter { &BCBP_P[model] } else { &BCBP_I };
        let (bcbp_row, _, _) = self.rows();
        bcbp[bcbp_row].map(|(m, n)| CtxState::init(m, n, slice_qp))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn residual_p_tables_have_three_models_matching_jm() {
        // The P residual coeff tables must carry all three cabac_init_idc models
        // (only model 0 was transcribed originally — 1/2 are needed for any P/B
        // slice with cabac_init_idc != 0). Spot-check model-1/2 row 0 against
        // JM ctx_tables.h INIT_*_P[model][0].
        assert_eq!(BCBP_P[1][0], [(0, 80), (-5, 89), (-7, 94), (-4, 92)]);
        assert_eq!(BCBP_P[2][0], [(11, 80), (5, 76), (2, 84), (5, 78)]);
        assert_eq!(ONE_P[1][0], [(-23, 112), (-15, 71), (-7, 61), (0, 53), (-5, 66)]);
        // model 0 is unchanged from the original single-model table.
        assert_eq!(ABS_P[0][0], [(-2, 59), (-4, 70), (-4, 75), (-8, 82), (-17, 102)]);
    }

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
        let c = ResidualCat::Luma8x8.coeff_contexts(26, false, 0);
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
        let dc = ResidualCat::Luma16Dc.coeff_contexts(26, false, 0);
        assert_eq!(dc.sig[0], CtxState::init(-7, 93, 26));
        assert_eq!(dc.level[0], CtxState::init(-3, 71, 26));
        assert_eq!(ResidualCat::Luma16Dc.bcbp_contexts(26, false, 0)[0], CtxState::init(-17, 123, 26));
        // LUMA_4x4 (type 5): map/last row 5 entry 0 = (-13, 108); one/abs
        // row 4 entry 0 = (-12, 92); bcbp row 4 entry 0 = (-3, 70).
        let l4 = ResidualCat::Luma4x4.coeff_contexts(26, false, 0);
        assert_eq!(l4.sig[0], CtxState::init(-13, 108, 26));
        assert_eq!(l4.level[0], CtxState::init(-12, 92, 26));
        assert_eq!(ResidualCat::Luma4x4.bcbp_contexts(26, false, 0)[0], CtxState::init(-3, 70, 26));
        // 16AC starts at scan position 1.
        assert_eq!(ResidualCat::Luma16Ac.desc().max_num_coeff, 15);
        assert_eq!(ResidualCat::Luma16Ac.desc().pos2ctx_map[0], 1);
    }

    #[test]
    fn chroma_dc_uses_one_abs_row_5_and_bcbp_row_5() {
        // CHROMA_DC: type2ctx_one[6]=5 -> ONE_I row 5 entry 0 = (-11, 97);
        // type2ctx_bcbp[6]=5 -> BCBP_I row 5 entry 0 = (-1, 74).
        let c = ResidualCat::ChromaDc.coeff_contexts(26, false, 0);
        assert_eq!(c.level[0], CtxState::init(-11, 97, 26));
        let b = ResidualCat::ChromaDc.bcbp_contexts(26, false, 0);
        assert_eq!(b[0], CtxState::init(-1, 74, 26));
    }
}
