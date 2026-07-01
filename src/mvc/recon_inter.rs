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
    decode_inter_mb_type, decode_mb_skip_flag, decode_mvd_component, decode_ref_idx, decode_sub_mb_type, mvd_ctx_inc, InterContexts,
};
use crate::mvc::mb_residual::{decode_mb_residual, CbfNeighbours, CbpBits, ResidualContexts};
use crate::mvc::mc::{mc_chroma, mc_luma, Plane};
use crate::mvc::mv::{predict_mv, predict_skip_mv, Directional, Neighbour};
use crate::mvc::pps::Pps;
use crate::mvc::recon::Frame;
use crate::mvc::scaling::ScalingLists;
use crate::mvc::slice_header::parse_slice_header;
use crate::mvc::sps::Sps;

/// Per-4×4-block motion (list-0): MV, ref index (-1 = intra/none), and the
/// luma nonzero-coefficient flag (for the deblock bS). Co-located source for a
/// B-slice's spatial-direct colZeroFlag.
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
    let scaling = pps.scaling.clone().or_else(|| sps.scaling.clone()).unwrap_or_else(ScalingLists::flat);

    let mut y = vec![0u8; ysz];
    let mut cb = vec![0u8; csz];
    let mut cr = vec![0u8; csz];
    let mut g_mv = vec![(0i32, 0i32); bw4 * bh4];
    let mut g_mvd = vec![(0i32, 0i32); bw4 * bh4];
    let mut g_ref = vec![-1i32; bw4 * bh4];
    let mut nz = vec![false; bw4 * bh4];
    let mut skip_grid: Vec<bool> = Vec::new();
    let mut cbp_grid: Vec<CbpBits> = Vec::new();
    let mut cbpv: Vec<u8> = Vec::new();
    let mut mb_info: Vec<MbInfo> = Vec::new();
    let mut qp_grid: Vec<i32> = Vec::new();
    let mut sh_last = None;

    // Neighbour accessor: unavailable if out of frame, undecoded, or in an
    // earlier slice (its MB address < the current slice's first MB).
    let nb = |g_mv: &[(i32, i32)], g_ref: &[i32], bx: i32, by: i32, slice_start: usize| -> Neighbour {
        if bx < 0 || by < 0 || bx >= bw4 as i32 || by >= bh4 as i32 {
            return None;
        }
        if (by as usize / 4) * width + (bx as usize / 4) < slice_start {
            return None;
        }
        let i = by as usize * bw4 + bx as usize;
        if g_ref[i] < 0 { None } else { Some((g_mv[i].0, g_mv[i].1, g_ref[i])) }
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
        let mbx = addr % width;
        let mby = addr / width;
        let (mbx4, mby4) = (mbx * 4, mby * 4);
        let mb_top = addr >= width && addr - width >= slice_start;
        let left_ns = if mbx != 0 { (!skip_grid[addr - 1]) as u32 } else { 0 };
        let up_ns = if mb_top { (!skip_grid[addr - width]) as u32 } else { 0 };
        let (is_skip, _) = decode_mb_skip_flag(&mut e, &mut ctx, left_ns, up_ns);
        skip_grid.push(is_skip);

        if is_skip {
            let a = nb(&g_mv, &g_ref, mbx4 as i32 - 1, mby4 as i32, slice_start);
            let b = nb(&g_mv, &g_ref, mbx4 as i32, mby4 as i32 - 1, slice_start);
            let c = {
                let cc = nb(&g_mv, &g_ref, mbx4 as i32 + 4, mby4 as i32 - 1, slice_start);
                if cc.is_some() { cc } else { nb(&g_mv, &g_ref, mbx4 as i32 - 1, mby4 as i32 - 1, slice_start) }
            };
            let mv = predict_skip_mv(a, b, c);
            fill(&mut g_mv, &mut g_ref, &mut g_mvd, mbx4, mby4, 4, 4, mv, (0, 0), 0, bw4);
            recon_part(&mut y, &mut cb, &mut cr, &ref_planes[0], mbx * 16, mby * 16, 0, 0, 16, 16, mv, &[[0i32; 16]; 16], &[[0i32; 8]; 8], &[[0i32; 8]; 8], fw, cw);
            cbp_grid.push(CbpBits::default());
            cbpv.push(0);
            mb_info.push(MbInfo { i_nxn: false, transform8x8: false, c_ipred: 0, cbp: 0, i16_pred: 0 });
            qp_grid.push(qp);
            if e.decode_terminate() == 1 {
                break;
            }
            addr += 1;
            continue;
        }

        let mb_type = decode_inter_mb_type(&mut e, &mut ctx);
        let parts: Vec<(usize, usize, usize, usize, Option<Directional>)> = if mb_type == 4 {
            let subs: Vec<i64> = (0..4).map(|_| decode_sub_mb_type(&mut e, &mut ctx)).collect();
            assert!(subs.iter().all(|&s| s == 0), "only P_L0_8x8 sub-partitions handled");
            vec![(0, 0, 2, 2, None), (2, 0, 2, 2, None), (0, 2, 2, 2, None), (2, 2, 2, 2, None)]
        } else {
            partitions(mb_type)
        };

        // ref_idx_l0 for each partition (all before the mvds — § 7.3.5.1),
        // when more than one L0 reference. Fill g_ref so later partitions'
        // and MBs' ref_idx contexts + MV prediction see it.
        let mut part_ref: Vec<i32> = Vec::new();
        for &(bx4, by4, _, _, _) in &parts {
            let (gx, gy) = (mbx4 + bx4, mby4 + by4);
            let ridx = if num_ref > 1 {
                let la = nref(&g_ref, gx as i32 - 1, gy as i32, slice_start, width, bw4);
                let ub = nref(&g_ref, gx as i32, gy as i32 - 1, slice_start, width, bw4);
                decode_ref_idx(&mut e, &mut ctx, (la > 0) as u32, if ub > 0 { 2 } else { 0 }) as i32
            } else {
                0
            };
            part_ref.push(ridx);
            let (pw, ph) = parts.iter().find(|p| p.0 == bx4 && p.1 == by4).map(|p| (p.2, p.3)).unwrap();
            for j in 0..ph {
                for i in 0..pw {
                    g_ref[(gy + j) * bw4 + gx + i] = ridx;
                }
            }
        }

        let mut part_mv = Vec::new();
        for (pi, &(bx4, by4, w4, h4, dir)) in parts.iter().enumerate() {
            let ridx = part_ref[pi];
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
        cbp_grid.push(rneigh.cur);
        cbpv.push(cbp as u8);
        mb_info.push(info);
        qp_grid.push(qp);
        for by in 0..4u32 {
            for bx in 0..4u32 {
                if rneigh.cur.luma4x4_nonzero(bx, by) {
                    nz[(mby4 + by as usize) * bw4 + mbx4 + bx as usize] = true;
                }
            }
        }

        for (bx4, by4, w4, h4, mv, ridx) in part_mv {
            let (px, py) = (mbx * 16 + bx4 * 4, mby * 16 + by4 * 4);
            recon_part(&mut y, &mut cb, &mut cr, &ref_planes[ridx as usize], px, py, bx4 * 4, by4 * 4, w4 * 4, h4 * 4, mv, &res.luma, &res.cb, &res.cr, fw, cw);
        }
        if e.decode_terminate() == 1 {
            break;
        }
        addr += 1;
    }
    } // per-slice loop

    let sh = sh_last.expect("at least one slice");
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
        disable_deblock_idc: sh.disable_deblocking_filter_idc,
        slice_alpha_c0_offset_div2: sh.slice_alpha_c0_offset_div2,
        slice_beta_offset_div2: sh.slice_beta_offset_div2,
    };
    let mf = MotionField { mv: g_mv, refidx: g_ref, nz, bw4, bh4 };
    Ok((frame, mf))
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
    use crate::mvc::deblock::{filter_chroma, filter_luma_normal, ALPHA, BETA, TC0};
    let off_a = frame.slice_alpha_c0_offset_div2 * 2;
    let off_b = frame.slice_beta_offset_div2 * 2;
    let (fw, cw, width) = (frame.fw, frame.cw, frame.width_mbs);
    let bw4 = mf.bw4;
    let height = frame.qp.len() / width;
    let (mv, refg, nz) = (&mf.mv, &mf.refidx, &mf.nz);
    let qp_grid = &frame.qp;

    let bs_of = |pi: usize, qi: usize| -> usize {
        if nz[pi] || nz[qi] {
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
                        let bs = bs_of(pby * bw4 + pbx, qby * bw4 + qbx);
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
                            filter_luma_normal(&mut s, alpha, beta, TC0[ia][bs - 1]);
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
                            let bs = bs_of(pby * bw4 + pbx, qby * bw4 + qbx);
                            if bs == 0 {
                                continue;
                            }
                            let (bx, by) = if horiz { (mbx * 8 + t, mby * 8 + cofs) } else { (mbx * 8 + cofs, mby * 8 + t) };
                            let mut s = [0i32; 8];
                            for (k, sl) in s.iter_mut().enumerate() {
                                *sl = if horiz { at(plane, cw, bx, by - 4 + k) } else { at(plane, cw, bx - 4 + k, by) };
                            }
                            filter_chroma(&mut s, alpha, beta, TC0[ia][bs - 1], false);
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
