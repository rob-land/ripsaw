//! Inter (P-slice) reconstruction + the inter in-loop deblock, built on
//! `recon::Frame`. Decodes a single-reference P-slice — mb_skip, the inter
//! mb_type partitions, MV prediction (median + directional) / P_Skip MV, motion
//! compensation (`mc_luma`/`mc_chroma`) and the inter residual — against a
//! post-deblock reference frame, returning the reconstructed (pre-deblock)
//! frame plus its per-4×4 motion field (for a later B-slice's co-located
//! direct prediction and for the inter deblock bS). Validated bit-exact vs JM
//! (`examples/decode_inter_full`).

use crate::mvc::bitstream::BitReader;
use crate::mvc::cabac::CabacEngine;
use crate::mvc::mb_header::{decode_cbp_ctx, decode_dquant_ctx, MbInfo};
use crate::mvc::mb_inter::{
    decode_b_mb_type, decode_b_sub_mb_type, decode_inter_mb_type, decode_mb_skip_flag, decode_mb_skip_flag_b, decode_mvd_component, decode_ref_idx, decode_sub_mb_type, interpret_b_mb_type, mvd_ctx_inc,
    InterContexts,
};
use crate::mvc::mb_residual::{decode_mb_residual, CbfNeighbours, CbpBits, ResidualContexts};
use crate::mvc::mc::{mc_chroma_into, mc_luma_into, Plane};
use crate::mvc::mv::{predict_mv, predict_skip_mv, Directional, Neighbour};
use crate::mvc::pps::Pps;
use crate::mvc::recon::{reconstruct_intra_mb, split_bands, Band, Frame, Plane as OutPlane};
use crate::mvc::scaling::ScalingLists;
use crate::mvc::slice_header::{parse_slice_header, PredWeights};
use crate::mvc::sps::Sps;

/// Per-4×4-block motion (list-0): MV, ref index (-1 = intra/none), and the
/// luma nonzero-coefficient flag (for the deblock bS). Co-located source for a
/// B-slice's direct prediction.
///
/// `refpoc` is the display POC of the picture each block *referenced* (not the
/// local ref index) — needed for temporal-direct `MapColToList0` (§ 8.4.1.2.3),
/// which maps the co-located block's reference into the current L0 by identity.
/// It is resolved by the caller (which knows this frame's reference POCs) via
/// [`MotionField::resolve_refpoc`]; `-1` where the block is intra/unused. Empty
/// when unresolved (spatial-direct streams never read it).
#[derive(Clone)]
pub struct MotionField {
    pub mv: Vec<(i32, i32)>,
    pub refidx: Vec<i32>,
    pub refpoc: Vec<i32>,
    pub nz: Vec<bool>,
    pub bw4: usize,
    pub bh4: usize,
}

impl MotionField {
    /// Fill `refpoc` from this frame's own L0 reference POCs: for each 4×4
    /// block, the POC of `ref_pocs[refidx]` (or `-1` when the block is
    /// intra/unused). Call once, at the caller, right after the frame decodes —
    /// that is where the ref-list POCs are known. Needed only as a temporal-
    /// direct co-located source; harmless (and cheap) to always call.
    pub fn resolve_refpoc(&mut self, ref_pocs: &[i32]) {
        self.refpoc = self
            .refidx
            .iter()
            .map(|&r| if r >= 0 { ref_pocs.get(r as usize).copied().unwrap_or(-1) } else { -1 })
            .collect();
    }
}

/// Partition shapes (bx4, by4, w4, h4, directional-predictor) for a non-P_8x8
/// inter `mb_type` (§ 7.4.5). Returns `&'static` data — no allocation per MB.
fn partitions(mb_type: i64) -> &'static [(usize, usize, usize, usize, Option<Directional>)] {
    match mb_type {
        1 => &[(0, 0, 4, 4, None)],
        2 => &[(0, 0, 4, 2, Some(Directional::Above)), (0, 2, 4, 2, Some(Directional::Left))],
        3 => &[(0, 0, 2, 4, Some(Directional::Left)), (2, 0, 2, 4, Some(Directional::AboveRight))],
        _ => &[],
    }
}

/// Decode a single-reference P-slice's RBSP into a (pre-deblock) `Frame` plus
/// its motion field. `reference` is the post-deblock frame the MC reads from.
/// `idr` selects the slice-header ref-marking layout: `false` for a temporal
/// P-slice, `true` for an MVC dependent-view ANCHOR (idr_pic_flag = 1 — still
/// a P-slice, but IDR-marked: idr_pic_id + simple ref marking). Its single L0
/// reference is `reference` (the base-view picture, inter-view prediction).
/// `slices` are the P-slice's coded slices in decode order (single-slice
/// frames pass `&[rbsp]`); slice boundaries break MV prediction / neighbour
/// contexts (cross-slice neighbours unavailable). See `decode_intra_frame`.
/// `refs` is L0 (index = ref_idx): one frame for single-ref, `[temporal,
/// inter-view]` for the MVC dependent temporal P (ref_idx decoded per
/// partition when num_ref_idx_l0_active > 1).
/// Decode one or more P-slices of a frame into a single buffer set, returning
/// the (pre-deblock) frame, its motion field, and the number of MBs decoded.
/// Does NOT verify full-frame coverage — the caller does (so a single slice can
/// be decoded in isolation for the per-slice-parallel path).
/// The per-slice band views (see [`Band`]/[`OutPlane`]) a P-slice decoder writes
/// into: disjoint MB-row bands of the frame's shared output + scratch buffers,
/// so the parallel slices assemble the frame with no per-slice copy/merge.
struct PBufs<'a> {
    y: OutPlane<'a>,
    cb: OutPlane<'a>,
    cr: OutPlane<'a>,
    g_mv: Band<'a, (i32, i32)>,
    g_mvd: Band<'a, (i32, i32)>,
    g_ref: Band<'a, i32>,
    nz: Band<'a, bool>,
    modes: Band<'a, Option<u8>>,
    skip_grid: Band<'a, bool>,
    cbp_grid: Band<'a, CbpBits>,
    cbpv: Band<'a, u8>,
    mb_info: Band<'a, MbInfo>,
    qp_grid: Band<'a, i32>,
}

/// Decode one P-slice (or a set of them, in the single-thread fallback) into the
/// pre-allocated band views `bufs`. Returns the count of decoded MBs plus the
/// deblock params (from the last slice header). All buffer indexing stays in
/// global frame coordinates; the [`Band`]/[`OutPlane`] views subtract each
/// band's base so writes land in the shared frame's correct rows.
#[allow(clippy::too_many_arguments)]
fn decode_p_frame_one(slices: &[&[u8]], nal_ref_idc: u8, idr: bool, sps: &Sps, pps: &Pps, refs: &[&Frame], bufs: PBufs) -> anyhow::Result<(usize, (u32, i32, i32))> {
    let width = sps.pic_width_in_mbs as usize;
    let (fw, fh) = (width * 16, sps.pic_height_in_map_units as usize * 16);
    let (cw, ch) = (fw / 2, fh / 2);
    let (bw4, bh4) = (fw / 4, fh / 4);

    // Per-ref-index L0 reference planes.
    let ref_planes: Vec<(Plane, Plane, Plane)> = refs
        .iter()
        .map(|r| (Plane { data: &r.y, w: fw, h: fh }, Plane { data: &r.cb, w: cw, h: ch }, Plane { data: &r.cr, w: cw, h: ch }))
        .collect();
    // A P-slice needs at least one reference; an empty list means the stream
    // was entered mid-GOP (no preceding IDR/anchor). Bail cleanly so the
    // caller falls back rather than indexing an empty ref list.
    anyhow::ensure!(!ref_planes.is_empty(), "P-slice with no reference (stream doesn't start at a clean GOP?)");
    let scaling = pps.scaling.clone().or_else(|| sps.scaling.clone()).unwrap_or_else(ScalingLists::flat);

    let PBufs { mut y, mut cb, mut cr, mut g_mv, mut g_mvd, mut g_ref, mut nz, mut modes, mut skip_grid, mut cbp_grid, mut cbpv, mut mb_info, mut qp_grid } = bufs;
    let num_mbs = width * (fh / 16);
    let mut decoded_mbs = 0usize;
    let mut sh_last = None;
    let _ = ch;

    // Neighbour accessor: `None` only when the neighbour MB is truly
    // unavailable — out of frame, in an earlier slice (MB address <
    // the current slice's first MB), or NOT YET DECODED (MB address >
    // the current MB, i.e. `cur_addr`). The last case matters for the
    // above-right (C) neighbour: for a lower partition it points into the
    // still-undecoded right MB, and § 6.4.11.7 then substitutes the above-left
    // (D). Without the `> cur_addr` guard that future cell reads back as its
    // init (0, 0, -1) and the C→D fallback never fires — a bug only visible
    // with mixed references (num_ref > 1), since a single-ref directional
    // predictor always matches and never consults C.
    // An available-but-intra neighbour returns `Some((0, 0, -1))` (mv 0, ref
    // -1): the median treats it as a zero-MV non-match, and the P_Skip
    // zero-condition / "B,C unavailable → A" rule (§ 8.4.1.1 / 8.4.1.3.2) must
    // distinguish "MB not available" from "MB is intra".
    let nb = |g_mv: &Band<(i32, i32)>, g_ref: &Band<i32>, bx: i32, by: i32, slice_start: usize, cur_addr: usize| -> Neighbour {
        if bx < 0 || by < 0 || bx >= bw4 as i32 || by >= bh4 as i32 {
            return None;
        }
        let nmb = (by as usize / 4) * width + (bx as usize / 4);
        if nmb < slice_start || nmb > cur_addr {
            return None;
        }
        let i = by as usize * bw4 + bx as usize;
        Some((g_mv[i].0, g_mv[i].1, g_ref[i]))
    };

    for rbsp in slices {
        let mut sr = BitReader::new(rbsp);
        let sh = parse_slice_header(&mut sr, idr, nal_ref_idc, sps, pps)?;
        let slice_qp = 26 + pps.pic_init_qp_minus26 + sh.slice_qp_delta;
        let idc = sh.cabac_init_idc.unwrap_or(0);
        let slice_start = sh.first_mb_in_slice as usize;
        let cabac_start = (sr.position_bits() + 7) / 8;
        let mut e = CabacEngine::new(&rbsp[cabac_start..]);
        let mut ctx = InterContexts::new(idc, slice_qp);
        let mut rctx = ResidualContexts::new(slice_qp, true, idc as usize % 3);
        let num_ref = (sh.num_ref_idx_l0_active_minus1 + 1) as usize;
        let pw = sh.pred_weights.clone();
        let mut last_dquant = 0;
        let mut qp = slice_qp;
        let mut addr = slice_start;
        sh_last = Some(sh);

    // Per-MB partition/motion scratch, reused across MBs (cleared, not
    // re-allocated, each MB) — a P MB has ≤16 sub-partitions, so after the
    // first few MBs these never re-grow: no allocation on the inter-MB hot path.
    let mut parts: Vec<(usize, usize, usize, usize, Option<Directional>, usize)> = Vec::new();
    let mut part_mv: Vec<(usize, usize, usize, usize, (i32, i32), i32)> = Vec::new();
    loop {
        anyhow::ensure!(addr < width * (fh / 16), "decode ran past the frame (desync — unsupported feature?) at addr {addr}");
        let mbx = addr % width;
        let mby = addr / width;
        let (mbx4, mby4) = (mbx * 4, mby * 4);
        let mb_top = addr >= width && addr - width >= slice_start;
        let left_ns = if mbx != 0 { (!skip_grid[addr - 1]) as u32 } else { 0 };
        let up_ns = if mb_top { (!skip_grid[addr - width]) as u32 } else { 0 };
        let (is_skip, _) = decode_mb_skip_flag(&mut e, &mut ctx, left_ns, up_ns);
        skip_grid[addr] = is_skip;
        decoded_mbs += 1;

        if is_skip {
            let a = nb(&g_mv, &g_ref, mbx4 as i32 - 1, mby4 as i32, slice_start, addr);
            let b = nb(&g_mv, &g_ref, mbx4 as i32, mby4 as i32 - 1, slice_start, addr);
            let c = {
                let cc = nb(&g_mv, &g_ref, mbx4 as i32 + 4, mby4 as i32 - 1, slice_start, addr);
                if cc.is_some() { cc } else { nb(&g_mv, &g_ref, mbx4 as i32 - 1, mby4 as i32 - 1, slice_start, addr) }
            };
            let mv = predict_skip_mv(a, b, c);
            fill(&mut g_mv, &mut g_ref, &mut g_mvd, mbx4, mby4, 4, 4, mv, (0, 0), 0, bw4);
            recon_part(&mut y, &mut cb, &mut cr, &ref_planes[0], mbx * 16, mby * 16, 0, 0, 16, 16, mv, &[[0i32; 16]; 16], &[[0i32; 8]; 8], &[[0i32; 8]; 8], fw, cw, wpred_of(&pw, 0, 0));
            qp_grid[addr] = qp;
            // (cbp_grid/cbpv/mb_info default to a skip MB's zeros — pre-filled.)
            // A skipped MB infers mb_qp_delta = 0, so the next coded MB's
            // mb_qp_delta context (ctxIdxInc from the previous MB's delta) must
            // see 0 here — reset the running prevMbQpDelta.
            last_dquant = 0;
            if e.decode_terminate() == 1 {
                break;
            }
            addr += 1;
            continue;
        }

        let mb_type = decode_inter_mb_type(&mut e, &mut ctx);
        // mb_type ≥ 6 is an intra MB inside the P-slice (6 = I_NxN, 7..30 =
        // I_16x16, 31 = I_PCM). Decode its intra header + residual and
        // reconstruct via the shared intra path.
        if mb_type >= 6 {
            anyhow::ensure!(mb_type != 31, "I_PCM in P-slice not supported");
            let is_i16 = mb_type >= 7;
            let mut info = MbInfo { i_nxn: !is_i16, transform8x8: false, c_ipred: 0, cbp: 0, i16_pred: 0 };
            if is_i16 {
                let x = mb_type - 7; // 0..23, same layout as the I-slice numbering
                info.i16_pred = (x % 4) as u8;
                let cbp_chroma = ((x / 4) % 3) as u8;
                let cbp_luma = if x >= 12 { 15u8 } else { 0 };
                info.cbp = cbp_luma | (cbp_chroma << 4);
            }
            let mut raw: Vec<i64> = Vec::new();
            if !is_i16 {
                // transform_size_8x8_flag is present only when the PPS enables
                // the 8x8 transform (§ 7.3.5); otherwise it's absent and reading
                // it would desync CABAC (same gate as the I-slice header).
                if pps.transform_8x8_mode_flag {
                    let lt = if mbx != 0 { mb_info[addr - 1].transform8x8 as usize } else { 0 };
                    let ut = if mb_top { mb_info[addr - width].transform8x8 as usize } else { 0 };
                    info.transform8x8 = e.decode_decision(&mut ctx.transform[lt + ut]) == 1;
                }
                for _ in 0..if info.transform8x8 { 4 } else { 16 } {
                    raw.push(if e.decode_decision(&mut ctx.ipr[0]) == 1 {
                        -1
                    } else {
                        let b0 = e.decode_decision(&mut ctx.ipr[1]);
                        let b1 = e.decode_decision(&mut ctx.ipr[1]);
                        let b2 = e.decode_decision(&mut ctx.ipr[1]);
                        (b0 | (b1 << 1) | (b2 << 2)) as i64
                    });
                }
            }
            // intra_chroma_pred_mode (ctx a/b from neighbour c_ipred != 0).
            let la = if mbx != 0 { (mb_info[addr - 1].c_ipred != 0) as usize } else { 0 };
            let ua = if mb_top { (mb_info[addr - width].c_ipred != 0) as usize } else { 0 };
            info.c_ipred = if e.decode_decision(&mut ctx.cipr[la + ua]) == 0 {
                0
            } else if e.decode_decision(&mut ctx.cipr[3]) == 0 {
                1
            } else if e.decode_decision(&mut ctx.cipr[3]) == 0 {
                2
            } else {
                3
            };
            if !is_i16 {
                let up = if mb_top { Some(cbpv[addr - width]) } else { None };
                let left = if mbx != 0 { Some(cbpv[addr - 1]) } else { None };
                info.cbp = decode_cbp_ctx(&mut e, &mut ctx.cbp, up, left) as u8;
            }
            let delta = if info.cbp != 0 || is_i16 {
                decode_dquant_ctx(&mut e, &mut ctx.delta_qp, &mut last_dquant)
            } else {
                last_dquant = 0;
                0
            };
            qp = (qp + delta).rem_euclid(52);
            let mut rneigh = CbfNeighbours {
                cur: CbpBits::default(),
                left: if mbx != 0 { Some(cbp_grid[addr - 1]) } else { None },
                up: if mb_top { Some(cbp_grid[addr - width]) } else { None },
            };
            // Intra residual (is_inter=false → cbf default_bit 1, I_16x16 DC path).
            let res = decode_mb_residual(&mut e, &mut rctx, &info, &mut rneigh, qp, pps.chroma_qp_index_offset, false, &scaling, &mut ());
            reconstruct_intra_mb(&mut y, &mut cb, &mut cr, &mut modes, &info, &raw, &res, mbx, mby, width, bw4, fw, mb_top);
            for by in 0..4u32 {
                for bx in 0..4u32 {
                    if rneigh.cur.luma4x4_nonzero(bx, by) {
                        nz[(mby4 + by as usize) * bw4 + mbx4 + bx as usize] = true;
                    }
                }
            }
            cbp_grid[addr] = rneigh.cur;
            cbpv[addr] = info.cbp;
            mb_info[addr] = info;
            qp_grid[addr] = qp;
            // g_ref/g_mv stay -1/0 for this MB (intra → unavailable for the MV
            // prediction of subsequent inter MBs).
            if e.decode_terminate() == 1 {
                break;
            }
            addr += 1;
            continue;
        }
        // (bx4, by4, w4, h4, dir, group) — `group` is the ref_idx unit: the
        // b8 for P_8x8, else the partition. P_8x8 sub_mb_type expands to its
        // sub-partitions (8x8/8x4/4x8/4x4), all L0, median prediction.
        // § 7.3.5 noSubMbPartSizeLessThan8x8Flag: for P_8x8, false as soon as
        // any sub_mb_type is not 8x8 (i.e. 8x4/4x8/4x4) — that suppresses the
        // MB's transform_size_8x8_flag below. For every non-P_8x8 mb_type the
        // partitions are ≥ 8x8, so it stays true.
        let mut no_sub_lt_8x8 = true;
        // (bx4, by4, w4, h4, directional-predictor, group). ≤16 per MB (P_8x8
        // all-4×4). Reuses the hoisted buffer's capacity.
        parts.clear();
        let ngroups: usize;
        if mb_type == 4 {
            let mut subs = [0i64; 4];
            for s in subs.iter_mut() {
                *s = decode_sub_mb_type(&mut e, &mut ctx);
            }
            no_sub_lt_8x8 = subs.iter().all(|&s| s == 0);
            for b8 in 0..4usize {
                let (gx0, gy0) = ((b8 & 1) * 2, (b8 >> 1) * 2);
                let sp: &[(usize, usize, usize, usize)] = match subs[b8] {
                    0 => &[(0, 0, 2, 2)],
                    1 => &[(0, 0, 2, 1), (0, 1, 2, 1)],
                    2 => &[(0, 0, 1, 2), (1, 0, 1, 2)],
                    _ => &[(0, 0, 1, 1), (1, 0, 1, 1), (0, 1, 1, 1), (1, 1, 1, 1)],
                };
                for &(dx, dy, w4, h4) in sp {
                    parts.push((gx0 + dx, gy0 + dy, w4, h4, None, b8));
                }
            }
            ngroups = 4;
        } else {
            let base = partitions(mb_type);
            ngroups = base.len();
            for (i, &(a, b, c, d, dir)) in base.iter().enumerate() {
                parts.push((a, b, c, d, dir, i));
            }
        }

        // ref_idx_l0 per group (before the mvds — § 7.3.5.1), when > 1 L0
        // reference. Fill g_ref so later groups'/MBs' ref_idx contexts + MV
        // prediction see it.
        // ≤4 groups (P_8x8's four b8, else one per partition).
        let mut group_ref = [0i32; 4];
        if num_ref > 1 {
            for g in 0..ngroups {
                let &(bx4, by4, ..) = parts.iter().find(|p| p.5 == g).unwrap();
                let (gx, gy) = (mbx4 + bx4, mby4 + by4);
                let la = nref(&g_ref, gx as i32 - 1, gy as i32, slice_start, width, bw4, bh4);
                let ub = nref(&g_ref, gx as i32, gy as i32 - 1, slice_start, width, bw4, bh4);
                let ridx = decode_ref_idx(&mut e, &mut ctx, (la > 0) as u32, if ub > 0 { 2 } else { 0 }) as i32;
                group_ref[g] = ridx;
                for &(pbx, pby, pw, ph, _, _) in parts.iter().filter(|p| p.5 == g) {
                    for j in 0..ph {
                        for i in 0..pw {
                            g_ref[(mby4 + pby + j) * bw4 + mbx4 + pbx + i] = ridx;
                        }
                    }
                }
            }
        }

        // (bx4, by4, w4, h4, mv, ref_idx) per partition, held between the MV
        // decode above and the reconstruction below (cbp/residual decode between).
        part_mv.clear();
        // Which of the MB's 16 4×4 cells are already decoded (filled). A
        // sub-partition's up-right (C) neighbour can point into a LATER
        // sub-partition of the SAME MB (e.g. b8_2's lower block → the not-yet-
        // decoded b8_3): § 6.4.11.7 makes that C unavailable, so the median
        // must substitute the above-left (D). Without this the init cell reads
        // back as an available zero-MV predictor and the sub-block's MV is
        // wrong. (The cross-MB analog — the undecoded RIGHT MB — is handled by
        // `nb`'s `nmb > cur_addr` guard; this is the within-MB case, which bites
        // even single-ref P since sub-partitions predict via the median, not a
        // directional predictor.)
        let mut mb_dec = [false; 16];
        for &(bx4, by4, w4, h4, dir, group) in parts.iter() {
            let ridx = group_ref[group];
            let (gx, gy) = (mbx4 + bx4, mby4 + by4);
            let lmvd = nb_mvd(&g_mvd, &g_ref, gx as i32 - 1, gy as i32, bw4, width, slice_start, bh4);
            let umvd = nb_mvd(&g_mvd, &g_ref, gx as i32, gy as i32 - 1, bw4, width, slice_start, bh4);
            let incx = mvd_ctx_inc(lmvd.0.abs() + umvd.0.abs());
            let incy = mvd_ctx_inc(lmvd.1.abs() + umvd.1.abs());
            let mvd = (decode_mvd_component(&mut e, &mut ctx, 0, incx) as i32, decode_mvd_component(&mut e, &mut ctx, 1, incy) as i32);
            let a = nb(&g_mv, &g_ref, gx as i32 - 1, gy as i32, slice_start, addr);
            let b = nb(&g_mv, &g_ref, gx as i32, gy as i32 - 1, slice_start, addr);
            let c = {
                let (cx, cy) = (gx as i32 + w4 as i32, gy as i32 - 1);
                let cc = if within_mb_undecoded(cx, cy, mbx4, mby4, &mb_dec) { None } else { nb(&g_mv, &g_ref, cx, cy, slice_start, addr) };
                if cc.is_some() { cc } else { nb(&g_mv, &g_ref, gx as i32 - 1, gy as i32 - 1, slice_start, addr) }
            };
            let mvp = predict_mv(a, b, c, ridx, dir);
            let mv = (mvp.0 + mvd.0, mvp.1 + mvd.1);
            fill(&mut g_mv, &mut g_ref, &mut g_mvd, gx, gy, w4, h4, mv, mvd, ridx, bw4);
            for j in 0..h4 {
                for i in 0..w4 {
                    mb_dec[(by4 + j) * 4 + bx4 + i] = true;
                }
            }
            part_mv.push((bx4, by4, w4, h4, mv, ridx));
        }

        let up = if mb_top { Some(cbpv[addr - width]) } else { None };
        let left = if mbx != 0 { Some(cbpv[addr - 1]) } else { None };
        let cbp = decode_cbp_ctx(&mut e, &mut ctx.cbp, up, left);
        let mut transform8x8 = false;
        if cbp & 0x0f != 0 && pps.transform_8x8_mode_flag && no_sub_lt_8x8 {
            let lt = if mbx != 0 { mb_info[addr - 1].transform8x8 as usize } else { 0 };
            let ut = if mb_top { mb_info[addr - width].transform8x8 as usize } else { 0 };
            transform8x8 = e.decode_decision(&mut ctx.transform[lt + ut]) == 1;
        }
        let delta = if cbp != 0 {
            decode_dquant_ctx(&mut e, &mut ctx.delta_qp, &mut last_dquant)
        } else {
            last_dquant = 0;
            0
        };
        qp = (qp + delta).rem_euclid(52);
        let info = MbInfo { i_nxn: false, transform8x8, c_ipred: 0, cbp: cbp as u8, i16_pred: 0 };
        let mut rneigh = CbfNeighbours {
            cur: CbpBits::default(),
            left: if mbx != 0 { Some(cbp_grid[addr - 1]) } else { None },
            up: if mb_top { Some(cbp_grid[addr - width]) } else { None },
        };
        let res = decode_mb_residual(&mut e, &mut rctx, &info, &mut rneigh, qp, pps.chroma_qp_index_offset, true, &scaling, &mut ());
        cbp_grid[addr] = rneigh.cur;
        cbpv[addr] = cbp as u8;
        mb_info[addr] = info;
        qp_grid[addr] = qp;
        for by in 0..4u32 {
            for bx in 0..4u32 {
                if rneigh.cur.luma4x4_nonzero(bx, by) {
                    nz[(mby4 + by as usize) * bw4 + mbx4 + bx as usize] = true;
                }
            }
        }

        for &(bx4, by4, w4, h4, mv, ridx) in part_mv.iter() {
            let (px, py) = (mbx * 16 + bx4 * 4, mby * 16 + by4 * 4);
            // A ref_idx past the supplied reference list means this P-slice uses
            // more L0 references than the caller built — currently the case for a
            // hierarchical MVC dependent-view P-frame with num_ref > 1 (inter-view
            // base + temporal), which decode_hier_view doesn't yet assemble. Bail
            // cleanly so the caller falls back rather than panicking on the index.
            anyhow::ensure!((ridx as usize) < ref_planes.len(), "P-slice ref_idx {ridx} exceeds {} built references (multi-ref dependent-view L0 not supported yet)", ref_planes.len());
            recon_part(&mut y, &mut cb, &mut cr, &ref_planes[ridx as usize], px, py, bx4 * 4, by4 * 4, w4 * 4, h4 * 4, mv, &res.luma, &res.cb, &res.cr, fw, cw, wpred_of(&pw, 0, ridx as usize));
        }
        if e.decode_terminate() == 1 {
            break;
        }
        addr += 1;
    }
    } // per-slice loop

    let _ = num_mbs;
    let sh = sh_last.expect("at least one slice");
    Ok((decoded_mbs, (sh.disable_deblocking_filter_idc, sh.slice_alpha_c0_offset_div2, sh.slice_beta_offset_div2)))
}

/// Decode a P-slice frame (pre-deblock) + its motion field. The frame's output +
/// scratch buffers are allocated once and split into disjoint per-slice MB-row
/// bands ([`split_bands`]); the (independent) slices decode into their bands in
/// parallel, writing the assembled frame directly with no merge. Single-slice
/// frames decode inline. Verifies full MB coverage.
pub fn decode_p_frame(slices: &[&[u8]], nal_ref_idc: u8, idr: bool, sps: &Sps, pps: &Pps, refs: &[&Frame]) -> anyhow::Result<(Frame, MotionField)> {
    let width = sps.pic_width_in_mbs as usize;
    let (fw, fh) = (width * 16, sps.pic_height_in_map_units as usize * 16);
    let (cw, ch) = (fw / 2, fh / 2);
    let (bw4, bh4) = (fw / 4, fh / 4);
    let num_mbs = width * (fh / 16);

    // Shared output + scratch buffers, initialised once (each slice overwrites
    // its own band; the init values persist for intra/skip cells, e.g. g_ref = -1).
    let mut y = vec![0u8; fw * fh];
    let mut cb = vec![0u8; cw * ch];
    let mut cr = vec![0u8; cw * ch];
    let mut g_mv = vec![(0i32, 0i32); bw4 * bh4];
    let mut g_mvd = vec![(0i32, 0i32); bw4 * bh4];
    let mut g_ref = vec![-1i32; bw4 * bh4];
    let mut nz = vec![false; bw4 * bh4];
    let mut modes = vec![Some(2u8); bw4 * bh4];
    let mut skip_grid = vec![false; num_mbs];
    let mut cbp_grid = vec![CbpBits::default(); num_mbs];
    let mut cbpv = vec![0u8; num_mbs];
    let default_mb = MbInfo { i_nxn: false, transform8x8: false, c_ipred: 0, cbp: 0, i16_pred: 0 };
    let mut mb_info = vec![default_mb; num_mbs];
    let mut qp_grid = vec![0i32; num_mbs];

    let (n_total, dp) = {
        let firsts = slice_first_mbs(slices, idr, nal_ref_idc, sps, pps)?;
        anyhow::ensure!(firsts.iter().all(|&f| f % width == 0), "non-MB-row-aligned slice (unsupported)");
        let row_ends: Vec<usize> = (0..slices.len()).map(|k| firsts.get(k + 1).copied().unwrap_or(num_mbs) / width).collect();
        let (yb, cbnd, g4, mbb) = band_bounds(&row_ends, fw, cw, bw4, width);

        let mut yv = split_bands(&mut y, &yb).into_iter();
        let mut cbv = split_bands(&mut cb, &cbnd).into_iter();
        let mut crv = split_bands(&mut cr, &cbnd).into_iter();
        let mut gmv = split_bands(&mut g_mv, &g4).into_iter();
        let mut gmvd = split_bands(&mut g_mvd, &g4).into_iter();
        let mut gref = split_bands(&mut g_ref, &g4).into_iter();
        let mut nzv = split_bands(&mut nz, &g4).into_iter();
        let mut modv = split_bands(&mut modes, &g4).into_iter();
        let mut skv = split_bands(&mut skip_grid, &mbb).into_iter();
        let mut cbgv = split_bands(&mut cbp_grid, &mbb).into_iter();
        let mut cbvv = split_bands(&mut cbpv, &mbb).into_iter();
        let mut mbiv = split_bands(&mut mb_info, &mbb).into_iter();
        let mut qpv = split_bands(&mut qp_grid, &mbb).into_iter();

        let mut bufs_list: Vec<PBufs> = Vec::with_capacity(slices.len());
        for &f in &firsts {
            let r0 = f / width;
            bufs_list.push(PBufs {
                y: OutPlane { d: yv.next().unwrap(), w: fw, base: r0 * 16 * fw },
                cb: OutPlane { d: cbv.next().unwrap(), w: cw, base: r0 * 8 * cw },
                cr: OutPlane { d: crv.next().unwrap(), w: cw, base: r0 * 8 * cw },
                g_mv: Band::new(gmv.next().unwrap(), r0 * 4 * bw4),
                g_mvd: Band::new(gmvd.next().unwrap(), r0 * 4 * bw4),
                g_ref: Band::new(gref.next().unwrap(), r0 * 4 * bw4),
                nz: Band::new(nzv.next().unwrap(), r0 * 4 * bw4),
                modes: Band::new(modv.next().unwrap(), r0 * 4 * bw4),
                skip_grid: Band::new(skv.next().unwrap(), r0 * width),
                cbp_grid: Band::new(cbgv.next().unwrap(), r0 * width),
                cbpv: Band::new(cbvv.next().unwrap(), r0 * width),
                mb_info: Band::new(mbiv.next().unwrap(), r0 * width),
                qp_grid: Band::new(qpv.next().unwrap(), r0 * width),
            });
        }

        if slices.len() < 2 {
            let bufs = bufs_list.pop().unwrap();
            decode_p_frame_one(slices, nal_ref_idc, idr, sps, pps, refs, bufs)?
        } else {
            let results: Vec<anyhow::Result<(usize, (u32, i32, i32))>> = std::thread::scope(|scope| {
                let handles: Vec<_> = slices
                    .iter()
                    .zip(bufs_list)
                    .map(|(sl, bufs)| scope.spawn(move || decode_p_frame_one(std::slice::from_ref(sl), nal_ref_idc, idr, sps, pps, refs, bufs)))
                    .collect();
                handles.into_iter().map(|h| h.join().unwrap_or_else(|_| Err(anyhow::anyhow!("P-slice decode thread panicked")))).collect()
            });
            let mut total = 0usize;
            let mut dp = (0u32, 0i32, 0i32);
            for r in results {
                let (n, d) = r?;
                total += n;
                dp = d;
            }
            (total, dp)
        }
    };
    anyhow::ensure!(n_total == num_mbs, "slice desync — {n_total}/{num_mbs} MBs decoded (unsupported feature?)");

    let (disable, alpha, beta) = dp;
    let frame = Frame {
        y,
        cb,
        cr,
        fw,
        fh,
        cw,
        ch,
        width_mbs: width,
        mb_info,
        qp: qp_grid,
        disable_deblock_idc: disable,
        slice_alpha_c0_offset_div2: alpha,
        slice_beta_offset_div2: beta,
    };
    let mf = MotionField { mv: g_mv, refidx: g_ref, refpoc: Vec::new(), nz, bw4, bh4 };
    Ok((frame, mf))
}

/// Parse the `first_mb_in_slice` of each slice (they partition the frame into
/// contiguous, ascending MB ranges).
fn slice_first_mbs(slices: &[&[u8]], idr: bool, nal_ref_idc: u8, sps: &Sps, pps: &Pps) -> anyhow::Result<Vec<usize>> {
    slices
        .iter()
        .map(|sl| Ok(parse_slice_header(&mut BitReader::new(sl), idr, nal_ref_idc, sps, pps)?.first_mb_in_slice as usize))
        .collect()
}

/// Per-slice element-boundary lists for [`split_bands`], from the per-slice MB
/// row-end boundaries: luma (`fw`-stride), chroma (`cw`-stride), 4×4 grid
/// (`bw4`-stride), per-MB grid (`width`-stride).
fn band_bounds(row_ends: &[usize], fw: usize, cw: usize, bw4: usize, width: usize) -> (Vec<usize>, Vec<usize>, Vec<usize>, Vec<usize>) {
    (
        row_ends.iter().map(|&r| r * 16 * fw).collect(),
        row_ends.iter().map(|&r| r * 8 * cw).collect(),
        row_ends.iter().map(|&r| r * 4 * bw4).collect(),
        row_ends.iter().map(|&r| r * width).collect(),
    )
}

/// Per-4×4-block, two-list motion for a B-frame: per list an MV and the POC of
/// the referenced picture (`-1` = list unused); plus the intra flag and the
/// luma nonzero-coefficient flag. Drives the two-list deblock bS ([`deblock_b`])
/// and, for a b-pyramid *referenced* B, serves as the co-located source for a
/// later frame's temporal direct (via [`BMotionField::colocated`]).
pub struct BMotionField {
    pub mv: [Vec<(i32, i32)>; 2],
    pub refidx: [Vec<i32>; 2],
    pub refpoc: [Vec<i32>; 2],
    pub intra: Vec<bool>,
    pub nz: Vec<bool>,
    pub bw4: usize,
    pub bh4: usize,
}

impl BMotionField {
    /// Reduce this B-frame's two-list motion to a single co-located
    /// [`MotionField`] (§ 8.4.1.2.1): per 4×4 block use L0 when it predicts from
    /// L0 (refpoc[0] ≥ 0), else L1; an intra block contributes refIdx −1. The
    /// resulting `refpoc` carries the referenced picture's POC (temporal-direct
    /// `MapColToList0`); `refidx` is 0 for an inter block (only its sign matters
    /// downstream — refIdx < 0 flags intra) and `mv` the chosen list's vector.
    pub fn colocated(&self) -> MotionField {
        let n = self.mv[0].len();
        let mut mv = vec![(0, 0); n];
        let mut refidx = vec![-1i32; n];
        let mut refpoc = vec![-1i32; n];
        for i in 0..n {
            let (list, rp) = if self.refpoc[0][i] >= 0 {
                (0, self.refpoc[0][i])
            } else if self.refpoc[1][i] >= 0 {
                (1, self.refpoc[1][i])
            } else {
                continue; // intra / no motion → refidx stays −1
            };
            mv[i] = self.mv[list][i];
            refidx[i] = self.refidx[list][i].max(0); // chosen list's refIdxCol (for colZeroFlag)
            refpoc[i] = rp;
        }
        MotionField { mv, refidx, refpoc, nz: self.nz.clone(), bw4: self.bw4, bh4: self.bh4 }
    }
}

fn b_sub_geom(s: i64) -> (u8, &'static [(usize, usize, usize, usize)]) {
    let pdir = match s {
        0 => 3,
        1 | 4 | 5 | 10 => 0,
        2 | 6 | 7 | 11 => 1,
        _ => 2,
    };
    let parts: &[(usize, usize, usize, usize)] = match s {
        0 | 1 | 2 | 3 => &[(0, 0, 2, 2)],
        4 | 6 | 8 => &[(0, 0, 2, 1), (0, 1, 2, 1)],
        5 | 7 | 9 => &[(0, 0, 1, 2), (1, 0, 1, 2)],
        _ => &[(0, 0, 1, 1), (1, 0, 1, 1), (0, 1, 1, 1), (1, 1, 1, 1)],
    };
    (pdir, parts)
}

/// True when a neighbour 4×4 cell `(cx, cy)` (frame coords) lies inside the
/// current macroblock but has NOT been decoded yet — i.e. it belongs to a later
/// sub-partition in this MB. Such a cell is unavailable for MV prediction
/// (§ 6.4.11.7 substitutes the above-left neighbour D instead).
fn within_mb_undecoded(cx: i32, cy: i32, mbx4: usize, mby4: usize, mb_dec: &[bool; 16]) -> bool {
    let (mx, my) = (mbx4 as i32, mby4 as i32);
    if cx >= mx && cx < mx + 4 && cy >= my && cy < my + 4 {
        !mb_dec[(cy - my) as usize * 4 + (cx - mx) as usize]
    } else {
        false
    }
}

/// Like [`within_mb_undecoded`] but for the B decoder's list-major MV
/// derivation: a within-MB cell is unavailable if its partition (`cell_plan`
/// spatial index) is NOT before the current partition `pi` — i.e. it belongs to
/// a later b8/sub-partition in decode order (§ 6.4.11.7). A cell decoded by an
/// earlier b8 that simply doesn't use this list stays available.
fn within_mb_later(cx: i32, cy: i32, mbx4: usize, mby4: usize, cell_plan: &[usize; 16], pi: usize) -> bool {
    let (mx, my) = (mbx4 as i32, mby4 as i32);
    if cx >= mx && cx < mx + 4 && cy >= my && cy < my + 4 {
        cell_plan[(cy - my) as usize * 4 + (cx - mx) as usize] >= pi
    } else {
        false
    }
}

fn min_positive(a: i32, b: i32) -> i32 {
    if a >= 0 && b >= 0 {
        a.min(b)
    } else {
        a.max(b)
    }
}

/// Temporal-direct derivation (§ 8.4.1.2.3) for one direct 8×8 (with
/// direct_8x8_inference_flag = 1 the co-located sample is the block's outer
/// corner 4×4, whose grid index is `cidx`). Returns
/// `(mvL0, mvL1, refIdxL0, refIdxL1, useL0, useL1)`. Both lists are always
/// used in temporal direct (bi-prediction); refIdxL1 is always 0.
///
/// `col` is RefPicList1[0]'s motion field, with `refpoc` resolved to the POC of
/// the picture each co-located block referenced. `poc[0]`/`poc[1]` are the
/// current L0/L1 reference POCs (index = ref_idx); `cur` is the current POC.
fn temporal_direct(col: &MotionField, cidx: usize, poc: &[Vec<i32>; 2], cur: i32) -> ((i32, i32), (i32, i32), i32, i32, bool, bool) {
    // Co-located block intra / outside the field / no motion → zero motion,
    // refIdxL0 = 0 (§ 8.4.1.2.3: refIdxCol < 0 ⇒ mvL0 = mvL1 = 0, refIdxL0 = 0).
    if cidx >= col.mv.len() || col.refidx.get(cidx).copied().unwrap_or(-1) < 0 {
        return ((0, 0), (0, 0), 0, 0, true, true);
    }
    let mvcol = col.mv[cidx];
    let colrefpoc = col.refpoc.get(cidx).copied().unwrap_or(-1);
    // MapColToList0: the current L0 index whose picture is the one the
    // co-located block referenced (matched by POC identity). Falls back to 0
    // (nearest) when unresolved — correct for the single-ref case.
    let ref0 = poc[0].iter().position(|&p| p == colrefpoc).unwrap_or(0) as i32;
    let (p0, p1) = (poc[0][ref0 as usize], poc[1][0]);
    let td = (p1 - p0).clamp(-128, 127);
    let tb = (cur - p0).clamp(-128, 127);
    if td == 0 {
        // Equal POCs (or would-be long-term): no scaling, mvL0 = mvCol, mvL1 = 0.
        return (mvcol, (0, 0), ref0, 0, true, true);
    }
    let tx = (16384 + (td.abs() / 2)) / td;
    let dsf = ((tb * tx + 32) >> 6).clamp(-1024, 1023);
    let scale = |v: i32| (dsf * v + 128) >> 8;
    let mvl0 = (scale(mvcol.0), scale(mvcol.1));
    let mvl1 = (mvl0.0 - mvcol.0, mvl0.1 - mvcol.1);
    (mvl0, mvl1, ref0, 0, true, true)
}

/// Spatial-direct derivation (§ 8.4.1.2.2) per MB: median predictor + refIdx per
/// list. `resolve(colZeroFlag)` gives the per-block MVs and per-list use flags.
struct Direct {
    ref0: i32,
    ref1: i32,
    mvp0: (i32, i32),
    mvp1: (i32, i32),
    zero: bool,
}
impl Direct {
    fn resolve(&self, colzero: bool) -> ((i32, i32), (i32, i32), bool, bool) {
        // § 8.4.1.2.2: colZeroFlag zeros a list's MV only when its refIdx is 0
        // (the nearest reference). With MVC multi-ref L0, an inter-view direct
        // block has refIdxL0 = 1, so colZeroFlag must NOT zero its (disparity)
        // MV — it keeps the median predictor. (Base single-ref B always has
        // refIdx 0, so the old unconditional zeroing was correct there.)
        let mv0 = if self.zero || self.ref0 < 0 || (colzero && self.ref0 == 0) { (0, 0) } else { self.mvp0 };
        let mv1 = if self.zero || self.ref1 < 0 || (colzero && self.ref1 == 0) { (0, 0) } else { self.mvp1 };
        (mv0, mv1, self.ref0 >= 0, self.ref1 >= 0)
    }
}

/// B-frame two-list per-4×4 motion grids (MV/refIdx/mvd per list), with
/// cross-slice-aware neighbour access.
struct BGrids<'a> {
    mv: [Band<'a, (i32, i32)>; 2],
    refi: [Band<'a, i32>; 2],
    mvd: [Band<'a, (i32, i32)>; 2],
    bw4: usize,
    bh4: usize,
    width: usize,
}
impl BGrids<'_> {
    fn nb(&self, list: usize, bx: i32, by: i32, slice_start: usize, cur_addr: usize) -> Neighbour {
        if bx < 0 || by < 0 || bx >= self.bw4 as i32 || by >= self.bh4 as i32 {
            return None;
        }
        // Unavailable if in an earlier slice OR not yet decoded (MB address >
        // the current MB) — the latter is the above-right (C) of a lower
        // partition pointing into the still-undecoded right MB (§ 6.4.11.7
        // then substitutes above-left D); see the P-slice `nb`.
        let nmb = (by as usize / 4) * self.width + (bx as usize / 4);
        if nmb < slice_start || nmb > cur_addr {
            return None;
        }
        // `None` only for a truly-unavailable MB; an available neighbour that
        // is intra or doesn't use this list returns `Some((0, 0, -1))` (its
        // cell holds mv 0, ref -1) — so the median treats it as a zero-MV
        // non-match rather than as "not available".
        let i = by as usize * self.bw4 + bx as usize;
        Some((self.mv[list][i].0, self.mv[list][i].1, self.refi[list][i]))
    }
    fn nb_mvd(&self, list: usize, bx: i32, by: i32, slice_start: usize) -> (i32, i32) {
        if bx < 0 || by < 0 || (by as usize / 4) * self.width + (bx as usize / 4) < slice_start {
            return (0, 0);
        }
        let i = by as usize * self.bw4 + bx as usize;
        if self.refi[list][i] < 0 { (0, 0) } else { self.mvd[list][i] }
    }
    #[allow(clippy::too_many_arguments)]
    fn fill(&mut self, list: usize, gx: usize, gy: usize, w4: usize, h4: usize, mv: (i32, i32), refi: i32, mvd: (i32, i32)) {
        for j in 0..h4 {
            for i in 0..w4 {
                let idx = (gy + j) * self.bw4 + gx + i;
                self.mv[list][idx] = mv;
                self.refi[list][idx] = refi;
                self.mvd[list][idx] = mvd;
            }
        }
    }
    fn spatial_direct(&self, mbx4: usize, mby4: usize, slice_start: usize, cur_addr: usize) -> Direct {
        let (x, yy) = (mbx4 as i32, mby4 as i32);
        let mut d = Direct { ref0: -1, ref1: -1, mvp0: (0, 0), mvp1: (0, 0), zero: false };
        for list in 0..2 {
            let a = self.nb(list, x - 1, yy, slice_start, cur_addr);
            let b = self.nb(list, x, yy - 1, slice_start, cur_addr);
            let c = {
                let cc = self.nb(list, x + 4, yy - 1, slice_start, cur_addr);
                if cc.is_some() { cc } else { self.nb(list, x - 1, yy - 1, slice_start, cur_addr) }
            };
            let r = |n: Neighbour| n.map(|(_, _, r)| r).unwrap_or(-1);
            let refi = min_positive(min_positive(r(a), r(b)), r(c));
            let mvp = predict_mv(a, b, c, refi.max(0), None);
            if list == 0 {
                d.ref0 = refi;
                d.mvp0 = mvp;
            } else {
                d.ref1 = refi;
                d.mvp1 = mvp;
            }
        }
        if d.ref0 < 0 && d.ref1 < 0 {
            d.ref0 = 0;
            d.ref1 = 0;
            d.zero = true;
        }
        d
    }
}

/// Decode a B-slice frame into a (pre-deblock) `Frame` + its two-list
/// `BMotionField`. `l0`/`l1` are the reference lists as `(frame, POC)` pairs
/// (index = ref_idx); `col` is the co-located picture's L0 motion field
/// (RefPicList1[0]'s, for spatial-direct colZeroFlag); `bipred` is the implicit
/// bi-pred weight (`(32, 32)` for the default average). Multi-slice (per-slice
/// CABAC reinit + cross-slice neighbour unavailability); spatial direct only,
/// num_ref_idx = 1 per list (ref_idx not coded); intra MBs handled.
/// The per-slice band views a B-slice decoder writes into (see [`PBufs`]); the
/// two-list motion grids `mv`/`refi`/`mvd` become the decoder's [`BGrids`].
struct BBufs<'a> {
    y: OutPlane<'a>,
    cb: OutPlane<'a>,
    cr: OutPlane<'a>,
    mv: [Band<'a, (i32, i32)>; 2],
    refi: [Band<'a, i32>; 2],
    mvd: [Band<'a, (i32, i32)>; 2],
    refpoc: [Band<'a, i32>; 2],
    intra_grid: Band<'a, bool>,
    // Per-4×4 "direct-predicted" flag (B_Skip / B_Direct_16x16 / B_Direct_8x8):
    // such a neighbour contributes condTermFlag 0 to the ref_idx context even
    // though its derived refIdx may be > 0 (JM readRefFrame_CABAC).
    dir_grid: Band<'a, bool>,
    nz: Band<'a, bool>,
    modes: Band<'a, Option<u8>>,
    skip_grid: Band<'a, bool>,
    mbtype_grid: Band<'a, i64>,
    cbp_grid: Band<'a, CbpBits>,
    cbpv: Band<'a, u8>,
    mb_info: Band<'a, MbInfo>,
    qp_grid: Band<'a, i32>,
}

#[allow(clippy::too_many_arguments)]
fn decode_b_frame_one(
    slices: &[&[u8]],
    nal_ref_idc: u8,
    idr: bool,
    sps: &Sps,
    pps: &Pps,
    l0: &[(&Frame, i32)],
    l1: &[(&Frame, i32)],
    cur_poc: i32,
    col: &MotionField,
    bipred: (i32, i32),
    bufs: BBufs,
) -> anyhow::Result<(usize, (u32, i32, i32))> {
    let width = sps.pic_width_in_mbs as usize;
    let (fw, fh) = (width * 16, sps.pic_height_in_map_units as usize * 16);
    let (cw, ch) = (fw / 2, fh / 2);
    let (bw4, bh4) = (fw / 4, fh / 4);

    anyhow::ensure!(!l0.is_empty() && !l1.is_empty(), "B-frame needs an L0 and an L1 reference");
    let scaling = pps.scaling.clone().or_else(|| sps.scaling.clone()).unwrap_or_else(ScalingLists::flat);
    // Reference planes per list (index = ref_idx).
    fn planes<'a>(lst: &[(&'a Frame, i32)], fw: usize, fh: usize, cw: usize, ch: usize) -> Vec<(Plane<'a>, Plane<'a>, Plane<'a>)> {
        lst.iter()
            .map(|(r, _)| (Plane { data: &r.y, w: fw, h: fh }, Plane { data: &r.cb, w: cw, h: ch }, Plane { data: &r.cr, w: cw, h: ch }))
            .collect()
    }
    let pl = [planes(l0, fw, fh, cw, ch), planes(l1, fw, fh, cw, ch)];
    let poc = [l0.iter().map(|(_, p)| *p).collect::<Vec<_>>(), l1.iter().map(|(_, p)| *p).collect::<Vec<_>>()];

    let BBufs { mut y, mut cb, mut cr, mv, refi, mvd, mut refpoc, mut intra_grid, mut dir_grid, mut nz, mut modes, mut skip_grid, mut mbtype_grid, mut cbp_grid, mut cbpv, mut mb_info, mut qp_grid } = bufs;
    let mut g = BGrids { mv, refi, mvd, bw4, bh4, width };
    let num_mbs = width * (fh / 16);
    let mut decoded_mbs = 0usize;
    let mut sh_last = None;

    for rbsp in slices {
        let mut sr = BitReader::new(rbsp);
        let sh = parse_slice_header(&mut sr, idr, nal_ref_idc, sps, pps)?;
        let spatial_direct = sh.direct_spatial_mv_pred_flag;
        let num_ref_l0 = (sh.num_ref_idx_l0_active_minus1 + 1) as usize;
        let num_ref_l1 = (sh.num_ref_idx_l1_active_minus1 + 1) as usize;
        anyhow::ensure!(num_ref_l0 <= l0.len() && num_ref_l1 <= l1.len(), "B ref list shorter than num_ref_idx_active");
        let slice_qp = 26 + pps.pic_init_qp_minus26 + sh.slice_qp_delta;
        let idc = sh.cabac_init_idc.unwrap_or(0);
        let slice_start = sh.first_mb_in_slice as usize;
        let cabac_start = (sr.position_bits() + 7) / 8;
        let mut e = CabacEngine::new(&rbsp[cabac_start..]);
        let mut ctx = InterContexts::new(idc, slice_qp);
        let mut rctx = ResidualContexts::new(slice_qp, true, idc as usize % 3);
        let pw = sh.pred_weights.clone();
        let mut last_dquant = 0;
        let mut qp = slice_qp;
        let mut addr = slice_start;
        sh_last = Some(sh);

        loop {
            anyhow::ensure!(addr < width * (fh / 16), "B decode ran past the frame (desync) at addr {addr}");
            let (mbx, mby) = (addr % width, addr / width);
            let (mbx4, mby4) = (mbx * 4, mby * 4);
            let mb_top = addr >= width && addr - width >= slice_start;
            let left_ns = if mbx != 0 { (!skip_grid[addr - 1]) as u32 } else { 0 };
            let up_ns = if mb_top { (!skip_grid[addr - width]) as u32 } else { 0 };
            let (is_skip, _) = decode_mb_skip_flag_b(&mut e, &mut ctx, left_ns, up_ns);
            skip_grid[addr] = is_skip;
            decoded_mbs += 1;

            let mb_type = if is_skip {
                0
            } else {
                let la = if mbx != 0 { (mbtype_grid[addr - 1] != 0) as u32 } else { 0 };
                let ua = if mb_top { (mbtype_grid[addr - width] != 0) as u32 } else { 0 };
                decode_b_mb_type(&mut e, &mut ctx, la, ua)
            };
            mbtype_grid[addr] = if is_skip { 0 } else { mb_type };

            // ---- intra MB inside the B-slice (mb_type ≥ 23) ----
            if !is_skip && mb_type >= 23 {
                let it = mb_type - 23; // I-slice intra numbering (0 = I_NxN)
                anyhow::ensure!(it != 25, "I_PCM in B-slice not supported");
                let is_i16 = it >= 1;
                let mut info = MbInfo { i_nxn: !is_i16, transform8x8: false, c_ipred: 0, cbp: 0, i16_pred: 0 };
                if is_i16 {
                    let x = it - 1;
                    info.i16_pred = (x % 4) as u8;
                    let cbp_chroma = ((x / 4) % 3) as u8;
                    let cbp_luma = if x >= 12 { 15u8 } else { 0 };
                    info.cbp = cbp_luma | (cbp_chroma << 4);
                }
                let mut raw: Vec<i64> = Vec::new();
                if !is_i16 {
                    // transform_size_8x8_flag only present when the PPS enables
                    // the 8x8 transform (§ 7.3.5) — otherwise reading it desyncs.
                    if pps.transform_8x8_mode_flag {
                        let lt = if mbx != 0 { mb_info[addr - 1].transform8x8 as usize } else { 0 };
                        let ut = if mb_top { mb_info[addr - width].transform8x8 as usize } else { 0 };
                        info.transform8x8 = e.decode_decision(&mut ctx.transform[lt + ut]) == 1;
                    }
                    for _ in 0..if info.transform8x8 { 4 } else { 16 } {
                        raw.push(if e.decode_decision(&mut ctx.ipr[0]) == 1 {
                            -1
                        } else {
                            let b0 = e.decode_decision(&mut ctx.ipr[1]);
                            let b1 = e.decode_decision(&mut ctx.ipr[1]);
                            let b2 = e.decode_decision(&mut ctx.ipr[1]);
                            (b0 | (b1 << 1) | (b2 << 2)) as i64
                        });
                    }
                }
                let la = if mbx != 0 { (mb_info[addr - 1].c_ipred != 0) as usize } else { 0 };
                let ua = if mb_top { (mb_info[addr - width].c_ipred != 0) as usize } else { 0 };
                info.c_ipred = if e.decode_decision(&mut ctx.cipr[la + ua]) == 0 {
                    0
                } else if e.decode_decision(&mut ctx.cipr[3]) == 0 {
                    1
                } else if e.decode_decision(&mut ctx.cipr[3]) == 0 {
                    2
                } else {
                    3
                };
                if !is_i16 {
                    let up = if mb_top { Some(cbpv[addr - width]) } else { None };
                    let left = if mbx != 0 { Some(cbpv[addr - 1]) } else { None };
                    info.cbp = decode_cbp_ctx(&mut e, &mut ctx.cbp, up, left) as u8;
                }
                let delta = if info.cbp != 0 || is_i16 {
                    decode_dquant_ctx(&mut e, &mut ctx.delta_qp, &mut last_dquant)
                } else {
                    last_dquant = 0;
                    0
                };
                qp = (qp + delta).rem_euclid(52);
                let mut rneigh = CbfNeighbours {
                    cur: CbpBits::default(),
                    left: if mbx != 0 { Some(cbp_grid[addr - 1]) } else { None },
                    up: if mb_top { Some(cbp_grid[addr - width]) } else { None },
                };
                let res = decode_mb_residual(&mut e, &mut rctx, &info, &mut rneigh, qp, pps.chroma_qp_index_offset, false, &scaling, &mut ());
                reconstruct_intra_mb(&mut y, &mut cb, &mut cr, &mut modes, &info, &raw, &res, mbx, mby, width, bw4, fw, mb_top);
                for by in 0..4usize {
                    for bx in 0..4usize {
                        let cell = (mby4 + by) * bw4 + mbx4 + bx;
                        intra_grid[cell] = true;
                        if rneigh.cur.luma4x4_nonzero(bx as u32, by as u32) {
                            nz[cell] = true;
                        }
                    }
                }
                cbp_grid[addr] = rneigh.cur;
                cbpv[addr] = info.cbp;
                mb_info[addr] = info;
                qp_grid[addr] = qp;
                if e.decode_terminate() == 1 {
                    break;
                }
                addr += 1;
                continue;
            }

            // ---- inter (direct / explicit) ----
            // Spatial-direct predictor (used only when direct_spatial_mv_pred);
            // temporal direct derives per-8×8 from the co-located motion field.
            let direct = g.spatial_direct(mbx4, mby4, slice_start, addr);
            // Per-direct-8×8 motion, dispatching spatial vs temporal direct.
            // Returns (mvL0, mvL1, gridRefL0, gridRefL1, useL0, useL1) — the grid
            // ref is -1 for an unused list (spatial). direct_8x8_inference_flag=1
            // takes the co-located sample at the 8×8's outer corner 4×4.
            let db8 = |b8: usize| -> ((i32, i32), (i32, i32), i32, i32, bool, bool) {
                let ccol = mbx4 + if b8 & 1 == 0 { 0 } else { 3 };
                let crow = mby4 + if b8 >> 1 == 0 { 0 } else { 3 };
                let cidx = crow * bw4 + ccol;
                if spatial_direct {
                    let colzero = cidx < col.refidx.len() && col.refidx[cidx] == 0 && col.mv[cidx].0.abs() <= 1 && col.mv[cidx].1.abs() <= 1;
                    let (mv0, mv1, use0, use1) = direct.resolve(colzero);
                    (mv0, mv1, if use0 { direct.ref0 } else { -1 }, if use1 { direct.ref1 } else { -1 }, use0, use1)
                } else {
                    temporal_direct(col, cidx, &poc, cur_poc)
                }
            };
            enum Plan {
                Direct { b8: usize },
                // `group` = the mbPartIdx / b8 that owns this partition's ref_idx
                // (sub-partitions of an 8×8 share one ref_idx). `ref0`/`ref1` are
                // the decoded reference indices per list.
                Explicit { pdir: u8, dir: Option<Directional>, group: usize, ref0: i32, ref1: i32, mv0: (i32, i32), mv1: (i32, i32) },
            }
            let mut plan: Vec<(usize, usize, usize, usize, Plan)> = Vec::new();
            if is_skip || mb_type == 0 {
                for b8 in 0..4usize {
                    let (bx8, by8) = (b8 & 1, b8 >> 1);
                    plan.push((mbx4 + bx8 * 2, mby4 + by8 * 2, 2, 2, Plan::Direct { b8 }));
                }
            } else if mb_type == 22 {
                for b8 in 0..4usize {
                    let s = decode_b_sub_mb_type(&mut e, &mut ctx);
                    let (bx8, by8) = (b8 & 1, b8 >> 1);
                    let (gx0, gy0) = (mbx4 + bx8 * 2, mby4 + by8 * 2);
                    let (pdir, parts) = b_sub_geom(s);
                    if pdir == 3 {
                        plan.push((gx0, gy0, 2, 2, Plan::Direct { b8 }));
                    } else {
                        for &(dx, dy, w4, h4) in parts {
                            plan.push((gx0 + dx, gy0 + dy, w4, h4, Plan::Explicit { pdir, dir: None, group: b8, ref0: 0, ref1: 0, mv0: (0, 0), mv1: (0, 0) }));
                        }
                    }
                }
            } else {
                let (nparts, pw4, ph4, pdir) = interpret_b_mb_type(mb_type);
                for p in 0..nparts {
                    let (dx, dy, dir) = if pw4 == 4 && ph4 == 2 {
                        (0, p * 2, Some(if p == 0 { Directional::Above } else { Directional::Left }))
                    } else if pw4 == 2 && ph4 == 4 {
                        (p * 2, 0, Some(if p == 0 { Directional::Left } else { Directional::AboveRight }))
                    } else {
                        (0, 0, None)
                    };
                    plan.push((mbx4 + dx, mby4 + dy, pw4, ph4, Plan::Explicit { pdir: pdir[p], dir, group: p, ref0: 0, ref1: 0, mv0: (0, 0), mv1: (0, 0) }));
                }
            }

            // Pre-fill the refIdx grid for direct partitions with the spatial-
            // direct-derived refIdx (§ 8.4.1.2.2) so a later same-MB explicit
            // partition's ref_idx / MV-pred context reads the right value.
            for (gx, gy, w4, h4, pk) in plan.iter() {
                if let Plan::Direct { b8 } = pk {
                    let (_, _, gref0, gref1, _, _) = db8(*b8);
                    for j in 0..*h4 {
                        for i in 0..*w4 {
                            g.refi[0][(*gy + j) * bw4 + *gx + i] = gref0;
                            g.refi[1][(*gy + j) * bw4 + *gx + i] = gref1;
                            dir_grid[(*gy + j) * bw4 + *gx + i] = true;
                        }
                    }
                }
            }

            // ref_idx per mbPart per list, list-major, before all mvds
            // (§ 7.3.5.1). Only coded when that list has > 1 active reference;
            // sub-partitions of an 8×8 share the b8's ref_idx. Fill g.refi as we
            // go so a later same-MB partition sees an earlier one.
            for list in 0..2usize {
                let num_ref = if list == 0 { num_ref_l0 } else { num_ref_l1 };
                for grp in 0..4usize {
                    let uses = |pk: &Plan| matches!(pk, Plan::Explicit { group, pdir, .. } if *group == grp && ((*pdir as usize) == list || *pdir == 2));
                    let Some((gx0, gy0)) = plan.iter().find(|(_, _, _, _, pk)| uses(pk)).map(|(gx, gy, ..)| (*gx as i32, *gy as i32)) else { continue };
                    let ridx = if num_ref > 1 {
                        // ref_idx ctxIdxInc (JM readRefFrame_CABAC): condTermFlag
                        // is 0 not only for an unavailable/intra/other-list
                        // neighbour (refIdx ≤ 0) but ALSO for a DIRECT neighbour
                        // (B_Skip / B_Direct_16x16, or a direct b8) — its derived
                        // refIdx may be > 0 yet must not raise the context, or the
                        // ref_no model state drifts and a later decode desyncs.
                        let condterm = |bx: i32, by: i32| -> bool {
                            let r = match g.nb(list, bx, by, slice_start, addr) {
                                Some((_, _, r)) => r,
                                None => return false,
                            };
                            if r <= 0 {
                                return false;
                            }
                            // A direct-predicted neighbour block (whole-MB or a
                            // B_8×8 direct b8, in this MB or an earlier one) →
                            // condTermFlag 0. dir_grid is filled for the current
                            // MB's direct cells in the pre-fill pass above.
                            !dir_grid[by as usize * bw4 + bx as usize]
                        };
                        let la = condterm(gx0 - 1, gy0);
                        let ub = condterm(gx0, gy0 - 1);
                        decode_ref_idx(&mut e, &mut ctx, la as u32, if ub { 2 } else { 0 }) as i32
                    } else {
                        0
                    };
                    for (gx, gy, w4, h4, pk) in plan.iter_mut() {
                        if uses(pk) {
                            if let Plan::Explicit { ref0, ref1, .. } = pk {
                                if list == 0 { *ref0 = ridx } else { *ref1 = ridx }
                            }
                            for j in 0..*h4 {
                                for i in 0..*w4 {
                                    g.refi[list][(*gy + j) * bw4 + *gx + i] = ridx;
                                }
                            }
                        }
                    }
                }
            }

            // Spatial decode-order index per 4×4 cell (the plan is in b8 raster
            // + sub-partition order). A neighbour cell within this MB is
            // available for MV prediction only if its partition precedes the
            // current one — used for the up-right (C) availability test below.
            let mut cell_plan = [usize::MAX; 16];
            for (pi, (gx, gy, w4, h4, _)) in plan.iter().enumerate() {
                for j in 0..*h4 {
                    for i in 0..*w4 {
                        cell_plan[(*gy + j - mby4) * 4 + (*gx + i - mbx4)] = pi;
                    }
                }
            }

            // Derive direct-partition motion up front (spatial direct §8.4.1.2.2
            // + colZeroFlag) so it is available as a neighbour when the explicit
            // sub-partitions predict their MVs — JM derives a b8's direct motion
            // before the later b8s' mvds; libmvc otherwise only filled direct
            // MVs in the reconstruction pass, leaving an explicit partition's D
            // neighbour (a direct b8) reading an unset cell.
            for (gx, gy, w4, h4, pk) in plan.iter() {
                if let Plan::Direct { b8 } = pk {
                    let (mv0, mv1, gref0, gref1, _, _) = db8(*b8);
                    g.fill(0, *gx, *gy, *w4, *h4, mv0, gref0, (0, 0));
                    g.fill(1, *gx, *gy, *w4, *h4, mv1, gref1, (0, 0));
                }
            }

            // mvd for explicit partitions, list-major (all L0 then all L1). The
            // C-neighbour availability is by spatial (plan) order — NOT the list
            // pass order — so a decoded earlier-b8 that just doesn't use this
            // list (refIdxLX = -1) still counts as available (§ 6.4.11.7).
            for list in 0..2usize {
                for (pi, (gx, gy, w4, h4, pk)) in plan.iter_mut().enumerate() {
                    if let Plan::Explicit { pdir, dir, ref0, ref1, mv0, mv1, .. } = pk {
                        if (*pdir as usize) != list && *pdir != 2 {
                            continue;
                        }
                        let ridx = if list == 0 { *ref0 } else { *ref1 };
                        let lmvd = g.nb_mvd(list, *gx as i32 - 1, *gy as i32, slice_start);
                        let umvd = g.nb_mvd(list, *gx as i32, *gy as i32 - 1, slice_start);
                        let incx = mvd_ctx_inc(lmvd.0.abs() + umvd.0.abs());
                        let incy = mvd_ctx_inc(lmvd.1.abs() + umvd.1.abs());
                        let mvd = (decode_mvd_component(&mut e, &mut ctx, 0, incx) as i32, decode_mvd_component(&mut e, &mut ctx, 1, incy) as i32);
                        let a = g.nb(list, *gx as i32 - 1, *gy as i32, slice_start, addr);
                        let b = g.nb(list, *gx as i32, *gy as i32 - 1, slice_start, addr);
                        let c = {
                            let (cx, cy) = (*gx as i32 + *w4 as i32, *gy as i32 - 1);
                            let cc = if within_mb_later(cx, cy, mbx4, mby4, &cell_plan, pi) { None } else { g.nb(list, cx, cy, slice_start, addr) };
                            if cc.is_some() { cc } else { g.nb(list, *gx as i32 - 1, *gy as i32 - 1, slice_start, addr) }
                        };
                        let mvp = predict_mv(a, b, c, ridx.max(0), *dir);
                        let mv = (mvp.0 + mvd.0, mvp.1 + mvd.1);
                        if list == 0 {
                            *mv0 = mv;
                        } else {
                            *mv1 = mv;
                        }
                        g.fill(list, *gx, *gy, *w4, *h4, mv, ridx, mvd);
                    }
                }
            }

            // Residual (skip has none).
            let mut res = crate::mvc::mb_residual::MbResidual::default();
            let mut transform8x8 = false;
            if !is_skip {
                let up = if mb_top { Some(cbpv[addr - width]) } else { None };
                let left = if mbx != 0 { Some(cbpv[addr - 1]) } else { None };
                let cbp = decode_cbp_ctx(&mut e, &mut ctx.cbp, up, left);
                let all_ge_8 = mb_type != 22 || plan.iter().all(|(_, _, w4, h4, _)| *w4 >= 2 && *h4 >= 2);
                if cbp & 0x0f != 0 && pps.transform_8x8_mode_flag && all_ge_8 {
                    let lt = if mbx != 0 { mb_info[addr - 1].transform8x8 as usize } else { 0 };
                    let ut = if mb_top { mb_info[addr - width].transform8x8 as usize } else { 0 };
                    transform8x8 = e.decode_decision(&mut ctx.transform[lt + ut]) == 1;
                }
                let delta = if cbp != 0 {
                    decode_dquant_ctx(&mut e, &mut ctx.delta_qp, &mut last_dquant)
                } else {
                    last_dquant = 0;
                    0
                };
                qp = (qp + delta).rem_euclid(52);
                let info = MbInfo { i_nxn: false, transform8x8, c_ipred: 0, cbp: cbp as u8, i16_pred: 0 };
                let mut rneigh = CbfNeighbours {
                    cur: CbpBits::default(),
                    left: if mbx != 0 { Some(cbp_grid[addr - 1]) } else { None },
                    up: if mb_top { Some(cbp_grid[addr - width]) } else { None },
                };
                res = decode_mb_residual(&mut e, &mut rctx, &info, &mut rneigh, qp, pps.chroma_qp_index_offset, true, &scaling, &mut ());
                for by in 0..4u32 {
                    for bx in 0..4u32 {
                        if rneigh.cur.luma4x4_nonzero(bx, by) {
                            nz[(mby4 + by as usize) * bw4 + mbx4 + bx as usize] = true;
                        }
                    }
                }
                cbp_grid[addr] = rneigh.cur;
                cbpv[addr] = cbp as u8;
                mb_info[addr] = info;
                qp_grid[addr] = qp;
            } else {
                // skip MB — cbp_grid/cbpv/mb_info keep their pre-filled zeros.
                qp_grid[addr] = qp;
                last_dquant = 0;
            }

            // Reconstruct partitions + record deblock motion.
            for (gx, gy, w4, h4, pk) in &plan {
                let (mv0, mv1, use0, use1, ref0, ref1) = match pk {
                    Plan::Direct { b8 } => {
                        // Spatial (§ 8.4.1.2.2 colZeroFlag) or temporal
                        // (§ 8.4.1.2.3 POC scaling) direct, per db8. An empty
                        // co-located field (I-frame L1[0]) yields refIdxCol = -1
                        // → zero motion in both paths.
                        let (mv0, mv1, gref0, gref1, use0, use1) = db8(*b8);
                        g.fill(0, *gx, *gy, *w4, *h4, mv0, gref0, (0, 0));
                        g.fill(1, *gx, *gy, *w4, *h4, mv1, gref1, (0, 0));
                        (mv0, mv1, use0, use1, gref0.max(0), gref1.max(0))
                    }
                    Plan::Explicit { pdir, mv0, mv1, ref0, ref1, .. } => (*mv0, *mv1, *pdir == 0 || *pdir == 2, *pdir == 1 || *pdir == 2, (*ref0).max(0), (*ref1).max(0)),
                };
                let (ref0, ref1) = (ref0 as usize, ref1 as usize);
                for j in 0..*h4 {
                    for i in 0..*w4 {
                        let cell = (*gy + j) * bw4 + *gx + i;
                        refpoc[0][cell] = if use0 { poc[0][ref0] } else { -1 };
                        refpoc[1][cell] = if use1 { poc[1][ref1] } else { -1 };
                    }
                }
                let bwp = pw.as_ref().map(|_| (wpred_of(&pw, 0, ref0).unwrap(), wpred_of(&pw, 1, ref1).unwrap()));
                b_recon_block(&mut y, &mut cb, &mut cr, &pl, ref0, ref1, gx * 4, gy * 4, w4 * 4, h4 * 4, mv0, mv1, use0, use1, &res, fw, cw, bipred, bwp);
            }

            if e.decode_terminate() == 1 {
                break;
            }
            addr += 1;
        }
    }

    let _ = num_mbs;
    let sh = sh_last.expect("at least one slice");
    Ok((decoded_mbs, (sh.disable_deblocking_filter_idc, sh.slice_alpha_c0_offset_div2, sh.slice_beta_offset_div2)))
}

/// Decode a B-slice frame (pre-deblock) + its two-list motion field. Like
/// [`decode_p_frame`], the frame's buffers are allocated once and split into
/// disjoint per-slice MB-row bands; the independent slices decode into their
/// bands in parallel, assembling the frame directly (no merge).
#[allow(clippy::too_many_arguments)]
pub fn decode_b_frame(slices: &[&[u8]], nal_ref_idc: u8, idr: bool, sps: &Sps, pps: &Pps, l0: &[(&Frame, i32)], l1: &[(&Frame, i32)], cur_poc: i32, col: &MotionField, bipred: (i32, i32)) -> anyhow::Result<(Frame, BMotionField)> {
    let width = sps.pic_width_in_mbs as usize;
    let (fw, fh) = (width * 16, sps.pic_height_in_map_units as usize * 16);
    let (cw, ch) = (fw / 2, fh / 2);
    let (bw4, bh4) = (fw / 4, fh / 4);
    let num_mbs = width * (fh / 16);

    let mut y = vec![0u8; fw * fh];
    let mut cb = vec![0u8; cw * ch];
    let mut cr = vec![0u8; cw * ch];
    let mut mv0 = vec![(0i32, 0i32); bw4 * bh4];
    let mut mv1 = vec![(0i32, 0i32); bw4 * bh4];
    let mut refi0 = vec![-1i32; bw4 * bh4];
    let mut refi1 = vec![-1i32; bw4 * bh4];
    let mut mvd0 = vec![(0i32, 0i32); bw4 * bh4];
    let mut mvd1 = vec![(0i32, 0i32); bw4 * bh4];
    let mut refpoc0 = vec![-1i32; bw4 * bh4];
    let mut refpoc1 = vec![-1i32; bw4 * bh4];
    let mut intra_grid = vec![false; bw4 * bh4];
    let mut dir_grid = vec![false; bw4 * bh4];
    let mut nz = vec![false; bw4 * bh4];
    let mut modes = vec![Some(2u8); bw4 * bh4];
    let mut skip_grid = vec![false; num_mbs];
    let mut mbtype_grid = vec![0i64; num_mbs];
    let mut cbp_grid = vec![CbpBits::default(); num_mbs];
    let mut cbpv = vec![0u8; num_mbs];
    let default_mb = MbInfo { i_nxn: false, transform8x8: false, c_ipred: 0, cbp: 0, i16_pred: 0 };
    let mut mb_info = vec![default_mb; num_mbs];
    let mut qp_grid = vec![0i32; num_mbs];

    let (n_total, dp) = {
        let firsts = slice_first_mbs(slices, idr, nal_ref_idc, sps, pps)?;
        anyhow::ensure!(firsts.iter().all(|&f| f % width == 0), "non-MB-row-aligned slice (unsupported)");
        let row_ends: Vec<usize> = (0..slices.len()).map(|k| firsts.get(k + 1).copied().unwrap_or(num_mbs) / width).collect();
        let (yb, cbnd, g4, mbb) = band_bounds(&row_ends, fw, cw, bw4, width);

        let mut yv = split_bands(&mut y, &yb).into_iter();
        let mut cbv = split_bands(&mut cb, &cbnd).into_iter();
        let mut crv = split_bands(&mut cr, &cbnd).into_iter();
        let mut mv0v = split_bands(&mut mv0, &g4).into_iter();
        let mut mv1v = split_bands(&mut mv1, &g4).into_iter();
        let mut refi0v = split_bands(&mut refi0, &g4).into_iter();
        let mut refi1v = split_bands(&mut refi1, &g4).into_iter();
        let mut mvd0v = split_bands(&mut mvd0, &g4).into_iter();
        let mut mvd1v = split_bands(&mut mvd1, &g4).into_iter();
        let mut rp0v = split_bands(&mut refpoc0, &g4).into_iter();
        let mut rp1v = split_bands(&mut refpoc1, &g4).into_iter();
        let mut igv = split_bands(&mut intra_grid, &g4).into_iter();
        let mut dgv = split_bands(&mut dir_grid, &g4).into_iter();
        let mut nzv = split_bands(&mut nz, &g4).into_iter();
        let mut modv = split_bands(&mut modes, &g4).into_iter();
        let mut skv = split_bands(&mut skip_grid, &mbb).into_iter();
        let mut mtv = split_bands(&mut mbtype_grid, &mbb).into_iter();
        let mut cbgv = split_bands(&mut cbp_grid, &mbb).into_iter();
        let mut cbvv = split_bands(&mut cbpv, &mbb).into_iter();
        let mut mbiv = split_bands(&mut mb_info, &mbb).into_iter();
        let mut qpv = split_bands(&mut qp_grid, &mbb).into_iter();

        let mut bufs_list: Vec<BBufs> = Vec::with_capacity(slices.len());
        for &f in &firsts {
            let r0 = f / width;
            let (g4b, mbg) = (r0 * 4 * bw4, r0 * width);
            bufs_list.push(BBufs {
                y: OutPlane { d: yv.next().unwrap(), w: fw, base: r0 * 16 * fw },
                cb: OutPlane { d: cbv.next().unwrap(), w: cw, base: r0 * 8 * cw },
                cr: OutPlane { d: crv.next().unwrap(), w: cw, base: r0 * 8 * cw },
                mv: [Band::new(mv0v.next().unwrap(), g4b), Band::new(mv1v.next().unwrap(), g4b)],
                refi: [Band::new(refi0v.next().unwrap(), g4b), Band::new(refi1v.next().unwrap(), g4b)],
                mvd: [Band::new(mvd0v.next().unwrap(), g4b), Band::new(mvd1v.next().unwrap(), g4b)],
                refpoc: [Band::new(rp0v.next().unwrap(), g4b), Band::new(rp1v.next().unwrap(), g4b)],
                intra_grid: Band::new(igv.next().unwrap(), g4b),
                dir_grid: Band::new(dgv.next().unwrap(), g4b),
                nz: Band::new(nzv.next().unwrap(), g4b),
                modes: Band::new(modv.next().unwrap(), g4b),
                skip_grid: Band::new(skv.next().unwrap(), mbg),
                mbtype_grid: Band::new(mtv.next().unwrap(), mbg),
                cbp_grid: Band::new(cbgv.next().unwrap(), mbg),
                cbpv: Band::new(cbvv.next().unwrap(), mbg),
                mb_info: Band::new(mbiv.next().unwrap(), mbg),
                qp_grid: Band::new(qpv.next().unwrap(), mbg),
            });
        }

        if slices.len() < 2 {
            let bufs = bufs_list.pop().unwrap();
            decode_b_frame_one(slices, nal_ref_idc, idr, sps, pps, l0, l1, cur_poc, col, bipred, bufs)?
        } else {
            let results: Vec<anyhow::Result<(usize, (u32, i32, i32))>> = std::thread::scope(|scope| {
                let handles: Vec<_> = slices
                    .iter()
                    .zip(bufs_list)
                    .map(|(sl, bufs)| scope.spawn(move || decode_b_frame_one(std::slice::from_ref(sl), nal_ref_idc, idr, sps, pps, l0, l1, cur_poc, col, bipred, bufs)))
                    .collect();
                handles.into_iter().map(|h| h.join().unwrap_or_else(|_| Err(anyhow::anyhow!("B-slice decode thread panicked")))).collect()
            });
            let mut total = 0usize;
            let mut dp = (0u32, 0i32, 0i32);
            for r in results {
                let (n, d) = r?;
                total += n;
                dp = d;
            }
            (total, dp)
        }
    };
    anyhow::ensure!(n_total == num_mbs, "slice desync — {n_total}/{num_mbs} MBs decoded (unsupported feature?)");

    let (disable, alpha, beta) = dp;
    let frame = Frame {
        y,
        cb,
        cr,
        fw,
        fh,
        cw,
        ch,
        width_mbs: width,
        mb_info,
        qp: qp_grid,
        disable_deblock_idc: disable,
        slice_alpha_c0_offset_div2: alpha,
        slice_beta_offset_div2: beta,
    };
    let bmf = BMotionField { mv: [mv0, mv1], refidx: [refi0, refi1], refpoc: [refpoc0, refpoc1], intra: intra_grid, nz, bw4, bh4 };
    Ok((frame, bmf))
}

/// Reconstruct one B partition: uni- or bi-predicted MC (implicit weight `wt`,
/// `(32,32)` = default average) + residual, into the output planes.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_arguments)]
fn b_recon_block<'a>(
    y: &mut OutPlane<'a>, cb: &mut OutPlane<'a>, cr: &mut OutPlane<'a>,
    pl: &[Vec<(Plane, Plane, Plane)>; 2], ref0: usize, ref1: usize,
    px: usize, py: usize, w: usize, h: usize,
    mv0: (i32, i32), mv1: (i32, i32), use0: bool, use1: bool,
    res: &crate::mvc::mb_residual::MbResidual, fw: usize, cw: usize, wt: (i32, i32),
    wp: Option<(WPred, WPred)>,
) {
    let _ = (fw, cw);
    // Sample combine (§ 8.4.2.3): explicit weighted bi/uni-pred when `wp` is
    // set (weighted_bipred_idc == 1); otherwise the default average (`wt`).
    // `cw0`/`cw1` are the per-list (weight, offset) for this component, `denom`
    // its log2 weight denominator.
    let comb = |a: Option<&[u8]>, b: Option<&[u8]>, k: usize, cw0: (i32, i32), cw1: (i32, i32), denom: i32| -> i32 {
        match (a, b) {
            (Some(a), Some(b)) => {
                let (av, bv) = (a[k] as i32, b[k] as i32);
                if wp.is_some() {
                    (((av * cw0.0 + bv * cw1.0 + (1 << denom)) >> (denom + 1)) + ((cw0.1 + cw1.1 + 1) >> 1)).clamp(0, 255)
                } else {
                    ((av * wt.0 + bv * wt.1 + 32) >> 6).clamp(0, 255)
                }
            }
            (Some(a), None) => if wp.is_some() { wp_apply(a[k] as i32, cw0.0, cw0.1, denom) } else { a[k] as i32 },
            (None, Some(b)) => if wp.is_some() { wp_apply(b[k] as i32, cw1.0, cw1.1, denom) } else { b[k] as i32 },
            (None, None) => 0,
        }
    };
    let (w0, w1) = wp.unwrap_or((WPred { luma: (1, 0), luma_denom: 0, chroma: [(1, 0); 2], chroma_denom: 0 }, WPred { luma: (1, 0), luma_denom: 0, chroma: [(1, 0); 2], chroma_denom: 0 }));
    let (mut lb0, mut lb1) = ([0u8; 256], [0u8; 256]);
    let p0: Option<&[u8]> = if use0 { mc_luma_into(&pl[0][ref0].0, px as i32, py as i32, mv0.0, mv0.1, w, h, &mut lb0[..w * h]); Some(&lb0[..w * h]) } else { None };
    let p1: Option<&[u8]> = if use1 { mc_luma_into(&pl[1][ref1].0, px as i32, py as i32, mv1.0, mv1.1, w, h, &mut lb1[..w * h]); Some(&lb1[..w * h]) } else { None };
    let (rx, ry) = (px % 16, py % 16);
    for j in 0..h {
        for i in 0..w {
            let pred = comb(p0, p1, j * w + i, w0.luma, w1.luma, w0.luma_denom);
            y.set(px + i, py + j, pred + res.luma[ry + j][rx + i]);
        }
    }
    let (cpx, cpy, cww, chh) = (px / 2, py / 2, w / 2, h / 2);
    let (crx, cry) = (cpx % 8, cpy % 8);
    for pi in 0..2usize {
        let rp0 = if pi == 0 { &pl[0][ref0].1 } else { &pl[0][ref0].2 };
        let rp1 = if pi == 0 { &pl[1][ref1].1 } else { &pl[1][ref1].2 };
        let plane: &mut OutPlane = if pi == 0 { &mut *cb } else { &mut *cr };
        let (mut cb0, mut cb1) = ([0u8; 64], [0u8; 64]);
        let c0: Option<&[u8]> = if use0 { mc_chroma_into(rp0, cpx as i32, cpy as i32, mv0.0, mv0.1, cww, chh, &mut cb0[..cww * chh]); Some(&cb0[..cww * chh]) } else { None };
        let c1: Option<&[u8]> = if use1 { mc_chroma_into(rp1, cpx as i32, cpy as i32, mv1.0, mv1.1, cww, chh, &mut cb1[..cww * chh]); Some(&cb1[..cww * chh]) } else { None };
        let resc = if pi == 0 { &res.cb } else { &res.cr };
        for j in 0..chh {
            for i in 0..cww {
                let pred = comb(c0, c1, j * cww + i, w0.chroma[pi], w1.chroma[pi], w0.chroma_denom);
                plane.set(cpx + i, cpy + j, pred + resc[cry + j][crx + i]);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn fill(g_mv: &mut Band<(i32, i32)>, g_ref: &mut Band<i32>, g_mvd: &mut Band<(i32, i32)>, gx: usize, gy: usize, w4: usize, h4: usize, mv: (i32, i32), mvd: (i32, i32), ref_idx: i32, bw4: usize) {
    for j in 0..h4 {
        for i in 0..w4 {
            let idx = (gy + j) * bw4 + gx + i;
            g_mv[idx] = mv;
            g_ref[idx] = ref_idx;
            g_mvd[idx] = mvd;
        }
    }
}

/// Neighbour ref_idx (for the ref_idx context) — -1 if out of frame or in an
/// earlier slice.
fn nref(g_ref: &Band<i32>, bx: i32, by: i32, slice_start: usize, width: usize, bw4: usize, bh4: usize) -> i32 {
    if bx < 0 || by < 0 || bx as usize >= bw4 || by as usize >= bh4 || (by as usize / 4) * width + (bx as usize / 4) < slice_start {
        return -1;
    }
    g_ref[by as usize * bw4 + bx as usize]
}

#[allow(clippy::too_many_arguments)]
fn nb_mvd(g_mvd: &Band<(i32, i32)>, g_ref: &Band<i32>, bx: i32, by: i32, bw4: usize, width: usize, slice_start: usize, bh4: usize) -> (i32, i32) {
    if bx < 0 || by < 0 || bx as usize >= bw4 || by as usize >= bh4 || (by as usize / 4) * width + (bx as usize / 4) < slice_start {
        return (0, 0);
    }
    let i = by as usize * bw4 + bx as usize;
    if g_ref[i] < 0 { (0, 0) } else { g_mvd[i] }
}

#[allow(clippy::too_many_arguments)]
/// Explicit weighted-prediction weight for one reference: sample
/// `((s * weight + round) >> denom) + offset`, clamped (§ 8.4.2.3.2).
#[derive(Clone, Copy)]
struct WPred {
    luma: (i32, i32),
    luma_denom: i32,
    chroma: [(i32, i32); 2],
    chroma_denom: i32,
}
#[inline]
fn wp_apply(s: i32, w: i32, o: i32, d: i32) -> i32 {
    (if d >= 1 { ((s * w + (1 << (d - 1))) >> d) + o } else { s * w + o }).clamp(0, 255)
}

/// The [`WPred`] for reference `ridx` in `list`, or `None` for default
/// (unweighted) prediction.
fn wpred_of(pw: &Option<PredWeights>, list: usize, ridx: usize) -> Option<WPred> {
    pw.as_ref().map(|w| {
        let (lu, ch) = if list == 0 { (&w.l0_luma, &w.l0_chroma) } else { (&w.l1_luma, &w.l1_chroma) };
        WPred {
            luma: lu.get(ridx).copied().unwrap_or((1 << w.luma_denom, 0)),
            luma_denom: w.luma_denom,
            chroma: ch.get(ridx).copied().unwrap_or([(1 << w.chroma_denom, 0); 2]),
            chroma_denom: w.chroma_denom,
        }
    })
}

#[allow(clippy::too_many_arguments)]
fn recon_part<'a>(
    y: &mut OutPlane<'a>, cb: &mut OutPlane<'a>, cr: &mut OutPlane<'a>,
    rp: &(Plane, Plane, Plane),
    px: usize, py: usize, rx: usize, ry: usize, w: usize, h: usize, mv: (i32, i32),
    luma: &[[i32; 16]; 16], rcb: &[[i32; 8]; 8], rcr: &[[i32; 8]; 8], fw: usize, cw: usize,
    wp: Option<WPred>,
) {
    let _ = (fw, cw);
    let (rpy, rpcb, rpcr) = (&rp.0, &rp.1, &rp.2);
    let mut pbuf = [0u8; 256];
    let pred = &mut pbuf[..w * h];
    mc_luma_into(rpy, px as i32, py as i32, mv.0, mv.1, w, h, pred);
    for j in 0..h {
        for i in 0..w {
            let mut s = pred[j * w + i] as i32;
            if let Some(wp) = &wp {
                s = wp_apply(s, wp.luma.0, wp.luma.1, wp.luma_denom);
            }
            y.set(px + i, py + j, s + luma[ry + j][rx + i]);
        }
    }
    let (cpx, cpy, cww, chh, crx, cry) = (px / 2, py / 2, w / 2, h / 2, rx / 2, ry / 2);
    let mut cbuf = [0u8; 64];
    for (ci, (plane, res, rp)) in [(&mut *cb, rcb, rpcb), (&mut *cr, rcr, rpcr)].into_iter().enumerate() {
        let p = &mut cbuf[..cww * chh];
        mc_chroma_into(rp, cpx as i32, cpy as i32, mv.0, mv.1, cww, chh, p);
        for j in 0..chh {
            for i in 0..cww {
                let mut s = p[j * cww + i] as i32;
                if let Some(wp) = &wp {
                    s = wp_apply(s, wp.chroma[ci].0, wp.chroma[ci].1, wp.chroma_denom);
                }
                plane.set(cpx + i, cpy + j, s + res[cry + j][crx + i]);
            }
        }
    }
}

fn chroma_qp_jm(qpy: i32, offset: i32) -> i32 {
    const MAP: [i32; 22] = [29, 30, 31, 32, 32, 33, 34, 34, 35, 35, 36, 36, 37, 37, 37, 38, 38, 38, 39, 39, 39, 39];
    let qpi = (qpy + offset).clamp(0, 51);
    if qpi < 30 { qpi } else { MAP[(qpi - 30) as usize] }
}

/// Inter in-loop deblock (§ 8.7) for a P-slice (no intra MBs). bS per 4-sample
/// segment: 2 if either side has nonzero luma coeffs, 1 if refs differ or
/// |Δmv| ≥ 4 (¼-pel), else 0. Mutates `frame` in place.
pub fn deblock_inter(frame: &mut Frame, mf: &MotionField, chroma_off: i32) {
    if frame.disable_deblock_idc == 1 {
        return;
    }
    use crate::mvc::deblock::{filter_chroma, filter_luma_normal, filter_luma_strong, ALPHA, BETA, TC0};
    let off_a = frame.slice_alpha_c0_offset_div2 * 2;
    let off_b = frame.slice_beta_offset_div2 * 2;
    let (fw, cw, width) = (frame.fw, frame.cw, frame.width_mbs);
    let bw4 = mf.bw4;
    let height = frame.qp.len() / width;
    let (mv, refg, nz) = (&mf.mv, &mf.refidx, &mf.nz);
    let qp_grid = &frame.qp;

    // An intra cell (ref -1) forces bS 4 on a macroblock edge, 3 internally
    // (intra MBs occur inside P-slices via mb_type ≥ 6). Otherwise 2 for
    // nonzero coeffs, 1 for differing ref/MV, else 0.
    let bs_of = |pi: usize, qi: usize, mb_edge: bool| -> usize {
        if refg[pi] < 0 || refg[qi] < 0 {
            if mb_edge { 4 } else { 3 }
        } else if nz[pi] || nz[qi] {
            2
        } else if refg[pi] != refg[qi] || (mv[pi].0 - mv[qi].0).abs() >= 4 || (mv[pi].1 - mv[qi].1).abs() >= 4 {
            1
        } else {
            0
        }
    };
    let at = |p: &[u8], stride: usize, x: usize, yy: usize| p[yy * stride + x] as i32;

    let (y, cb, cr) = (&mut frame.y, &mut frame.cb, &mut frame.cr);
    for mby in 0..height {
        for mbx in 0..width {
            let addr = mby * width + mbx;
            let qp_q = qp_grid[addr];
            let t8 = frame.mb_info[addr].transform8x8;

            for &horiz in &[false, true] {
                for ei in 0..4usize {
                    let ofs = ei * 4;
                    let at_pic_edge = if horiz { mby == 0 } else { mbx == 0 };
                    if ofs == 0 && at_pic_edge {
                        continue;
                    }
                    if ofs != 0 && t8 && (ofs == 4 || ofs == 12) {
                        continue;
                    }
                    let qp_p = if ofs != 0 {
                        qp_q
                    } else if horiz {
                        qp_grid[addr - width]
                    } else {
                        qp_grid[addr - 1]
                    };
                    let qpav = (qp_p + qp_q + 1) >> 1;
                    let ia = (qpav + off_a).clamp(0, 51) as usize;
                    let ib = (qpav + off_b).clamp(0, 51) as usize;
                    let (alpha, beta) = (ALPHA[ia], BETA[ib]);
                    if alpha == 0 {
                        continue;
                    }
                    for seg in 0..4usize {
                        let (qbx, qby) = if horiz { (mbx * 4 + seg, mby * 4 + ofs / 4) } else { (mbx * 4 + ofs / 4, mby * 4 + seg) };
                        let (pbx, pby) = if horiz { (qbx, qby - 1) } else { (qbx - 1, qby) };
                        let bs = bs_of(pby * bw4 + pbx, qby * bw4 + qbx, ofs == 0);
                        if bs == 0 {
                            continue;
                        }
                        for line in 0..4usize {
                            let t = seg * 4 + line;
                            let (bx, by) = if horiz { (mbx * 16 + t, mby * 16 + ofs) } else { (mbx * 16 + ofs, mby * 16 + t) };
                            let mut s = [0i32; 8];
                            for (k, sl) in s.iter_mut().enumerate() {
                                *sl = if horiz { at(y, fw, bx, by - 4 + k) } else { at(y, fw, bx - 4 + k, by) };
                            }
                            if bs == 4 {
                                filter_luma_strong(&mut s, alpha, beta);
                            } else {
                                filter_luma_normal(&mut s, alpha, beta, TC0[ia][bs - 1]);
                            }
                            for (k, &v) in s.iter().enumerate() {
                                let (px, py) = if horiz { (bx, by - 4 + k) } else { (bx - 4 + k, by) };
                                y[py * fw + px] = v.clamp(0, 255) as u8;
                            }
                        }
                    }
                }
            }

            let cqp_q = chroma_qp_jm(qp_q, chroma_off);
            for plane in [&mut *cb, &mut *cr] {
                for &horiz in &[false, true] {
                    for ce in 0..2usize {
                        let cofs = ce * 4;
                        let lofs = ce * 8;
                        let at_pic_edge = if horiz { mby == 0 } else { mbx == 0 };
                        if cofs == 0 && at_pic_edge {
                            continue;
                        }
                        let qp_p_y = if cofs != 0 {
                            qp_q
                        } else if horiz {
                            qp_grid[addr - width]
                        } else {
                            qp_grid[addr - 1]
                        };
                        let cqp_p = chroma_qp_jm(qp_p_y, chroma_off);
                        let qpav = (cqp_p + cqp_q + 1) >> 1;
                        let ia = (qpav + off_a).clamp(0, 51) as usize;
                        let ib = (qpav + off_b).clamp(0, 51) as usize;
                        let (alpha, beta) = (ALPHA[ia], BETA[ib]);
                        if alpha == 0 {
                            continue;
                        }
                        for t in 0..8usize {
                            let seg = t / 2;
                            let (qbx, qby) = if horiz { (mbx * 4 + seg, mby * 4 + lofs / 4) } else { (mbx * 4 + lofs / 4, mby * 4 + seg) };
                            let (pbx, pby) = if horiz { (qbx, qby - 1) } else { (qbx - 1, qby) };
                            let bs = bs_of(pby * bw4 + pbx, qby * bw4 + qbx, cofs == 0);
                            if bs == 0 {
                                continue;
                            }
                            let (bx, by) = if horiz { (mbx * 8 + t, mby * 8 + cofs) } else { (mbx * 8 + cofs, mby * 8 + t) };
                            let mut s = [0i32; 8];
                            for (k, sl) in s.iter_mut().enumerate() {
                                *sl = if horiz { at(plane, cw, bx, by - 4 + k) } else { at(plane, cw, bx - 4 + k, by) };
                            }
                            let tc0 = if bs == 4 { 0 } else { TC0[ia][bs - 1] };
                            filter_chroma(&mut s, alpha, beta, tc0, bs == 4);
                            for (k, &v) in s.iter().enumerate() {
                                let (px, py) = if horiz { (bx, by - 4 + k) } else { (bx - 4 + k, by) };
                                plane[py * cw + px] = v.clamp(0, 255) as u8;
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Two-list in-loop deblock (§ 8.7) for a B-frame. Boundary strength per JM
/// `get_strength` for B-slices: 4/3 when either side is intra (MB / internal
/// edge), 2 for nonzero coeffs, else the two-list reference-picture + MV
/// comparison (compare the MVs for matched reference pictures across L0/L1;
/// bS = 1 when the reference sets differ). Strong filter at bS = 4.
pub fn deblock_b(frame: &mut Frame, mf: &BMotionField, chroma_off: i32) {
    if frame.disable_deblock_idc == 1 {
        return;
    }
    use crate::mvc::deblock::{filter_chroma, filter_luma_normal, filter_luma_strong, ALPHA, BETA, TC0};
    let off_a = frame.slice_alpha_c0_offset_div2 * 2;
    let off_b = frame.slice_beta_offset_div2 * 2;
    let (fw, cw, width) = (frame.fw, frame.cw, frame.width_mbs);
    let bw4 = mf.bw4;
    let height = frame.qp.len() / width;
    let qp_grid = &frame.qp;

    // Boundary strength between the p and q 4×4 cells. `mb_edge` = the edge sits
    // on the macroblock boundary (ofs == 0).
    let mv_diff = |a: (i32, i32), b: (i32, i32)| a.0.wrapping_sub(b.0).abs() >= 4 || a.1.wrapping_sub(b.1).abs() >= 4;
    let bs_of = |pi: usize, qi: usize, mb_edge: bool| -> usize {
        if mf.intra[pi] || mf.intra[qi] {
            return if mb_edge { 4 } else { 3 };
        }
        if mf.nz[pi] || mf.nz[qi] {
            return 2;
        }
        let (rp0, rp1) = (mf.refpoc[0][pi], mf.refpoc[1][pi]);
        let (rq0, rq1) = (mf.refpoc[0][qi], mf.refpoc[1][qi]);
        let (pm0, pm1) = (mf.mv[0][pi], mf.mv[1][pi]);
        let (qm0, qm1) = (mf.mv[0][qi], mf.mv[1][qi]);
        let same = (rp0 == rq0 && rp1 == rq1) || (rp0 == rq1 && rp1 == rq0);
        if !same {
            return 1;
        }
        let strv = if rp0 != rp1 {
            if rp0 == rq0 {
                mv_diff(pm0, qm0) || mv_diff(pm1, qm1)
            } else {
                mv_diff(pm0, qm1) || mv_diff(pm1, qm0)
            }
        } else {
            (mv_diff(pm0, qm0) || mv_diff(pm1, qm1)) && (mv_diff(pm0, qm1) || mv_diff(pm1, qm0))
        };
        strv as usize
    };
    let at = |p: &[u8], stride: usize, x: usize, yy: usize| p[yy * stride + x] as i32;

    let (y, cb, cr) = (&mut frame.y, &mut frame.cb, &mut frame.cr);
    for mby in 0..height {
        for mbx in 0..width {
            let addr = mby * width + mbx;
            let qp_q = qp_grid[addr];
            let t8 = frame.mb_info[addr].transform8x8;

            for &horiz in &[false, true] {
                for ei in 0..4usize {
                    let ofs = ei * 4;
                    let at_pic_edge = if horiz { mby == 0 } else { mbx == 0 };
                    if ofs == 0 && at_pic_edge {
                        continue;
                    }
                    if ofs != 0 && t8 && (ofs == 4 || ofs == 12) {
                        continue;
                    }
                    let qp_p = if ofs != 0 {
                        qp_q
                    } else if horiz {
                        qp_grid[addr - width]
                    } else {
                        qp_grid[addr - 1]
                    };
                    let qpav = (qp_p + qp_q + 1) >> 1;
                    let ia = (qpav + off_a).clamp(0, 51) as usize;
                    let ib = (qpav + off_b).clamp(0, 51) as usize;
                    let (alpha, beta) = (ALPHA[ia], BETA[ib]);
                    if alpha == 0 {
                        continue;
                    }
                    for seg in 0..4usize {
                        let (qbx, qby) = if horiz { (mbx * 4 + seg, mby * 4 + ofs / 4) } else { (mbx * 4 + ofs / 4, mby * 4 + seg) };
                        let (pbx, pby) = if horiz { (qbx, qby - 1) } else { (qbx - 1, qby) };
                        let bs = bs_of(pby * bw4 + pbx, qby * bw4 + qbx, ofs == 0);
                        if bs == 0 {
                            continue;
                        }
                        for line in 0..4usize {
                            let t = seg * 4 + line;
                            let (bx, by) = if horiz { (mbx * 16 + t, mby * 16 + ofs) } else { (mbx * 16 + ofs, mby * 16 + t) };
                            let mut s = [0i32; 8];
                            for (k, sl) in s.iter_mut().enumerate() {
                                *sl = if horiz { at(y, fw, bx, by - 4 + k) } else { at(y, fw, bx - 4 + k, by) };
                            }
                            if bs == 4 {
                                filter_luma_strong(&mut s, alpha, beta);
                            } else {
                                filter_luma_normal(&mut s, alpha, beta, TC0[ia][bs - 1]);
                            }
                            for (k, &v) in s.iter().enumerate() {
                                let (px, py) = if horiz { (bx, by - 4 + k) } else { (bx - 4 + k, by) };
                                y[py * fw + px] = v.clamp(0, 255) as u8;
                            }
                        }
                    }
                }
            }

            let cqp_q = chroma_qp_jm(qp_q, chroma_off);
            for plane in [&mut *cb, &mut *cr] {
                for &horiz in &[false, true] {
                    for ce in 0..2usize {
                        let cofs = ce * 4;
                        let lofs = ce * 8;
                        let at_pic_edge = if horiz { mby == 0 } else { mbx == 0 };
                        if cofs == 0 && at_pic_edge {
                            continue;
                        }
                        let qp_p_y = if cofs != 0 {
                            qp_q
                        } else if horiz {
                            qp_grid[addr - width]
                        } else {
                            qp_grid[addr - 1]
                        };
                        let cqp_p = chroma_qp_jm(qp_p_y, chroma_off);
                        let qpav = (cqp_p + cqp_q + 1) >> 1;
                        let ia = (qpav + off_a).clamp(0, 51) as usize;
                        let ib = (qpav + off_b).clamp(0, 51) as usize;
                        let (alpha, beta) = (ALPHA[ia], BETA[ib]);
                        if alpha == 0 {
                            continue;
                        }
                        for t in 0..8usize {
                            let seg = t / 2;
                            let (qbx, qby) = if horiz { (mbx * 4 + seg, mby * 4 + lofs / 4) } else { (mbx * 4 + lofs / 4, mby * 4 + seg) };
                            let (pbx, pby) = if horiz { (qbx, qby - 1) } else { (qbx - 1, qby) };
                            let bs = bs_of(pby * bw4 + pbx, qby * bw4 + qbx, cofs == 0);
                            if bs == 0 {
                                continue;
                            }
                            let (bx, by) = if horiz { (mbx * 8 + t, mby * 8 + cofs) } else { (mbx * 8 + cofs, mby * 8 + t) };
                            let mut s = [0i32; 8];
                            for (k, sl) in s.iter_mut().enumerate() {
                                *sl = if horiz { at(plane, cw, bx, by - 4 + k) } else { at(plane, cw, bx - 4 + k, by) };
                            }
                            let tc0 = if bs == 4 { 0 } else { TC0[ia][bs - 1] };
                            filter_chroma(&mut s, alpha, beta, tc0, bs == 4);
                            for (k, &v) in s.iter().enumerate() {
                                let (px, py) = if horiz { (bx, by - 4 + k) } else { (bx - 4 + k, by) };
                                plane[py * cw + px] = v.clamp(0, 255) as u8;
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod temporal_direct_tests {
    use super::*;

    // One 4×4 co-located block with L0 motion `mv` referencing POC `refpoc`.
    fn col1(mv: (i32, i32), refpoc: i32) -> MotionField {
        MotionField { mv: vec![mv], refidx: vec![0], refpoc: vec![refpoc], nz: vec![false], bw4: 1, bh4: 1 }
    }

    #[test]
    fn temporal_scales_by_poc_distance() {
        // colPic (L1[0]) at POC 8 references POC 0; current B at POC 2.
        // td = 8, tb = 2 → mvL0 = mvCol/4, mvL1 = mvL0 − mvCol.
        let col = col1((40, -20), 0);
        let poc = [vec![0], vec![8]];
        let (mv0, mv1, r0, r1, u0, u1) = temporal_direct(&col, 0, &poc, 2);
        assert_eq!(mv0, (10, -5));
        assert_eq!(mv1, (10 - 40, -5 + 20));
        assert_eq!((r0, r1), (0, 0));
        assert!(u0 && u1);
    }

    #[test]
    fn map_col_to_list0_matches_by_poc() {
        // L0 = [POC 4, POC 0]; the co-located block referenced POC 0 → refIdxL0 1.
        let col = col1((16, 0), 0);
        let poc = [vec![4, 0], vec![8]];
        let (_, _, r0, r1, ..) = temporal_direct(&col, 0, &poc, 2);
        assert_eq!((r0, r1), (1, 0));
    }

    #[test]
    fn intra_colocated_block_is_zero_motion() {
        // refIdxCol < 0 (intra) ⇒ both MVs zero, refIdxL0 = 0.
        let col = MotionField { mv: vec![(99, 99)], refidx: vec![-1], refpoc: vec![-1], nz: vec![false], bw4: 1, bh4: 1 };
        let poc = [vec![0], vec![8]];
        let (mv0, mv1, r0, _, u0, u1) = temporal_direct(&col, 0, &poc, 2);
        assert_eq!((mv0, mv1), ((0, 0), (0, 0)));
        assert_eq!(r0, 0);
        assert!(u0 && u1);
    }

    #[test]
    fn zero_poc_distance_uses_colocated_mv_directly() {
        // td == 0 (degenerate) ⇒ mvL0 = mvCol, mvL1 = 0.
        let col = col1((12, 8), 8);
        let poc = [vec![8], vec![8]]; // p0 == p1 == 8
        let (mv0, mv1, ..) = temporal_direct(&col, 0, &poc, 2);
        assert_eq!(mv0, (12, 8));
        assert_eq!(mv1, (0, 0));
    }
}
