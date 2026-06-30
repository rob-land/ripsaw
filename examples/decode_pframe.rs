// Full P-slice reconstruction: decode every MB (skip + P_16x16/16x8/8x16/8x8)
// with the per-4x4-block MV / mvd / refIdx grids, run MV prediction
// (median + directional) to recover each partition's MV = mvp + mvd, motion-
// compensate from the reference, add the inter residual, and diff the whole
// frame against JM's P-frame. Reference = JM's decoded IDR (inter_post.yuv
// frame 0), isolating the inter reconstruction.
//
//   cargo run --release --example decode_pframe -- inter.h264 inter_post.yuv inter_predeblock.bin

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
use ripsaw::mvc::slice_header::parse_slice_header;
use ripsaw::mvc::sps::{parse_seq_parameter_set_data, Sps};

/// (bx4, by4, w4, h4, directional) partition layout per mb_type (4x4 units).
fn partitions(mb_type: i64) -> Vec<(usize, usize, usize, usize, Option<Directional>)> {
    match mb_type {
        1 => vec![(0, 0, 4, 4, None)],                                              // P_16x16
        2 => vec![(0, 0, 4, 2, Some(Directional::Above)), (0, 2, 4, 2, Some(Directional::Left))], // P_16x8
        3 => vec![(0, 0, 2, 4, Some(Directional::Left)), (2, 0, 2, 4, Some(Directional::AboveRight))], // P_8x16
        _ => vec![],
    }
}

fn main() -> anyhow::Result<()> {
    let h264 = std::env::args().nth(1).unwrap();
    let ref_yuv = std::env::args().nth(2).unwrap();
    let jm_p = std::env::args().nth(3).unwrap();
    let data = std::fs::read(&h264)?;

    let mut sps: Option<Sps> = None;
    let mut pps: Option<Pps> = None;
    let mut idr_done = false;

    for nal in NalSplitter::new(&data) {
        if nal.is_empty() {
            continue;
        }
        let Ok((hdr, consumed)) = parse_nal_unit_header(nal) else { continue };
        let rbsp = extract_rbsp(&nal[consumed..]);
        match hdr.nal_unit_type {
            7 => sps = Some(parse_seq_parameter_set_data(&mut BitReader::new(&rbsp))?),
            8 => {
                let chroma = sps.as_ref().map(|s| s.chroma_format_idc).unwrap_or(1);
                pps = Some(parse_pic_parameter_set(&mut BitReader::new(&rbsp), chroma)?);
            }
            5 => idr_done = true,
            1 if idr_done => {
                let (sps, pps) = (sps.as_ref().unwrap(), pps.as_ref().unwrap());
                let width = sps.pic_width_in_mbs as usize;
                let (fw, fh) = (width * 16, sps.pic_height_in_map_units as usize * 16);
                let (cw, ch) = (fw / 2, fh / 2);
                let (bw4, bh4) = (fw / 4, fh / 4);

                let r = std::fs::read(&ref_yuv)?;
                let (ysz, csz) = (fw * fh, cw * ch);
                let ref_y = r[..ysz].to_vec();
                let ref_cb = r[ysz..ysz + csz].to_vec();
                let ref_cr = r[ysz + csz..ysz + 2 * csz].to_vec();
                let rpy = Plane { data: &ref_y, w: fw, h: fh };
                let rpcb = Plane { data: &ref_cb, w: cw, h: ch };
                let rpcr = Plane { data: &ref_cr, w: cw, h: ch };

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
                // Per-4x4-block grids.
                let mut g_mv = vec![(0i32, 0i32); bw4 * bh4];
                let mut g_mvd = vec![(0i32, 0i32); bw4 * bh4];
                let mut g_ref = vec![-1i32; bw4 * bh4]; // -1 = not decoded
                let mut skip_grid: Vec<bool> = Vec::new();
                let mut cbp_grid: Vec<CbpBits> = Vec::new();
                let mut cbpv: Vec<u8> = Vec::new();
                let mut t8grid: Vec<bool> = Vec::new();
                let mut last_dquant = 0;
                let mut addr = 0usize;

                loop {
                    let mbx = addr % width;
                    let mby = addr / width;
                    let (mbx4, mby4) = (mbx * 4, mby * 4);
                    let left_ns = if mbx != 0 { skip_grid.get(addr - 1).map(|&s| (!s) as u32).unwrap_or(0) } else { 0 };
                    let up_ns = if addr >= width { skip_grid.get(addr - width).map(|&s| (!s) as u32).unwrap_or(0) } else { 0 };
                    let (is_skip, _) = decode_mb_skip_flag(&mut e, &mut ctx, left_ns, up_ns);
                    skip_grid.push(is_skip);

                    // Neighbour accessor for the MV grid.
                    let nb = |g_mv: &[(i32, i32)], g_ref: &[i32], bx: i32, by: i32| -> Neighbour {
                        if bx < 0 || by < 0 || bx >= bw4 as i32 || by >= bh4 as i32 {
                            return None;
                        }
                        let i = by as usize * bw4 + bx as usize;
                        if g_ref[i] < 0 {
                            None
                        } else {
                            Some((g_mv[i].0, g_mv[i].1, g_ref[i]))
                        }
                    };

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
                        if e.decode_terminate() == 1 {
                            break;
                        }
                        addr += 1;
                        continue;
                    }

                    let mb_type = decode_inter_mb_type(&mut e, &mut ctx);
                    if std::env::var("DBG").is_ok() {
                        eprintln!("addr {addr}: coded mb_type {mb_type}");
                    }
                    // Build the partition list (P_8x8 expands per sub_mb_type).
                    let parts: Vec<(usize, usize, usize, usize, Option<Directional>)> = if mb_type == 4 {
                        let subs: Vec<i64> = (0..4).map(|_| decode_sub_mb_type(&mut e, &mut ctx)).collect();
                        if std::env::var("DBG").is_ok() {
                            eprintln!("addr {addr}: subs {subs:?}");
                        }
                        assert!(subs.iter().all(|&s| s == 0), "only P_L0_8x8 sub-partitions handled");
                        // 8x8 sub-blocks in raster order.
                        vec![(0, 0, 2, 2, None), (2, 0, 2, 2, None), (0, 2, 2, 2, None), (2, 2, 2, 2, None)]
                    } else {
                        partitions(mb_type)
                    };

                    // Decode each partition's mvd, predict MV, fill grids.
                    let mut part_mv = Vec::new();
                    for &(bx4, by4, w4, h4, dir) in &parts {
                        let (gx, gy) = (mbx4 + bx4, mby4 + by4);
                        // mvd context: neighbour-|mvd| sum, per component.
                        let lmvd = nb_mvd(&g_mvd, &g_ref, gx as i32 - 1, gy as i32, bw4);
                        let umvd = nb_mvd(&g_mvd, &g_ref, gx as i32, gy as i32 - 1, bw4);
                        let incx = mvd_ctx_inc(lmvd.0.abs() + umvd.0.abs());
                        let incy = mvd_ctx_inc(lmvd.1.abs() + umvd.1.abs());
                        let mvd = (decode_mvd_component(&mut e, &mut ctx, 0, incx) as i32, decode_mvd_component(&mut e, &mut ctx, 1, incy) as i32);
                        if std::env::var("DBG").is_ok() {
                            eprintln!("  part ({bx4},{by4}) mvd {mvd:?}");
                        }
                        // MV prediction.
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

                    // cbp / transform / qp / residual.
                    let up = if addr >= width { Some(cbpv[addr - width]) } else { None };
                    let left = if mbx != 0 { Some(cbpv[addr - 1]) } else { None };
                    let cbp = decode_cbp_ctx(&mut e, &mut ctx.cbp, up, left);
                    let mut transform8x8 = false;
                    if cbp & 0x0f != 0 && pps.transform_8x8_mode_flag {
                        // Transform-8x8 needs all sub-partitions >= 8x8; here always true.
                        // ctxIdxInc = (left.t8x8) + (up.t8x8).
                        let lt = if mbx != 0 { t8grid[addr - 1] as usize } else { 0 };
                        let ut = if addr >= width { t8grid[addr - width] as usize } else { 0 };
                        transform8x8 = e.decode_decision(&mut ctx.transform[lt + ut]) == 1;
                    }
                    if cbp != 0 {
                        decode_dquant_ctx(&mut e, &mut ctx.delta_qp, &mut last_dquant);
                    }
                    let info = MbInfo { i_nxn: false, transform8x8, c_ipred: 0, cbp: cbp as u8, i16_pred: 0 };
                    let mut rneigh = CbfNeighbours {
                        cur: CbpBits::default(),
                        left: if mbx != 0 { Some(cbp_grid[addr - 1]) } else { None },
                        up: if addr >= width { Some(cbp_grid[addr - width]) } else { None },
                    };
                    let mut sink = Vec::new();
                    let res = decode_mb_residual(&mut e, &mut rctx, &info, &mut rneigh, slice_qp + last_dquant, pps.chroma_qp_index_offset, true, &ripsaw::mvc::scaling::ScalingLists::flat(), &mut sink);
                    cbp_grid.push(rneigh.cur);
                    cbpv.push(cbp as u8);
                    t8grid.push(transform8x8);

                    // Reconstruct each partition: MC + residual.
                    for (bx4, by4, w4, h4, mv) in part_mv {
                        let (px, py) = (mbx * 16 + bx4 * 4, mby * 16 + by4 * 4);
                        recon_part(&mut y, &mut cb, &mut cr, &rpy, &rpcb, &rpcr, px, py, bx4 * 4, by4 * 4, w4 * 4, h4 * 4, mv, &res.luma, &res.cb, &res.cr, fw, cw);
                    }
                    if e.decode_terminate() == 1 {
                        break;
                    }
                    addr += 1;
                }

                // Diff vs JM's P-frame (deblock disabled -> pre-deblock = final).
                let jm = std::fs::read(&jm_p)?;
                let rows = (skip_grid.len().div_ceil(width)) * 16;
                let cmp = |name: &str, got: &[u8], w: usize, h: usize, off: usize| -> bool {
                    for yy in 0..h {
                        for xx in 0..w {
                            if got[yy * w + xx] != jm[off + yy * w + xx] {
                                eprintln!("✗ {name} mismatch at ({xx},{yy}): {} vs JM {}", got[yy * w + xx], jm[off + yy * w + xx]);
                                return false;
                            }
                        }
                    }
                    eprintln!("  ✓ {name}: {h}×{w} matches JM");
                    true
                };
                eprintln!("decoded {} MBs ({rows} luma rows)", skip_grid.len());
                let oy = cmp("Y", &y, fw, rows.min(fh), 0);
                let ou = cmp("U", &cb, cw, (rows / 2).min(ch), ysz);
                let ov = cmp("V", &cr, cw, (rows / 2).min(ch), ysz + csz);
                if oy && ou && ov {
                    eprintln!("✓ full P-slice reconstruction MATCHES JM exactly");
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
    if i >= g_ref.len() || g_ref[i] < 0 {
        (0, 0)
    } else {
        g_mvd[i]
    }
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
    // 4:2:0 chroma: half the luma region, chroma MV = luma MV.
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
