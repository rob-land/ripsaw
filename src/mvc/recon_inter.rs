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
use crate::mvc::mc::{mc_chroma, mc_luma, Plane};
use crate::mvc::mv::{predict_mv, predict_skip_mv, Directional, Neighbour};
use crate::mvc::pps::Pps;
use crate::mvc::recon::{reconstruct_intra_mb, Frame, Plane as OutPlane};
use crate::mvc::scaling::ScalingLists;
use crate::mvc::slice_header::parse_slice_header;
use crate::mvc::sps::Sps;

/// Per-4×4-block motion (list-0): MV, ref index (-1 = intra/none), and the
/// luma nonzero-coefficient flag (for the deblock bS). Co-located source for a
/// B-slice's spatial-direct colZeroFlag.
#[derive(Clone)]
pub struct MotionField {
    pub mv: Vec<(i32, i32)>,
    pub refidx: Vec<i32>,
    pub nz: Vec<bool>,
    pub bw4: usize,
    pub bh4: usize,
}

fn partitions(mb_type: i64) -> Vec<(usize, usize, usize, usize, Option<Directional>)> {
    match mb_type {
        1 => vec![(0, 0, 4, 4, None)],
        2 => vec![(0, 0, 4, 2, Some(Directional::Above)), (0, 2, 4, 2, Some(Directional::Left))],
        3 => vec![(0, 0, 2, 4, Some(Directional::Left)), (2, 0, 2, 4, Some(Directional::AboveRight))],
        _ => vec![],
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
pub fn decode_p_frame(slices: &[&[u8]], nal_ref_idc: u8, idr: bool, sps: &Sps, pps: &Pps, refs: &[&Frame]) -> anyhow::Result<(Frame, MotionField)> {
    let width = sps.pic_width_in_mbs as usize;
    let (fw, fh) = (width * 16, sps.pic_height_in_map_units as usize * 16);
    let (cw, ch) = (fw / 2, fh / 2);
    let (bw4, bh4) = (fw / 4, fh / 4);
    let (ysz, csz) = (fw * fh, cw * ch);

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

    let mut y = OutPlane::new(fw, fh);
    let mut cb = OutPlane::new(cw, ch);
    let mut cr = OutPlane::new(cw, ch);
    let mut g_mv = vec![(0i32, 0i32); bw4 * bh4];
    let mut g_mvd = vec![(0i32, 0i32); bw4 * bh4];
    let mut g_ref = vec![-1i32; bw4 * bh4];
    let mut nz = vec![false; bw4 * bh4];
    // Intra pred-mode grid (Some(2)=DC for inter/skip cells, so intra MBs in
    // the P-slice derive their pred modes correctly).
    let mut modes = vec![Some(2u8); bw4 * bh4];
    // Per-MB grids indexed by MB address (not decode/push order), so a slice
    // may start at any first_mb and a mid-slice desync leaves a detectable gap
    // rather than a misaligned index (see the coverage check below).
    let num_mbs = width * (fh / 16);
    let mut skip_grid = vec![false; num_mbs];
    let mut cbp_grid = vec![CbpBits::default(); num_mbs];
    let mut cbpv = vec![0u8; num_mbs];
    let mut mb_info = vec![MbInfo { i_nxn: false, transform8x8: false, c_ipred: 0, cbp: 0, i16_pred: 0 }; num_mbs];
    let mut qp_grid = vec![0i32; num_mbs];
    let mut decoded_mbs = 0usize;
    let mut sh_last = None;
    let _ = (ysz, csz);

    // Neighbour accessor: `None` only when the neighbour MB is truly
    // unavailable — out of frame or in an earlier slice (MB address <
    // the current slice's first MB). An available-but-intra neighbour returns
    // `Some((0, 0, -1))` (its grid cells hold mv 0, ref -1): the median MV
    // prediction treats it as a zero-MV non-match, and — crucially — the P_Skip
    // zero-condition and the "B,C unavailable → A" rule (§8.4.1.1 / §8.4.1.3.2)
    // must distinguish "MB not available" from "MB is intra".
    let nb = |g_mv: &[(i32, i32)], g_ref: &[i32], bx: i32, by: i32, slice_start: usize| -> Neighbour {
        if bx < 0 || by < 0 || bx >= bw4 as i32 || by >= bh4 as i32 {
            return None;
        }
        if (by as usize / 4) * width + (bx as usize / 4) < slice_start {
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
        let mut rctx = ResidualContexts::new(slice_qp, true);
        let num_ref = (sh.num_ref_idx_l0_active_minus1 + 1) as usize;
        let mut last_dquant = 0;
        let mut qp = slice_qp;
        let mut addr = slice_start;
        sh_last = Some(sh);

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
            let a = nb(&g_mv, &g_ref, mbx4 as i32 - 1, mby4 as i32, slice_start);
            let b = nb(&g_mv, &g_ref, mbx4 as i32, mby4 as i32 - 1, slice_start);
            let c = {
                let cc = nb(&g_mv, &g_ref, mbx4 as i32 + 4, mby4 as i32 - 1, slice_start);
                if cc.is_some() { cc } else { nb(&g_mv, &g_ref, mbx4 as i32 - 1, mby4 as i32 - 1, slice_start) }
            };
            let mv = predict_skip_mv(a, b, c);
            fill(&mut g_mv, &mut g_ref, &mut g_mvd, mbx4, mby4, 4, 4, mv, (0, 0), 0, bw4);
            recon_part(&mut y.d, &mut cb.d, &mut cr.d, &ref_planes[0], mbx * 16, mby * 16, 0, 0, 16, 16, mv, &[[0i32; 16]; 16], &[[0i32; 8]; 8], &[[0i32; 8]; 8], fw, cw);
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
                let lt = if mbx != 0 { mb_info[addr - 1].transform8x8 as usize } else { 0 };
                let ut = if mb_top { mb_info[addr - width].transform8x8 as usize } else { 0 };
                info.transform8x8 = e.decode_decision(&mut ctx.transform[lt + ut]) == 1;
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
            let mut sink = Vec::new();
            // Intra residual (is_inter=false → cbf default_bit 1, I_16x16 DC path).
            let res = decode_mb_residual(&mut e, &mut rctx, &info, &mut rneigh, qp, pps.chroma_qp_index_offset, false, &scaling, &mut sink);
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
        let (parts, ngroups): (Vec<(usize, usize, usize, usize, Option<Directional>, usize)>, usize) = if mb_type == 4 {
            let subs: Vec<i64> = (0..4).map(|_| decode_sub_mb_type(&mut e, &mut ctx)).collect();
            let mut parts = Vec::new();
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
            (parts, 4)
        } else {
            let base = partitions(mb_type);
            let n = base.len();
            (base.into_iter().enumerate().map(|(i, (a, b, c, d, e))| (a, b, c, d, e, i)).collect(), n)
        };

        // ref_idx_l0 per group (before the mvds — § 7.3.5.1), when > 1 L0
        // reference. Fill g_ref so later groups'/MBs' ref_idx contexts + MV
        // prediction see it.
        let mut group_ref = vec![0i32; ngroups];
        if num_ref > 1 {
            for g in 0..ngroups {
                let &(bx4, by4, ..) = parts.iter().find(|p| p.5 == g).unwrap();
                let (gx, gy) = (mbx4 + bx4, mby4 + by4);
                let la = nref(&g_ref, gx as i32 - 1, gy as i32, slice_start, width, bw4);
                let ub = nref(&g_ref, gx as i32, gy as i32 - 1, slice_start, width, bw4);
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

        let mut part_mv = Vec::new();
        for &(bx4, by4, w4, h4, dir, group) in &parts {
            let ridx = group_ref[group];
            let (gx, gy) = (mbx4 + bx4, mby4 + by4);
            let lmvd = nb_mvd(&g_mvd, &g_ref, gx as i32 - 1, gy as i32, bw4, width, slice_start);
            let umvd = nb_mvd(&g_mvd, &g_ref, gx as i32, gy as i32 - 1, bw4, width, slice_start);
            let incx = mvd_ctx_inc(lmvd.0.abs() + umvd.0.abs());
            let incy = mvd_ctx_inc(lmvd.1.abs() + umvd.1.abs());
            let mvd = (decode_mvd_component(&mut e, &mut ctx, 0, incx) as i32, decode_mvd_component(&mut e, &mut ctx, 1, incy) as i32);
            let a = nb(&g_mv, &g_ref, gx as i32 - 1, gy as i32, slice_start);
            let b = nb(&g_mv, &g_ref, gx as i32, gy as i32 - 1, slice_start);
            let c = {
                let cc = nb(&g_mv, &g_ref, gx as i32 + w4 as i32, gy as i32 - 1, slice_start);
                if cc.is_some() { cc } else { nb(&g_mv, &g_ref, gx as i32 - 1, gy as i32 - 1, slice_start) }
            };
            let mvp = predict_mv(a, b, c, ridx, dir);
            let mv = (mvp.0 + mvd.0, mvp.1 + mvd.1);
            fill(&mut g_mv, &mut g_ref, &mut g_mvd, gx, gy, w4, h4, mv, mvd, ridx, bw4);
            part_mv.push((bx4, by4, w4, h4, mv, ridx));
        }

        let up = if mb_top { Some(cbpv[addr - width]) } else { None };
        let left = if mbx != 0 { Some(cbpv[addr - 1]) } else { None };
        let cbp = decode_cbp_ctx(&mut e, &mut ctx.cbp, up, left);
        let mut transform8x8 = false;
        if cbp & 0x0f != 0 && pps.transform_8x8_mode_flag {
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
        let mut sink = Vec::new();
        let res = decode_mb_residual(&mut e, &mut rctx, &info, &mut rneigh, qp, pps.chroma_qp_index_offset, true, &scaling, &mut sink);
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

        for (bx4, by4, w4, h4, mv, ridx) in part_mv {
            let (px, py) = (mbx * 16 + bx4 * 4, mby * 16 + by4 * 4);
            recon_part(&mut y.d, &mut cb.d, &mut cr.d, &ref_planes[ridx as usize], px, py, bx4 * 4, by4 * 4, w4 * 4, h4 * 4, mv, &res.luma, &res.cb, &res.cr, fw, cw);
        }
        if e.decode_terminate() == 1 {
            break;
        }
        addr += 1;
    }
    } // per-slice loop

    // Every MB must be decoded exactly once. A CABAC desync that ends a slice
    // early (or a stream feature that skips MBs) leaves a gap — bail cleanly so
    // the caller falls back, rather than emitting a frame with undecoded holes.
    anyhow::ensure!(decoded_mbs == num_mbs, "slice desync — {decoded_mbs}/{num_mbs} MBs decoded (unsupported feature?)");

    let sh = sh_last.expect("at least one slice");
    let frame = Frame {
        y: y.d,
        cb: cb.d,
        cr: cr.d,
        fw,
        fh,
        cw,
        ch,
        width_mbs: width,
        mb_info,
        qp: qp_grid,
        disable_deblock_idc: sh.disable_deblocking_filter_idc,
        slice_alpha_c0_offset_div2: sh.slice_alpha_c0_offset_div2,
        slice_beta_offset_div2: sh.slice_beta_offset_div2,
    };
    let mf = MotionField { mv: g_mv, refidx: g_ref, nz, bw4, bh4 };
    Ok((frame, mf))
}

/// Per-4×4-block, two-list motion for a B-frame: per list an MV and the POC of
/// the referenced picture (`-1` = list unused); plus the intra flag and the
/// luma nonzero-coefficient flag. Drives the two-list deblock bS ([`deblock_b`])
/// and could serve as a co-located source (B-frames here are non-reference).
pub struct BMotionField {
    pub mv: [Vec<(i32, i32)>; 2],
    pub refpoc: [Vec<i32>; 2],
    pub intra: Vec<bool>,
    pub nz: Vec<bool>,
    pub bw4: usize,
    pub bh4: usize,
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

fn min_positive(a: i32, b: i32) -> i32 {
    if a >= 0 && b >= 0 {
        a.min(b)
    } else {
        a.max(b)
    }
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
        let mv0 = if self.zero || self.ref0 < 0 || colzero { (0, 0) } else { self.mvp0 };
        let mv1 = if self.zero || self.ref1 < 0 || colzero { (0, 0) } else { self.mvp1 };
        (mv0, mv1, self.ref0 >= 0, self.ref1 >= 0)
    }
}

/// B-frame two-list per-4×4 motion grids (MV/refIdx/mvd per list), with
/// cross-slice-aware neighbour access.
struct BGrids {
    mv: [Vec<(i32, i32)>; 2],
    refi: [Vec<i32>; 2],
    mvd: [Vec<(i32, i32)>; 2],
    bw4: usize,
    bh4: usize,
    width: usize,
}
impl BGrids {
    fn nb(&self, list: usize, bx: i32, by: i32, slice_start: usize) -> Neighbour {
        if bx < 0 || by < 0 || bx >= self.bw4 as i32 || by >= self.bh4 as i32 {
            return None;
        }
        if (by as usize / 4) * self.width + (bx as usize / 4) < slice_start {
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
    fn spatial_direct(&self, mbx4: usize, mby4: usize, slice_start: usize) -> Direct {
        let (x, yy) = (mbx4 as i32, mby4 as i32);
        let mut d = Direct { ref0: -1, ref1: -1, mvp0: (0, 0), mvp1: (0, 0), zero: false };
        for list in 0..2 {
            let a = self.nb(list, x - 1, yy, slice_start);
            let b = self.nb(list, x, yy - 1, slice_start);
            let c = {
                let cc = self.nb(list, x + 4, yy - 1, slice_start);
                if cc.is_some() { cc } else { self.nb(list, x - 1, yy - 1, slice_start) }
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
#[allow(clippy::too_many_arguments)]
pub fn decode_b_frame(
    slices: &[&[u8]],
    nal_ref_idc: u8,
    idr: bool,
    sps: &Sps,
    pps: &Pps,
    l0: &[(&Frame, i32)],
    l1: &[(&Frame, i32)],
    col: &MotionField,
    bipred: (i32, i32),
) -> anyhow::Result<(Frame, BMotionField)> {
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

    let mut y = OutPlane::new(fw, fh);
    let mut cb = OutPlane::new(cw, ch);
    let mut cr = OutPlane::new(cw, ch);
    let mut g = BGrids {
        mv: [vec![(0, 0); bw4 * bh4], vec![(0, 0); bw4 * bh4]],
        refi: [vec![-1; bw4 * bh4], vec![-1; bw4 * bh4]],
        mvd: [vec![(0, 0); bw4 * bh4], vec![(0, 0); bw4 * bh4]],
        bw4,
        bh4,
        width,
    };
    // Per-4×4 deblock state (two-list ref POC + intra + nz).
    let mut refpoc = [vec![-1i32; bw4 * bh4], vec![-1i32; bw4 * bh4]];
    let mut intra_grid = vec![false; bw4 * bh4];
    let mut nz = vec![false; bw4 * bh4];
    // Intra pred-mode grid (Some(2)=DC seed for inter/skip cells).
    let mut modes = vec![Some(2u8); bw4 * bh4];
    // Per-MB grids indexed by MB address (see decode_p_frame).
    let num_mbs = width * (fh / 16);
    let mut skip_grid = vec![false; num_mbs];
    let mut mbtype_grid = vec![0i64; num_mbs];
    let mut cbp_grid = vec![CbpBits::default(); num_mbs];
    let mut cbpv = vec![0u8; num_mbs];
    let mut mb_info = vec![MbInfo { i_nxn: false, transform8x8: false, c_ipred: 0, cbp: 0, i16_pred: 0 }; num_mbs];
    let mut qp_grid = vec![0i32; num_mbs];
    let mut decoded_mbs = 0usize;
    let mut sh_last = None;

    for rbsp in slices {
        let mut sr = BitReader::new(rbsp);
        let sh = parse_slice_header(&mut sr, idr, nal_ref_idc, sps, pps)?;
        anyhow::ensure!(sh.direct_spatial_mv_pred_flag, "temporal direct not supported (need spatial)");
        anyhow::ensure!(sh.num_ref_idx_l0_active_minus1 == 0 && sh.num_ref_idx_l1_active_minus1 == 0, "B multi-ref (num_ref_idx > 1) not supported yet");
        let slice_qp = 26 + pps.pic_init_qp_minus26 + sh.slice_qp_delta;
        let idc = sh.cabac_init_idc.unwrap_or(0);
        let slice_start = sh.first_mb_in_slice as usize;
        let cabac_start = (sr.position_bits() + 7) / 8;
        let mut e = CabacEngine::new(&rbsp[cabac_start..]);
        let mut ctx = InterContexts::new(idc, slice_qp);
        let mut rctx = ResidualContexts::new(slice_qp, true);
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
                    let lt = if mbx != 0 { mb_info[addr - 1].transform8x8 as usize } else { 0 };
                    let ut = if mb_top { mb_info[addr - width].transform8x8 as usize } else { 0 };
                    info.transform8x8 = e.decode_decision(&mut ctx.transform[lt + ut]) == 1;
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
                let mut sink = Vec::new();
                let res = decode_mb_residual(&mut e, &mut rctx, &info, &mut rneigh, qp, pps.chroma_qp_index_offset, false, &scaling, &mut sink);
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
            let direct = g.spatial_direct(mbx4, mby4, slice_start);
            enum Plan {
                Direct { b8: usize },
                Explicit { pdir: u8, dir: Option<Directional>, mv0: (i32, i32), mv1: (i32, i32) },
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
                            plan.push((gx0 + dx, gy0 + dy, w4, h4, Plan::Explicit { pdir, dir: None, mv0: (0, 0), mv1: (0, 0) }));
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
                    plan.push((mbx4 + dx, mby4 + dy, pw4, ph4, Plan::Explicit { pdir: pdir[p], dir, mv0: (0, 0), mv1: (0, 0) }));
                }
            }

            // mvd for explicit partitions, list-major (all L0 then all L1).
            for list in 0..2usize {
                for (gx, gy, w4, h4, pk) in plan.iter_mut() {
                    if let Plan::Explicit { pdir, dir, mv0, mv1 } = pk {
                        if (*pdir as usize) != list && *pdir != 2 {
                            continue;
                        }
                        let lmvd = g.nb_mvd(list, *gx as i32 - 1, *gy as i32, slice_start);
                        let umvd = g.nb_mvd(list, *gx as i32, *gy as i32 - 1, slice_start);
                        let incx = mvd_ctx_inc(lmvd.0.abs() + umvd.0.abs());
                        let incy = mvd_ctx_inc(lmvd.1.abs() + umvd.1.abs());
                        let mvd = (decode_mvd_component(&mut e, &mut ctx, 0, incx) as i32, decode_mvd_component(&mut e, &mut ctx, 1, incy) as i32);
                        let a = g.nb(list, *gx as i32 - 1, *gy as i32, slice_start);
                        let b = g.nb(list, *gx as i32, *gy as i32 - 1, slice_start);
                        let c = {
                            let cc = g.nb(list, *gx as i32 + *w4 as i32, *gy as i32 - 1, slice_start);
                            if cc.is_some() { cc } else { g.nb(list, *gx as i32 - 1, *gy as i32 - 1, slice_start) }
                        };
                        let mvp = predict_mv(a, b, c, 0, *dir);
                        let mv = (mvp.0 + mvd.0, mvp.1 + mvd.1);
                        if list == 0 {
                            *mv0 = mv;
                        } else {
                            *mv1 = mv;
                        }
                        g.fill(list, *gx, *gy, *w4, *h4, mv, 0, mvd);
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
                let mut sink = Vec::new();
                res = decode_mb_residual(&mut e, &mut rctx, &info, &mut rneigh, qp, pps.chroma_qp_index_offset, true, &scaling, &mut sink);
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
                let (mv0, mv1, use0, use1) = match pk {
                    Plan::Direct { b8 } => {
                        let ccol = mbx4 + if b8 & 1 == 0 { 0 } else { 3 };
                        let crow = mby4 + if b8 >> 1 == 0 { 0 } else { 3 };
                        let cidx = crow * bw4 + ccol;
                        let colzero = col.refidx[cidx] == 0 && col.mv[cidx].0.abs() <= 1 && col.mv[cidx].1.abs() <= 1;
                        let (mv0, mv1, use0, use1) = direct.resolve(colzero);
                        g.fill(0, *gx, *gy, *w4, *h4, mv0, if use0 { 0 } else { -1 }, (0, 0));
                        g.fill(1, *gx, *gy, *w4, *h4, mv1, if use1 { 0 } else { -1 }, (0, 0));
                        (mv0, mv1, use0, use1)
                    }
                    Plan::Explicit { pdir, mv0, mv1, .. } => (*mv0, *mv1, *pdir == 0 || *pdir == 2, *pdir == 1 || *pdir == 2),
                };
                for j in 0..*h4 {
                    for i in 0..*w4 {
                        let cell = (*gy + j) * bw4 + *gx + i;
                        refpoc[0][cell] = if use0 { poc[0][0] } else { -1 };
                        refpoc[1][cell] = if use1 { poc[1][0] } else { -1 };
                    }
                }
                b_recon_block(&mut y.d, &mut cb.d, &mut cr.d, &pl, gx * 4, gy * 4, w4 * 4, h4 * 4, mv0, mv1, use0, use1, &res, fw, cw, bipred);
            }

            if e.decode_terminate() == 1 {
                break;
            }
            addr += 1;
        }
    }

    anyhow::ensure!(decoded_mbs == num_mbs, "slice desync — {decoded_mbs}/{num_mbs} MBs decoded (unsupported feature?)");

    let sh = sh_last.expect("at least one slice");
    let frame = Frame {
        y: y.d,
        cb: cb.d,
        cr: cr.d,
        fw,
        fh,
        cw,
        ch,
        width_mbs: width,
        mb_info,
        qp: qp_grid,
        disable_deblock_idc: sh.disable_deblocking_filter_idc,
        slice_alpha_c0_offset_div2: sh.slice_alpha_c0_offset_div2,
        slice_beta_offset_div2: sh.slice_beta_offset_div2,
    };
    let bmf = BMotionField { mv: g.mv, refpoc, intra: intra_grid, nz, bw4, bh4 };
    Ok((frame, bmf))
}

/// Reconstruct one B partition: uni- or bi-predicted MC (implicit weight `wt`,
/// `(32,32)` = default average) + residual, into the output planes.
#[allow(clippy::too_many_arguments)]
fn b_recon_block(
    y: &mut [u8], cb: &mut [u8], cr: &mut [u8],
    pl: &[Vec<(Plane, Plane, Plane)>; 2],
    px: usize, py: usize, w: usize, h: usize,
    mv0: (i32, i32), mv1: (i32, i32), use0: bool, use1: bool,
    res: &crate::mvc::mb_residual::MbResidual, fw: usize, cw: usize, wt: (i32, i32),
) {
    let comb = |a: &Option<Vec<u8>>, b: &Option<Vec<u8>>, k: usize| -> i32 {
        match (a, b) {
            (Some(a), Some(b)) => ((a[k] as i32 * wt.0 + b[k] as i32 * wt.1 + 32) >> 6).clamp(0, 255),
            (Some(a), None) => a[k] as i32,
            (None, Some(b)) => b[k] as i32,
            (None, None) => 0,
        }
    };
    let p0 = if use0 { Some(mc_luma(&pl[0][0].0, px as i32, py as i32, mv0.0, mv0.1, w, h)) } else { None };
    let p1 = if use1 { Some(mc_luma(&pl[1][0].0, px as i32, py as i32, mv1.0, mv1.1, w, h)) } else { None };
    let (rx, ry) = (px % 16, py % 16);
    for j in 0..h {
        for i in 0..w {
            let pred = comb(&p0, &p1, j * w + i);
            y[(py + j) * fw + px + i] = (pred + res.luma[ry + j][rx + i]).clamp(0, 255) as u8;
        }
    }
    let (cpx, cpy, cww, chh) = (px / 2, py / 2, w / 2, h / 2);
    let (crx, cry) = (cpx % 8, cpy % 8);
    for pi in 0..2usize {
        let rp0 = if pi == 0 { &pl[0][0].1 } else { &pl[0][0].2 };
        let rp1 = if pi == 0 { &pl[1][0].1 } else { &pl[1][0].2 };
        let plane: &mut [u8] = if pi == 0 { &mut *cb } else { &mut *cr };
        let c0 = if use0 { Some(mc_chroma(rp0, cpx as i32, cpy as i32, mv0.0, mv0.1, cww, chh)) } else { None };
        let c1 = if use1 { Some(mc_chroma(rp1, cpx as i32, cpy as i32, mv1.0, mv1.1, cww, chh)) } else { None };
        let resc = if pi == 0 { &res.cb } else { &res.cr };
        for j in 0..chh {
            for i in 0..cww {
                let pred = comb(&c0, &c1, j * cww + i);
                plane[(cpy + j) * cw + cpx + i] = (pred + resc[cry + j][crx + i]).clamp(0, 255) as u8;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn fill(g_mv: &mut [(i32, i32)], g_ref: &mut [i32], g_mvd: &mut [(i32, i32)], gx: usize, gy: usize, w4: usize, h4: usize, mv: (i32, i32), mvd: (i32, i32), ref_idx: i32, bw4: usize) {
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
fn nref(g_ref: &[i32], bx: i32, by: i32, slice_start: usize, width: usize, bw4: usize) -> i32 {
    if bx < 0 || by < 0 || (by as usize / 4) * width + (bx as usize / 4) < slice_start {
        return -1;
    }
    let i = by as usize * bw4 + bx as usize;
    if i >= g_ref.len() { -1 } else { g_ref[i] }
}

#[allow(clippy::too_many_arguments)]
fn nb_mvd(g_mvd: &[(i32, i32)], g_ref: &[i32], bx: i32, by: i32, bw4: usize, width: usize, slice_start: usize) -> (i32, i32) {
    if bx < 0 || by < 0 || (by as usize / 4) * width + (bx as usize / 4) < slice_start {
        return (0, 0);
    }
    let i = by as usize * bw4 + bx as usize;
    if i >= g_ref.len() || g_ref[i] < 0 { (0, 0) } else { g_mvd[i] }
}

#[allow(clippy::too_many_arguments)]
fn recon_part(
    y: &mut [u8], cb: &mut [u8], cr: &mut [u8],
    rp: &(Plane, Plane, Plane),
    px: usize, py: usize, rx: usize, ry: usize, w: usize, h: usize, mv: (i32, i32),
    luma: &[[i32; 16]; 16], rcb: &[[i32; 8]; 8], rcr: &[[i32; 8]; 8], fw: usize, cw: usize,
) {
    let (rpy, rpcb, rpcr) = (&rp.0, &rp.1, &rp.2);
    let pred = mc_luma(rpy, px as i32, py as i32, mv.0, mv.1, w, h);
    for j in 0..h {
        for i in 0..w {
            y[(py + j) * fw + px + i] = (pred[j * w + i] as i32 + luma[ry + j][rx + i]).clamp(0, 255) as u8;
        }
    }
    let (cpx, cpy, cww, chh, crx, cry) = (px / 2, py / 2, w / 2, h / 2, rx / 2, ry / 2);
    for (plane, res, rp) in [(&mut *cb, rcb, rpcb), (&mut *cr, rcr, rpcr)] {
        let p = mc_chroma(rp, cpx as i32, cpy as i32, mv.0, mv.1, cww, chh);
        for j in 0..chh {
            for i in 0..cww {
                plane[(cpy + j) * cw + cpx + i] = (p[j * cww + i] as i32 + res[cry + j][crx + i]).clamp(0, 255) as u8;
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
