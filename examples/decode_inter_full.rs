// End-to-end, JM-pixel-free decode of inter.h264 (IDR + P-slice): libmvc
// decodes the base IDR itself via recon::decode_intra_frame, then uses ITS OWN
// reconstructed IDR as the reference frame for the P-slice motion
// compensation. Both frames are diffed against JM ground truth:
//   - IDR    vs inter_post.yuv frame 0
//   - P-slice vs inter_predeblock.bin  (== inter_post.yuv frame 1, deblock off)
// No JM pixels enter the decode — only the final comparison.
//
//   cargo run --release --example decode_inter_full -- inter.h264 inter_post.yuv inter_predeblock.bin

use ripsaw::mvc::annexb::NalSplitter;
use ripsaw::mvc::bitstream::BitReader;
use ripsaw::mvc::cabac::CabacEngine;
use ripsaw::mvc::mb_header::{decode_cbp_ctx, decode_dquant_ctx, MbInfo};
use ripsaw::mvc::mb_inter::{
    decode_inter_mb_type, decode_mb_skip_flag, decode_mvd_component, decode_sub_mb_type, mvd_ctx_inc, InterContexts,
};
use ripsaw::mvc::mb_residual::{decode_mb_residual, CbfNeighbours, CbpBits, ResidualContexts};
use ripsaw::mvc::mc::{mc_chroma, mc_luma, Plane};
use ripsaw::mvc::mv::{predict_mv, predict_skip_mv, Directional, Neighbour};
use ripsaw::mvc::nal::parse_nal_unit_header;
use ripsaw::mvc::pps::{parse_pic_parameter_set, Pps};
use ripsaw::mvc::rbsp::extract_rbsp;
use ripsaw::mvc::recon::{decode_intra_frame, Frame};
use ripsaw::mvc::slice_header::parse_slice_header;
use ripsaw::mvc::sps::{parse_seq_parameter_set_data, Sps};

fn partitions(mb_type: i64) -> Vec<(usize, usize, usize, usize, Option<Directional>)> {
    match mb_type {
        1 => vec![(0, 0, 4, 4, None)],
        2 => vec![(0, 0, 4, 2, Some(Directional::Above)), (0, 2, 4, 2, Some(Directional::Left))],
        3 => vec![(0, 0, 2, 4, Some(Directional::Left)), (2, 0, 2, 4, Some(Directional::AboveRight))],
        _ => vec![],
    }
}

fn main() -> anyhow::Result<()> {
    let h264 = std::env::args().nth(1).unwrap();
    let jm_yuv = std::env::args().nth(2).unwrap();
    let jm_p = std::env::args().nth(3).unwrap();
    let data = std::fs::read(&h264)?;

    let mut sps: Option<Sps> = None;
    let mut pps: Option<Pps> = None;
    let mut reference: Option<Frame> = None;

    for nal in NalSplitter::new(&data) {
        if nal.is_empty() {
            continue;
        }
        let Ok((hdr, consumed)) = parse_nal_unit_header(nal) else { continue };
        if hdr.mvc_extension.as_ref().map(|e| e.view_id != 0).unwrap_or(false) {
            continue;
        }
        let rbsp = extract_rbsp(&nal[consumed..]);
        match hdr.nal_unit_type {
            7 => sps = Some(parse_seq_parameter_set_data(&mut BitReader::new(&rbsp))?),
            8 => {
                let chroma = sps.as_ref().map(|s| s.chroma_format_idc).unwrap_or(1);
                pps = Some(parse_pic_parameter_set(&mut BitReader::new(&rbsp), chroma)?);
            }
            5 => {
                // libmvc decodes the base IDR itself.
                let (sps, pps) = (sps.as_ref().unwrap(), pps.as_ref().unwrap());
                let mut frame = decode_intra_frame(&rbsp, hdr.nal_ref_idc, sps, pps)?;
                // Deblock (§ 8.7) so the IDR is the final recon AND the correct
                // post-deblock MC reference for the P-slice.
                frame.deblock_intra(pps.chroma_qp_index_offset);
                // Validate our own IDR vs JM frame 0.
                let jm = std::fs::read(&jm_yuv)?;
                let (ysz, csz) = (frame.fw * frame.fh, frame.cw * frame.ch);
                let okp = |name: &str, got: &[u8], jmoff: usize| -> bool {
                    for (i, &g) in got.iter().enumerate() {
                        if g != jm[jmoff + i] {
                            eprintln!("✗ IDR {name} mismatch at byte {i}: {g} vs JM {}", jm[jmoff + i]);
                            return false;
                        }
                    }
                    true
                };
                let a = okp("Y", &frame.y, 0);
                let b = okp("U", &frame.cb, ysz);
                let c = okp("V", &frame.cr, ysz + csz);
                if a && b && c {
                    eprintln!("✓ IDR decoded by libmvc matches JM frame 0 ({}×{})", frame.fw, frame.fh);
                } else {
                    std::process::exit(1);
                }
                reference = Some(frame);
            }
            1 => {
                let (sps, pps) = (sps.as_ref().unwrap(), pps.as_ref().unwrap());
                let reff = reference.as_ref().expect("P-slice before IDR");
                let (fw, fh, cw, ch) = (reff.fw, reff.fh, reff.cw, reff.ch);
                let width = reff.width_mbs;
                let (bw4, bh4) = (fw / 4, fh / 4);
                let rpy = Plane { data: &reff.y, w: fw, h: fh };
                let rpcb = Plane { data: &reff.cb, w: cw, h: ch };
                let rpcr = Plane { data: &reff.cr, w: cw, h: ch };
                let (ysz, csz) = (fw * fh, cw * ch);

                let mut sr = BitReader::new(&rbsp);
                let sh = parse_slice_header(&mut sr, false, hdr.nal_ref_idc, sps, pps)?;
                let slice_qp = 26 + pps.pic_init_qp_minus26 + sh.slice_qp_delta;
                let idc = sh.cabac_init_idc.unwrap_or(0);
                let cabac_start = (sr.position_bits() + 7) / 8;
                let mut e = CabacEngine::new(&rbsp[cabac_start..]);
                let mut ctx = InterContexts::new(idc, slice_qp);
                let mut rctx = ResidualContexts::new(slice_qp, true);

                let mut y = vec![0u8; ysz];
                let mut cb = vec![0u8; csz];
                let mut cr = vec![0u8; csz];
                let mut g_mv = vec![(0i32, 0i32); bw4 * bh4];
                let mut g_mvd = vec![(0i32, 0i32); bw4 * bh4];
                let mut g_ref = vec![-1i32; bw4 * bh4];
                let mut skip_grid: Vec<bool> = Vec::new();
                let mut cbp_grid: Vec<CbpBits> = Vec::new();
                let mut cbpv: Vec<u8> = Vec::new();
                let mut t8grid: Vec<bool> = Vec::new();
                let mut qp_grid: Vec<i32> = Vec::new();
                let mut nz = vec![false; bw4 * bh4]; // per-4×4 luma nonzero-coeff
                let mut last_dquant = 0;
                let mut qp = slice_qp;
                let mut addr = 0usize;

                let nb = |g_mv: &[(i32, i32)], g_ref: &[i32], bx: i32, by: i32| -> Neighbour {
                    if bx < 0 || by < 0 || bx >= bw4 as i32 || by >= bh4 as i32 {
                        return None;
                    }
                    let i = by as usize * bw4 + bx as usize;
                    if g_ref[i] < 0 { None } else { Some((g_mv[i].0, g_mv[i].1, g_ref[i])) }
                };

                loop {
                    let mbx = addr % width;
                    let mby = addr / width;
                    let (mbx4, mby4) = (mbx * 4, mby * 4);
                    let left_ns = if mbx != 0 { skip_grid.get(addr - 1).map(|&s| (!s) as u32).unwrap_or(0) } else { 0 };
                    let up_ns = if addr >= width { skip_grid.get(addr - width).map(|&s| (!s) as u32).unwrap_or(0) } else { 0 };
                    let (is_skip, _) = decode_mb_skip_flag(&mut e, &mut ctx, left_ns, up_ns);
                    skip_grid.push(is_skip);

                    if is_skip {
                        let a = nb(&g_mv, &g_ref, mbx4 as i32 - 1, mby4 as i32);
                        let b = nb(&g_mv, &g_ref, mbx4 as i32, mby4 as i32 - 1);
                        let c = {
                            let cc = nb(&g_mv, &g_ref, mbx4 as i32 + 4, mby4 as i32 - 1);
                            if cc.is_some() { cc } else { nb(&g_mv, &g_ref, mbx4 as i32 - 1, mby4 as i32 - 1) }
                        };
                        let mv = predict_skip_mv(a, b, c);
                        fill(&mut g_mv, &mut g_ref, &mut g_mvd, mbx4, mby4, 4, 4, mv, (0, 0), bw4);
                        recon_part(&mut y, &mut cb, &mut cr, &rpy, &rpcb, &rpcr, mbx * 16, mby * 16, 0, 0, 16, 16, mv, &[[0i32; 16]; 16], &[[0i32; 8]; 8], &[[0i32; 8]; 8], fw, cw);
                        cbp_grid.push(CbpBits::default());
                        cbpv.push(0);
                        t8grid.push(false);
                        qp_grid.push(qp); // P_Skip: no residual, QP unchanged
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

                    let mut part_mv = Vec::new();
                    for &(bx4, by4, w4, h4, dir) in &parts {
                        let (gx, gy) = (mbx4 + bx4, mby4 + by4);
                        let lmvd = nb_mvd(&g_mvd, &g_ref, gx as i32 - 1, gy as i32, bw4);
                        let umvd = nb_mvd(&g_mvd, &g_ref, gx as i32, gy as i32 - 1, bw4);
                        let incx = mvd_ctx_inc(lmvd.0.abs() + umvd.0.abs());
                        let incy = mvd_ctx_inc(lmvd.1.abs() + umvd.1.abs());
                        let mvd = (decode_mvd_component(&mut e, &mut ctx, 0, incx) as i32, decode_mvd_component(&mut e, &mut ctx, 1, incy) as i32);
                        let a = nb(&g_mv, &g_ref, gx as i32 - 1, gy as i32);
                        let b = nb(&g_mv, &g_ref, gx as i32, gy as i32 - 1);
                        let c = {
                            let cc = nb(&g_mv, &g_ref, gx as i32 + w4 as i32, gy as i32 - 1);
                            if cc.is_some() { cc } else { nb(&g_mv, &g_ref, gx as i32 - 1, gy as i32 - 1) }
                        };
                        let mvp = predict_mv(a, b, c, 0, dir);
                        let mv = (mvp.0 + mvd.0, mvp.1 + mvd.1);
                        fill(&mut g_mv, &mut g_ref, &mut g_mvd, gx, gy, w4, h4, mv, mvd, bw4);
                        part_mv.push((bx4, by4, w4, h4, mv));
                    }

                    let up = if addr >= width { Some(cbpv[addr - width]) } else { None };
                    let left = if mbx != 0 { Some(cbpv[addr - 1]) } else { None };
                    let cbp = decode_cbp_ctx(&mut e, &mut ctx.cbp, up, left);
                    let mut transform8x8 = false;
                    if cbp & 0x0f != 0 && pps.transform_8x8_mode_flag {
                        let lt = if mbx != 0 { t8grid[addr - 1] as usize } else { 0 };
                        let ut = if addr >= width { t8grid[addr - width] as usize } else { 0 };
                        transform8x8 = e.decode_decision(&mut ctx.transform[lt + ut]) == 1;
                    }
                    // Running QP = prev QP + mb_qp_delta (mod 52). Absent delta
                    // (cbp==0) is 0; reset the dquant predictor too.
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
                        up: if addr >= width { Some(cbp_grid[addr - width]) } else { None },
                    };
                    let mut sink = Vec::new();
                    let res = decode_mb_residual(&mut e, &mut rctx, &info, &mut rneigh, qp, pps.chroma_qp_index_offset, true, &mut sink);
                    cbp_grid.push(rneigh.cur);
                    cbpv.push(cbp as u8);
                    t8grid.push(transform8x8);
                    qp_grid.push(qp);
                    // Record per-4×4 luma nonzero-coeff flags for the deblock bS.
                    for by in 0..4u32 {
                        for bx in 0..4u32 {
                            if rneigh.cur.luma4x4_nonzero(bx, by) {
                                nz[(mby4 + by as usize) * bw4 + mbx4 + bx as usize] = true;
                            }
                        }
                    }

                    for (bx4, by4, w4, h4, mv) in part_mv {
                        let (px, py) = (mbx * 16 + bx4 * 4, mby * 16 + by4 * 4);
                        recon_part(&mut y, &mut cb, &mut cr, &rpy, &rpcb, &rpcr, px, py, bx4 * 4, by4 * 4, w4 * 4, h4 * 4, mv, &res.luma, &res.cb, &res.cr, fw, cw);
                    }
                    if e.decode_terminate() == 1 {
                        break;
                    }
                    addr += 1;
                }

                // Pre-deblock check vs inter_predeblock.bin.
                let pre = std::fs::read(&jm_p)?;
                let cmp = |label: &str, got: &[u8], jm: &[u8], off: usize| -> bool {
                    for (i, &g) in got.iter().enumerate() {
                        if g != jm[off + i] {
                            eprintln!("✗ {label} mismatch at byte {i}: {g} vs JM {}", jm[off + i]);
                            return false;
                        }
                    }
                    true
                };
                let pre_ok = cmp("P pre-deblock Y", &y, &pre, 0) && cmp("P pre-deblock U", &cb, &pre, ysz) && cmp("P pre-deblock V", &cr, &pre, ysz + csz);
                if pre_ok {
                    eprintln!("✓ P-slice pre-deblock (ref = libmvc's own IDR) matches JM ({fw}×{fh})");
                } else {
                    std::process::exit(1);
                }

                // Inter in-loop deblock (§ 8.7, MV/ref/coded-block bS) → diff
                // vs JM's final P-frame (inter_post.yuv frame 1).
                if sh.disable_deblocking_filter_idc != 1 {
                    let off_a = sh.slice_alpha_c0_offset_div2 * 2;
                    let off_b = sh.slice_beta_offset_div2 * 2;
                    deblock_inter(&mut y, &mut cb, &mut cr, fw, cw, width, &g_mv, &g_ref, &nz, &qp_grid, &t8grid, pps.chroma_qp_index_offset, off_a, off_b);
                }
                let jm = std::fs::read(&jm_yuv)?;
                let foff = ysz + 2 * csz; // frame 1 (P-frame) offset
                let post_ok = cmp("P post-deblock Y", &y, &jm, foff) && cmp("P post-deblock U", &cb, &jm, foff + ysz) && cmp("P post-deblock V", &cr, &jm, foff + ysz + csz);
                if post_ok {
                    eprintln!("✓ P-slice post-deblock matches JM final P-frame ({fw}×{fh})");
                    eprintln!("✓ inter.h264 decoded END-TO-END by libmvc — both frames bit-exact incl. inter deblock, zero JM pixels");
                } else {
                    std::process::exit(1);
                }
                break;
            }
            _ => {}
        }
    }
    Ok(())
}

fn chroma_qp_jm(qpy: i32, offset: i32) -> i32 {
    const MAP: [i32; 22] = [29, 30, 31, 32, 32, 33, 34, 34, 35, 35, 36, 36, 37, 37, 37, 38, 38, 38, 39, 39, 39, 39];
    let qpi = (qpy + offset).clamp(0, 51);
    if qpi < 30 { qpi } else { MAP[(qpi - 30) as usize] }
}

/// Inter in-loop deblocking filter (§ 8.7) for a P-slice with no intra MBs.
/// bS is computed per 4-sample edge segment (§ 8.7.2.1): 2 if either side has
/// nonzero coeffs, 1 if refs differ or |Δmv| ≥ 4 (¼-pel) in either component,
/// else 0. (bS = 4/3 would apply only across intra MBs — none here.) Luma 4
/// edges × 4 segments; transform-8×8 MBs skip the internal x/y = 4,12 edges.
/// Chroma edges at 0,4 take the luma bS of the co-located luma edge (0,8).
#[allow(clippy::too_many_arguments)]
fn deblock_inter(
    y: &mut [u8], cb: &mut [u8], cr: &mut [u8],
    fw: usize, cw: usize, width: usize,
    mv: &[(i32, i32)], refg: &[i32], nz: &[bool],
    qp_grid: &[i32], t8grid: &[bool],
    chroma_off: i32, off_a: i32, off_b: i32,
) {
    use ripsaw::mvc::deblock::{filter_chroma, filter_luma_normal, ALPHA, BETA, TC0};
    let bw4 = fw / 4;
    let height = qp_grid.len() / width;
    let at = |p: &[u8], stride: usize, x: usize, yy: usize| p[yy * stride + x] as i32;

    // bS between 4×4 block p (across the edge) and q (this side). Inter-only.
    let bs_of = |pi: usize, qi: usize| -> usize {
        if nz[pi] || nz[qi] {
            2
        } else if refg[pi] != refg[qi]
            || (mv[pi].0 - mv[qi].0).abs() >= 4
            || (mv[pi].1 - mv[qi].1).abs() >= 4
        {
            1
        } else {
            0
        }
    };

    for mby in 0..height {
        for mbx in 0..width {
            let addr = mby * width + mbx;
            let qp_q = qp_grid[addr];
            let t8 = t8grid[addr];

            // ---- Luma ----
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
                        // q = this side, p = one 4×4 cell across the edge.
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

            // ---- Chroma (4:2:0): edges at chroma 0,4 ↔ luma 0,8 ----
            let cqp_q = chroma_qp_jm(qp_q, chroma_off);
            for (plane, _name) in [(&mut *cb, 'u'), (&mut *cr, 'v')] {
                for &horiz in &[false, true] {
                    for ce in 0..2usize {
                        let cofs = ce * 4;
                        let lofs = ce * 8; // co-located luma edge (0 or 8)
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
                            // Chroma sample t ↔ luma 2t; its segment = (2t)/4 = t/2.
                            let seg = t / 2;
                            let (qbx, qby) = if horiz {
                                (mbx * 4 + seg, mby * 4 + lofs / 4)
                            } else {
                                (mbx * 4 + lofs / 4, mby * 4 + seg)
                            };
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

#[allow(clippy::too_many_arguments)]
fn fill(g_mv: &mut [(i32, i32)], g_ref: &mut [i32], g_mvd: &mut [(i32, i32)], gx: usize, gy: usize, w4: usize, h4: usize, mv: (i32, i32), mvd: (i32, i32), bw4: usize) {
    for j in 0..h4 {
        for i in 0..w4 {
            let idx = (gy + j) * bw4 + gx + i;
            g_mv[idx] = mv;
            g_ref[idx] = 0;
            g_mvd[idx] = mvd;
        }
    }
}

fn nb_mvd(g_mvd: &[(i32, i32)], g_ref: &[i32], bx: i32, by: i32, bw4: usize) -> (i32, i32) {
    if bx < 0 || by < 0 {
        return (0, 0);
    }
    let i = by as usize * bw4 + bx as usize;
    if i >= g_ref.len() || g_ref[i] < 0 { (0, 0) } else { g_mvd[i] }
}

#[allow(clippy::too_many_arguments)]
fn recon_part(
    y: &mut [u8], cb: &mut [u8], cr: &mut [u8],
    rpy: &Plane, rpcb: &Plane, rpcr: &Plane,
    px: usize, py: usize, rx: usize, ry: usize, w: usize, h: usize, mv: (i32, i32),
    luma: &[[i32; 16]; 16], rcb: &[[i32; 8]; 8], rcr: &[[i32; 8]; 8], fw: usize, cw: usize,
) {
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
