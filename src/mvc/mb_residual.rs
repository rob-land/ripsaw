// Macroblock residual orchestration for CABAC intra MBs (§ 7.3.5.3 +
// § 9.3.3.1.1.9). Ties the per-block residual decode (`residual.rs`) to the
// coded_block_flag neighbour context (JM `read_and_store_CBP_block_bit`) and
// the block-iteration order JM uses, for every intra category: I_16x16 luma
// DC/AC, I_4x4 / I_8x8 luma, and 4:2:0 chroma DC/AC.
//
// CABAC contexts adapt across the whole slice, so the per-category context
// banks here are built once (`ResidualContexts::new`) and shared by every
// block. The coded_block_flag for a block is decoded from a context picked
// by its left/up neighbour blocks' cbf bits (tracked per MB in `CbpBits`,
// JM `s_cbp`); LUMA_8x8 has no cbf (inferred from the CBP).
//
// This module emits the same (name, level, run) trace elements JM writes, so
// it can be diffed element-for-element against ldecod (`examples/`).

use super::cabac::{CabacEngine, CtxState};
use super::mb_header::MbInfo;
use super::residual::{decode_residual_block, CoeffContexts};
use super::residual_ctx::ResidualCat;

/// Per-MB coded_block_flag bits (JM `s_cbp[0].bits`): bit 0 = luma 16DC,
/// bit `1 + 4·by + bx` = luma 4×4 block, 17/18 = chroma Cb/Cr DC,
/// `19 + 4·cj + ci` / `35 + …` = chroma Cb/Cr AC block.
#[derive(Clone, Copy, Default)]
pub struct CbpBits(pub u64);
impl CbpBits {
    #[inline]
    fn get(self, bit: u32) -> u32 {
        ((self.0 >> bit) & 1) as u32
    }
    #[inline]
    fn set(&mut self, bit: u32) {
        self.0 |= 1u64 << bit;
    }
}

/// Left/up neighbour cbf state for the current MB (None = unavailable).
pub struct CbfNeighbours {
    pub cur: CbpBits,
    pub left: Option<CbpBits>,
    pub up: Option<CbpBits>,
}

const CATS: [ResidualCat; 6] = [
    ResidualCat::Luma16Dc,
    ResidualCat::Luma16Ac,
    ResidualCat::Luma4x4,
    ResidualCat::Luma8x8,
    ResidualCat::ChromaDc,
    ResidualCat::ChromaAc,
];
fn cat_index(cat: ResidualCat) -> usize {
    match cat {
        ResidualCat::Luma16Dc => 0,
        ResidualCat::Luma16Ac => 1,
        ResidualCat::Luma4x4 => 2,
        ResidualCat::Luma8x8 => 3,
        ResidualCat::ChromaDc => 4,
        ResidualCat::ChromaAc => 5,
    }
}

/// Persistent residual context bank for one slice (built once at slice QP).
pub struct ResidualContexts {
    coeff: Vec<CoeffContexts>,
    bcbp: [[CtxState; 4]; 6],
}
impl ResidualContexts {
    pub fn new(slice_qp: i32) -> Self {
        ResidualContexts {
            coeff: CATS.iter().map(|c| c.coeff_contexts(slice_qp)).collect(),
            bcbp: std::array::from_fn(|i| CATS[i].bcbp_contexts(slice_qp)),
        }
    }
}

/// One decoded trace element (name, level, run) — matches JM's residual
/// trace lines so the macroblock decoder can be diffed against ldecod.
pub type ResElem = (String, i64, Option<i64>);

/// scan-order coefficients -> JM (level, run) pairs + the trailing (0,0).
fn level_run(coeffs: &[i32]) -> Vec<(i64, i64)> {
    let mut out = Vec::new();
    let mut run = 0i64;
    for &c in coeffs {
        if c == 0 {
            run += 1;
        } else {
            out.push((c as i64, run));
            run = 0;
        }
    }
    out.push((0, 0));
    out
}

/// 4×4 luma block (bx, by in 0..4) -> its s_cbp bit.
fn luma4x4_bit(bx: u32, by: u32) -> u32 {
    1 + 4 * by + bx
}

/// Decode + emit one cbf-gated residual block of category `cat`, appending
/// its trace elements under `name` and returning whether it was coded.
#[allow(clippy::too_many_arguments)]
fn decode_block(
    e: &mut CabacEngine,
    ctxs: &mut ResidualContexts,
    cat: ResidualCat,
    cbf_ctx: usize,
    name: &str,
    out: &mut Vec<ResElem>,
) -> bool {
    let ci = cat_index(cat);
    let cbf = e.decode_decision(&mut ctxs.bcbp[ci][cbf_ctx]) == 1;
    if cbf {
        let d = cat.desc();
        let coeffs = decode_residual_block(
            e,
            &mut ctxs.coeff[ci],
            d.max_num_coeff,
            d.pos2ctx_map,
            d.pos2ctx_last,
            d.gt1_cap,
        );
        for (lvl, run) in level_run(&coeffs) {
            out.push((name.into(), lvl, Some(run)));
        }
    } else {
        out.push((name.into(), 0, 0.into()));
    }
    cbf
}

/// Decode an intra MB's residual, appending JM-matching trace elements to
/// `out`. `qp` is the MB's luma QP; chroma QP mapping is applied internally.
/// Updates `neigh.cur` with this MB's cbf bits (for the next MB's context).
pub fn decode_mb_residual(
    e: &mut CabacEngine,
    ctxs: &mut ResidualContexts,
    info: &MbInfo,
    neigh: &mut CbfNeighbours,
    qp: i32,
    chroma_qp_index_offset: i32,
    out: &mut Vec<ResElem>,
) {
    let cbp_luma = info.cbp & 0x0f;
    let cbp_chroma = info.cbp >> 4;

    // ---- Luma ----
    if !info.i_nxn {
        // I_16x16: always a luma DC block, then 16 AC blocks if cbp_luma.
        let up = neigh.up.map_or(1, |c| c.get(0));
        let left = neigh.left.map_or(1, |c| c.get(0));
        if decode_block(e, ctxs, ResidualCat::Luma16Dc, (2 * up + left) as usize, "DC luma 16x16", out) {
            neigh.cur.set(0);
        }
        if cbp_luma != 0 {
            // I_16x16 cbp_luma is all-or-nothing (15), so every region reads.
            decode_luma_4x4_blocks(e, ctxs, neigh, ResidualCat::Luma16Ac, cbp_luma, out);
        }
    } else if info.transform8x8 {
        // I_8x8: 4 8×8 blocks per cbp bit, no cbf.
        for b8 in 0..4u32 {
            if cbp_luma & (1 << b8) != 0 {
                decode_luma8x8(e, ctxs, out);
                // s_cbp: an 8×8 block sets its four 4×4 sub-blocks.
                let (bx8, by8) = (b8 & 1, b8 >> 1);
                for sub in 0..4u32 {
                    neigh.cur.set(luma4x4_bit(bx8 * 2 + (sub & 1), by8 * 2 + (sub >> 1)));
                }
            }
        }
    } else {
        // I_4x4: 16 4×4 blocks, per 8×8-region cbp bit.
        decode_luma_4x4_blocks(e, ctxs, neigh, ResidualCat::Luma4x4, cbp_luma, out);
    }

    // ---- Chroma (4:2:0) ----
    let _ = (qp, chroma_qp_index_offset);
    if cbp_chroma != 0 {
        // Chroma DC: Cb then Cr.
        for uv in 0..2u32 {
            let bit = 17 + uv;
            let up = neigh.up.map_or(1, |c| c.get(bit));
            let left = neigh.left.map_or(1, |c| c.get(bit));
            if decode_block(e, ctxs, ResidualCat::ChromaDc, (2 * up + left) as usize, "2x2 DC Chroma", out) {
                neigh.cur.set(bit);
            }
        }
        // Chroma AC: present only when cbp_chroma == 2.
        if cbp_chroma == 2 {
            for uv in 0..2u32 {
                let base = if uv == 0 { 19 } else { 35 };
                for cj in 0..2u32 {
                    for ci in 0..2u32 {
                        let left = if ci > 0 {
                            neigh.cur.get(base + 4 * cj + (ci - 1))
                        } else {
                            neigh.left.map_or(1, |c| c.get(base + 4 * cj + 1))
                        };
                        let up = if cj > 0 {
                            neigh.cur.get(base + 4 * (cj - 1) + ci)
                        } else {
                            neigh.up.map_or(1, |c| c.get(base + 4 + ci))
                        };
                        if decode_block(e, ctxs, ResidualCat::ChromaAc, (2 * up + left) as usize, "AC Chroma", out) {
                            neigh.cur.set(base + 4 * cj + ci);
                        }
                    }
                }
            }
        }
    }
}

/// Iterate the 16 luma 4×4 blocks in JM's 8×8-region-then-raster order,
/// decoding each per the 8×8-region cbp bit. Used for I_16x16 AC (start at
/// scan position 1, category LUMA_16AC) and I_4x4 (LUMA_4x4).
fn decode_luma_4x4_blocks(
    e: &mut CabacEngine,
    ctxs: &mut ResidualContexts,
    neigh: &mut CbfNeighbours,
    cat: ResidualCat,
    cbp_luma: u8,
    out: &mut Vec<ResElem>,
) {
    // s_cbp bits for the 4×4 neighbours are set as we go, so the context
    // derivation reads back the current MB's already-decoded blocks. Only
    // 8×8 regions whose cbp bit is set are read (JM gates per region).
    for region_y in 0..2u32 {
        for region_x in 0..2u32 {
            let b8 = region_y * 2 + region_x;
            if cbp_luma & (1 << b8) == 0 {
                continue;
            }
            for sub_y in 0..2u32 {
                for sub_x in 0..2u32 {
                    let bx = region_x * 2 + sub_x;
                    let by = region_y * 2 + sub_y;
                    let left = if bx > 0 {
                        neigh.cur.get(luma4x4_bit(bx - 1, by))
                    } else {
                        neigh.left.map_or(1, |c| c.get(luma4x4_bit(3, by)))
                    };
                    let up = if by > 0 {
                        neigh.cur.get(luma4x4_bit(bx, by - 1))
                    } else {
                        neigh.up.map_or(1, |c| c.get(luma4x4_bit(bx, 3)))
                    };
                    if decode_block(e, ctxs, cat, (2 * up + left) as usize, "Luma sng", out) {
                        neigh.cur.set(luma4x4_bit(bx, by));
                    }
                }
            }
        }
    }
}

/// Decode one coded 8×8 luma block (no cbf), emitting "Luma8x8 DC sng" for
/// the first (level, run) and "Luma8x8 sng" for the rest.
fn decode_luma8x8(e: &mut CabacEngine, ctxs: &mut ResidualContexts, out: &mut Vec<ResElem>) {
    let cat = ResidualCat::Luma8x8;
    let d = cat.desc();
    let coeffs = decode_residual_block(
        e,
        &mut ctxs.coeff[cat_index(cat)],
        d.max_num_coeff,
        d.pos2ctx_map,
        d.pos2ctx_last,
        d.gt1_cap,
    );
    for (idx, (lvl, run)) in level_run(&coeffs).into_iter().enumerate() {
        let name = if idx == 0 { "Luma8x8 DC sng" } else { "Luma8x8 sng" };
        out.push((name.into(), lvl, Some(run)));
    }
}
