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
use super::transform::{
    chroma_dc_2x2, dequant_4x4, idct_4x4, inverse_scan_4x4, inverse_scan_8x8, luma_dc_4x4,
    reconstruct_residual_4x4, reconstruct_residual_8x8, FLAT_WEIGHT_8X8,
};

/// Reconstructed residual samples for one MB (to add to the prediction).
#[derive(Clone)]
pub struct MbResidual {
    pub luma: [[i32; 16]; 16],
    pub cb: [[i32; 8]; 8],
    pub cr: [[i32; 8]; 8],
}
impl Default for MbResidual {
    fn default() -> Self {
        MbResidual { luma: [[0; 16]; 16], cb: [[0; 8]; 8], cr: [[0; 8]; 8] }
    }
}

/// Chroma QP from luma QP (§ 8.5.8, Table 8-15), 8-bit (QpBdOffsetC = 0).
fn chroma_qp(qpy: i32, offset: i32) -> i32 {
    #[rustfmt::skip]
    const MAP: [i32; 22] = [29,30,31,32,32,33,34,34,35,35,36,36,37,37,37,38,38,38,39,39,39,39];
    let qpi = (qpy + offset).clamp(0, 51);
    if qpi < 30 { qpi } else { MAP[(qpi - 30) as usize] }
}

/// Inverse-transform a 4×4 AC block (15 scan coeffs, positions 1..15) with an
/// externally dequantised DC inserted — the I_16x16 / chroma path where DC
/// comes from the Hadamard transform.
fn recon_4x4_ac_with_dc(ac: &[i32], dc: i32, qp: i32) -> [[i32; 4]; 4] {
    let mut scan = [0i32; 16];
    scan[1..16].copy_from_slice(&ac[..15]);
    let mut d = inverse_scan_4x4(&scan);
    dequant_4x4(&mut d, qp);
    d[0][0] = dc;
    idct_4x4(&d)
}

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
/// Decode + emit one cbf-gated residual block, returning its scan-order
/// coefficients (None when coded_block_flag = 0).
#[allow(clippy::too_many_arguments)]
fn decode_block(
    e: &mut CabacEngine,
    ctxs: &mut ResidualContexts,
    cat: ResidualCat,
    cbf_ctx: usize,
    name: &str,
    out: &mut Vec<ResElem>,
) -> Option<Vec<i32>> {
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
        Some(coeffs)
    } else {
        out.push((name.into(), 0, 0.into()));
        None
    }
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
) -> MbResidual {
    let cbp_luma = info.cbp & 0x0f;
    let cbp_chroma = info.cbp >> 4;
    let mut res = MbResidual::default();

    // ---- Luma ----
    if !info.i_nxn {
        // I_16x16: a luma DC block (Hadamard), then 16 AC blocks if cbp_luma.
        let up = neigh.up.map_or(1, |c| c.get(0));
        let left = neigh.left.map_or(1, |c| c.get(0));
        let dc16 = match decode_block(e, ctxs, ResidualCat::Luma16Dc, (2 * up + left) as usize, "DC luma 16x16", out) {
            Some(coeffs) => {
                neigh.cur.set(0);
                luma_dc_4x4(&inverse_scan_4x4(&to16(&coeffs)), qp)
            }
            None => [[0i32; 4]; 4],
        };
        if cbp_luma != 0 {
            decode_luma_4x4_blocks(e, ctxs, neigh, ResidualCat::Luma16Ac, cbp_luma, qp, Some(&dc16), &mut res.luma, out);
        } else {
            // No AC: each 4×4 block is just its DC (idct of DC-only block).
            for by in 0..4usize {
                for bx in 0..4usize {
                    place4x4(&mut res.luma, bx, by, recon_4x4_ac_with_dc(&[0; 15], dc16[by][bx], qp));
                }
            }
        }
    } else if info.transform8x8 {
        for b8 in 0..4u32 {
            if cbp_luma & (1 << b8) != 0 {
                let (bx8, by8) = ((b8 & 1) as usize, (b8 >> 1) as usize);
                let samples = decode_luma8x8(e, ctxs, qp, out);
                for yy in 0..8 {
                    for xx in 0..8 {
                        res.luma[by8 * 8 + yy][bx8 * 8 + xx] = samples[yy][xx];
                    }
                }
                for sub in 0..4u32 {
                    neigh.cur.set(luma4x4_bit(b8 % 2 * 2 + (sub & 1), b8 / 2 * 2 + (sub >> 1)));
                }
            }
        }
    } else {
        decode_luma_4x4_blocks(e, ctxs, neigh, ResidualCat::Luma4x4, cbp_luma, qp, None, &mut res.luma, out);
    }

    // ---- Chroma (4:2:0) ----
    if cbp_chroma != 0 {
        let cqp = chroma_qp(qp, chroma_qp_index_offset);
        let mut dc = [[[0i32; 2]; 2]; 2]; // [uv][cj][ci]
        for uv in 0..2u32 {
            let bit = 17 + uv;
            let up = neigh.up.map_or(1, |c| c.get(bit));
            let left = neigh.left.map_or(1, |c| c.get(bit));
            if let Some(coeffs) = decode_block(e, ctxs, ResidualCat::ChromaDc, (2 * up + left) as usize, "2x2 DC Chroma", out) {
                neigh.cur.set(bit);
                // Chroma DC: 2×2 inverse Hadamard + scale.
                let c2 = [[coeffs[0], coeffs[1]], [coeffs[2], coeffs[3]]];
                dc[uv as usize] = chroma_dc_2x2(&c2, cqp);
            }
        }
        // Each chroma component's 4 4×4 blocks: DC always, AC if cbp_chroma==2.
        for uv in 0..2usize {
            let plane = if uv == 0 { &mut res.cb } else { &mut res.cr };
            let base = if uv == 0 { 19u32 } else { 35 };
            for cj in 0..2usize {
                for ci in 0..2usize {
                    let ac = if cbp_chroma == 2 {
                        let left = if ci > 0 {
                            neigh.cur.get(base + 4 * cj as u32 + (ci as u32 - 1))
                        } else {
                            neigh.left.map_or(1, |c| c.get(base + 4 * cj as u32 + 1))
                        };
                        let up = if cj > 0 {
                            neigh.cur.get(base + 4 * (cj as u32 - 1) + ci as u32)
                        } else {
                            neigh.up.map_or(1, |c| c.get(base + 4 + ci as u32))
                        };
                        let r = decode_block(e, ctxs, ResidualCat::ChromaAc, (2 * up + left) as usize, "AC Chroma", out);
                        if r.is_some() {
                            neigh.cur.set(base + 4 * cj as u32 + ci as u32);
                        }
                        r
                    } else {
                        None
                    };
                    let block = recon_4x4_ac_with_dc(ac.as_deref().unwrap_or(&[0; 15]), dc[uv][cj][ci], cqp);
                    for yy in 0..4 {
                        for xx in 0..4 {
                            plane[cj * 4 + yy][ci * 4 + xx] = block[yy][xx];
                        }
                    }
                }
            }
        }
    }
    res
}

/// First 16 entries of a coeff vec as a fixed array (DC blocks have 16).
fn to16(c: &[i32]) -> [i32; 16] {
    let mut a = [0i32; 16];
    a[..c.len().min(16)].copy_from_slice(&c[..c.len().min(16)]);
    a
}

fn place4x4(luma: &mut [[i32; 16]; 16], bx: usize, by: usize, block: [[i32; 4]; 4]) {
    for yy in 0..4 {
        for xx in 0..4 {
            luma[by * 4 + yy][bx * 4 + xx] = block[yy][xx];
        }
    }
}

/// Iterate the 16 luma 4×4 blocks in JM's 8×8-region-then-raster order,
/// decoding each per the 8×8-region cbp bit. Used for I_16x16 AC (start at
/// scan position 1, category LUMA_16AC) and I_4x4 (LUMA_4x4).
#[allow(clippy::too_many_arguments)]
fn decode_luma_4x4_blocks(
    e: &mut CabacEngine,
    ctxs: &mut ResidualContexts,
    neigh: &mut CbfNeighbours,
    cat: ResidualCat,
    cbp_luma: u8,
    qp: i32,
    dc16: Option<&[[i32; 4]; 4]>,
    luma: &mut [[i32; 16]; 16],
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
                    let coeffs = decode_block(e, ctxs, cat, (2 * up + left) as usize, "Luma sng", out);
                    if coeffs.is_some() {
                        neigh.cur.set(luma4x4_bit(bx, by));
                    }
                    let (bxu, byu) = (bx as usize, by as usize);
                    let block = if let Some(dc) = dc16 {
                        // I_16x16 AC block: DC from the Hadamard, AC decoded here.
                        recon_4x4_ac_with_dc(coeffs.as_deref().unwrap_or(&[0; 15]), dc[byu][bxu], qp)
                    } else if let Some(c) = coeffs {
                        // I_4x4 block: full 4×4 residual.
                        reconstruct_residual_4x4(&inverse_scan_4x4(&to16(&c)), qp)
                    } else {
                        [[0i32; 4]; 4]
                    };
                    place4x4(luma, bxu, byu, block);
                }
            }
        }
    }
}

/// Decode one coded 8×8 luma block (no cbf), emitting "Luma8x8 DC sng" for
/// the first (level, run) and "Luma8x8 sng" for the rest; returns the
/// reconstructed 8×8 residual samples (flat scaling).
fn decode_luma8x8(e: &mut CabacEngine, ctxs: &mut ResidualContexts, qp: i32, out: &mut Vec<ResElem>) -> [[i32; 8]; 8] {
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
    let mut scan = [0i32; 64];
    scan.copy_from_slice(&coeffs[..64]);
    reconstruct_residual_8x8(&inverse_scan_8x8(&scan), qp, &FLAT_WEIGHT_8X8)
}
