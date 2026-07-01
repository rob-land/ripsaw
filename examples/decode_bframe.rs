// Full B-slice reconstruction, JM-pixel-free: libmvc decodes the IDR (POC 0)
// and the P-frame (POC 6) itself, then reconstructs the B-frame (POC 2) using
// its OWN deblocked I/P as the L0/L1 references — spatial direct (§ 8.4.1.2.2)
// for B_Skip / B_Direct (colZeroFlag against the P-frame's co-located motion
// field), explicit mvd + per-list MV prediction for coded partitions, and
// IMPLICIT weighted bi-prediction (§ 8.4.2.3.2, weighted_bipred_idc 2 — weights
// from the POC distances). Diffed pre-deblock against JM's per-frame dump
// (bframe_predeblock_all.bin frame 2).
//
//   cargo run --release --example decode_bframe -- bframe.h264 bframe_predeblock_all.bin

use ripsaw::mvc::annexb::NalSplitter;
use ripsaw::mvc::bitstream::BitReader;
use ripsaw::mvc::cabac::CabacEngine;
use ripsaw::mvc::mb_header::{decode_cbp_ctx, decode_dquant_ctx, MbInfo};
use ripsaw::mvc::mb_inter::{
    decode_b_mb_type, decode_b_sub_mb_type, decode_mb_skip_flag_b, decode_mvd_component, interpret_b_mb_type, mvd_ctx_inc,
    InterContexts,
};
use ripsaw::mvc::mb_residual::{decode_mb_residual, CbfNeighbours, CbpBits, ResidualContexts};
use ripsaw::mvc::mc::{mc_chroma, mc_luma, Plane};
use ripsaw::mvc::mv::{predict_mv, Directional, Neighbour};
use ripsaw::mvc::nal::parse_nal_unit_header;
use ripsaw::mvc::pps::{parse_pic_parameter_set, Pps};
use ripsaw::mvc::rbsp::extract_rbsp;
use ripsaw::mvc::recon::{decode_intra_frame, Frame};
use ripsaw::mvc::recon_inter::{deblock_inter, decode_p_frame, MotionField};
use ripsaw::mvc::slice_header::parse_slice_header;
use ripsaw::mvc::sps::{parse_seq_parameter_set_data, Sps};

fn sub_geom(s: i64) -> (u8, Vec<(usize, usize, usize, usize)>) {
    let pdir = match s {
        0 => 3,
        1 | 4 | 5 | 10 => 0,
        2 | 6 | 7 | 11 => 1,
        _ => 2,
    };
    let parts = match s {
        0 | 1 | 2 | 3 => vec![(0, 0, 2, 2)],
        4 | 6 | 8 => vec![(0, 0, 2, 1), (0, 1, 2, 1)],
        5 | 7 | 9 => vec![(0, 0, 1, 2), (1, 0, 1, 2)],
        _ => vec![(0, 0, 1, 1), (1, 0, 1, 1), (0, 1, 1, 1), (1, 1, 1, 1)],
    };
    (pdir, parts)
}

/// Per-4×4-block, per-list motion grids for the B-frame, plus the mvd grids
/// (for the mvd ctxIdxInc). list 0/1.
struct BGrids {
    mv: [Vec<(i32, i32)>; 2],
    refi: [Vec<i32>; 2],
    mvd: [Vec<(i32, i32)>; 2],
    bw4: usize,
    bh4: usize,
}
impl BGrids {
    fn nb(&self, list: usize, bx: i32, by: i32) -> Neighbour {
        if bx < 0 || by < 0 || bx >= self.bw4 as i32 || by >= self.bh4 as i32 {
            return None;
        }
        let i = by as usize * self.bw4 + bx as usize;
        if self.refi[list][i] < 0 { None } else { Some((self.mv[list][i].0, self.mv[list][i].1, self.refi[list][i])) }
    }
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
}

/// Implicit weighted bi-pred weights (§ 8.4.2.3.2). w0/w1 sum to 64; default
/// (32,32) when the scale factor is out of range or td == 0.
fn implicit_weights(poc0: i32, poc1: i32, cur: i32) -> (i32, i32) {
    let td = (poc1 - poc0).clamp(-128, 127);
    let tb = (cur - poc0).clamp(-128, 127);
    if td == 0 {
        return (32, 32);
    }
    let tx = (16384 + (td.abs() >> 1)) / td;
    let dsf = ((tb * tx + 32) >> 6).clamp(-1024, 1023);
    let w1 = dsf >> 2;
    if !(-64..=128).contains(&w1) {
        (32, 32)
    } else {
        (64 - w1, w1)
    }
}

fn min_positive(a: i32, b: i32) -> i32 {
    if a >= 0 && b >= 0 {
        a.min(b)
    } else {
        a.max(b)
    }
}

fn main() -> anyhow::Result<()> {
    let h264 = std::env::args().nth(1).unwrap();
    let jm_all = std::env::args().nth(2).unwrap();
    let data = std::fs::read(&h264)?;

    let mut sps: Option<Sps> = None;
    let mut pps: Option<Pps> = None;
    let mut refs: Vec<(i32, Frame)> = Vec::new(); // (POC, deblocked frame)
    let mut col: Option<MotionField> = None; // P-frame motion field (co-located)
    let mut slice_no = 0;

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
            5 => {
                let (sps, pps) = (sps.as_ref().unwrap(), pps.as_ref().unwrap());
                let mut f = decode_intra_frame(&[&rbsp[..]], hdr.nal_ref_idc, sps, pps)?;
                f.deblock_intra(pps.chroma_qp_index_offset);
                refs.push((0, f)); // IDR POC 0
                slice_no += 1;
            }
            1 => {
                let (sps, pps) = (sps.as_ref().unwrap(), pps.as_ref().unwrap());
                // Peek the slice type from the header.
                let mut sr = BitReader::new(&rbsp);
                let sh = parse_slice_header(&mut sr, false, hdr.nal_ref_idc, sps, pps)?;
                let poc = sh.pic_order_cnt_lsb.unwrap_or(0) as i32;
                if sh.slice_type % 5 == 0 {
                    // P-slice: decode against the most-recent ref (POC 0 IDR).
                    let reff = &refs.iter().max_by_key(|(p, _)| *p).unwrap().1;
                    let (mut pf, mf) = decode_p_frame(&rbsp, hdr.nal_ref_idc, sps, pps, reff)?;
                    deblock_inter(&mut pf, &mf, pps.chroma_qp_index_offset);
                    refs.push((poc, pf));
                    col = Some(mf);
                    slice_no += 1;
                } else {
                    // First B-slice (POC 2): reconstruct it.
                    eprintln!("B-slice POC {poc}: reconstructing");
                    let frame = reconstruct_b(&rbsp, &hdr, sps, pps, &refs, col.as_ref().unwrap())?;
                    let jm = std::fs::read(&jm_all)?;
                    let (ysz, csz) = (frame.fw * frame.fh, frame.cw * frame.ch);
                    let fsz = ysz + 2 * csz;
                    // bframe_predeblock_all.bin decode order: I,P,B(POC2),B(POC4).
                    let off = 2 * fsz; // frame 2 = first B
                    let ok = cmp("B Y", &frame.y, &jm, off) && cmp("B U", &frame.cb, &jm, off + ysz) && cmp("B V", &frame.cr, &jm, off + ysz + csz);
                    if ok {
                        eprintln!("✓ B-frame (POC {poc}) reconstruction matches JM pre-deblock ({}×{})", frame.fw, frame.fh);
                        eprintln!("✓ spatial direct + bi-prediction + explicit B partitions all bit-exact, zero JM pixels");
                    } else {
                        std::process::exit(1);
                    }
                    break;
                }
            }
            _ => {}
        }
    }
    let _ = slice_no;
    Ok(())
}

fn cmp(label: &str, got: &[u8], jm: &[u8], off: usize) -> bool {
    for (i, &g) in got.iter().enumerate() {
        if g != jm[off + i] {
            eprintln!("✗ {label} mismatch at byte {i} (x={},y={}): {g} vs JM {}", i % 128, i / 128, jm[off + i]);
            return false;
        }
    }
    true
}

#[allow(clippy::too_many_arguments)]
fn reconstruct_b(rbsp: &[u8], hdr: &ripsaw::mvc::nal::NalUnitHeader, sps: &Sps, pps: &Pps, refs: &[(i32, Frame)], col: &MotionField) -> anyhow::Result<Frame> {
    let width = sps.pic_width_in_mbs as usize;
    let (fw, fh) = (width * 16, sps.pic_height_in_map_units as usize * 16);
    let (cw, ch) = (fw / 2, fh / 2);
    let (bw4, bh4) = (fw / 4, fh / 4);
    let (ysz, csz) = (fw * fh, cw * ch);

    // Ref lists (single ref each, POC-based): L0[0] = nearest past (POC 0 IDR),
    // L1[0] = nearest future (POC 6 P-frame).
    let cur_poc = {
        let mut sr = BitReader::new(rbsp);
        let sh = parse_slice_header(&mut sr, false, hdr.nal_ref_idc, sps, pps)?;
        sh.pic_order_cnt_lsb.unwrap_or(0) as i32
    };
    let l0e = refs.iter().filter(|(p, _)| *p < cur_poc).max_by_key(|(p, _)| *p).expect("a past ref");
    let l1e = refs.iter().filter(|(p, _)| *p > cur_poc).min_by_key(|(p, _)| *p).expect("a future ref");
    let (poc0, poc1) = (l0e.0, l1e.0);
    let (l0, l1) = (&l0e.1, &l1e.1);
    // Implicit weighted bi-prediction weights (§ 8.4.2.3.2) from POC distances.
    let (w0, w1) = implicit_weights(poc0, poc1, cur_poc);
    let pl = [
        (Plane { data: &l0.y, w: fw, h: fh }, Plane { data: &l0.cb, w: cw, h: ch }, Plane { data: &l0.cr, w: cw, h: ch }),
        (Plane { data: &l1.y, w: fw, h: fh }, Plane { data: &l1.cb, w: cw, h: ch }, Plane { data: &l1.cr, w: cw, h: ch }),
    ];

    let mut sr = BitReader::new(rbsp);
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
    let mut g = BGrids {
        mv: [vec![(0, 0); bw4 * bh4], vec![(0, 0); bw4 * bh4]],
        refi: [vec![-1; bw4 * bh4], vec![-1; bw4 * bh4]],
        mvd: [vec![(0, 0); bw4 * bh4], vec![(0, 0); bw4 * bh4]],
        bw4,
        bh4,
    };
    let mut skip_grid: Vec<bool> = Vec::new();
    let mut mbtype_grid: Vec<i64> = Vec::new();
    let mut cbp_grid: Vec<CbpBits> = Vec::new();
    let mut cbpv: Vec<u8> = Vec::new();
    let mut t8grid: Vec<bool> = Vec::new();
    let mut last_dquant = 0;
    let mut qp = slice_qp;
    let mut addr = 0usize;

    loop {
        let (mbx, mby) = (addr % width, addr / width);
        let (mbx4, mby4) = (mbx * 4, mby * 4);
        let left_ns = if mbx != 0 { (!skip_grid[addr - 1]) as u32 } else { 0 };
        let up_ns = if addr >= width { (!skip_grid[addr - width]) as u32 } else { 0 };
        let (is_skip, _) = decode_mb_skip_flag_b(&mut e, &mut ctx, left_ns, up_ns);
        skip_grid.push(is_skip);

        // res accumulator (zero unless a coded MB fills it).
        let mut res = ripsaw::mvc::mb_residual::MbResidual::default();
        let mut transform8x8 = false;
        let mut cbp = 0i64;

        let mb_type = if is_skip { 0 } else { decode_b_mb_type(&mut e, &mut ctx, if mbx != 0 { (mbtype_grid[addr - 1] != 0) as u32 } else { 0 }, if addr >= width { (mbtype_grid[addr - width] != 0) as u32 } else { 0 }) };
        mbtype_grid.push(if is_skip { 0 } else { mb_type });
        assert!(mb_type <= 22, "intra-in-B not handled");

        // Derive spatial-direct (refIdxL0/L1, mvp, directZero) once per MB —
        // used by B_Skip / B_Direct_16x16 / B_Direct_8x8 subs.
        let direct = spatial_direct(&g, mbx4, mby4);

        // Build the partition plan: Vec<(gx,gy,w4,h4, kind)> where kind tells
        // how to get the MVs. For direct: use spatial-direct per-4×4. For
        // explicit: (pdir, mv per list already resolved).
        enum Plan {
            Direct { b8: usize }, // direct_8x8_inference: per-8×8, b8 = MB 8×8 idx
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
                let (pdir, parts) = sub_geom(s);
                if pdir == 3 {
                    plan.push((gx0, gy0, 2, 2, Plan::Direct { b8 }));
                } else {
                    // 8×8 sub-partitions use median prediction (dir = None).
                    for (dx, dy, w4, h4) in parts {
                        plan.push((gx0 + dx, gy0 + dy, w4, h4, Plan::Explicit { pdir, dir: None, mv0: (0, 0), mv1: (0, 0) }));
                    }
                }
            }
        } else {
            let (nparts, pw4, ph4, pdir) = interpret_b_mb_type(mb_type);
            for p in 0..nparts {
                let (dx, dy, dir) = if pw4 == 4 && ph4 == 2 {
                    (0, p * 2, Some(if p == 0 { Directional::Above } else { Directional::Left })) // 16×8
                } else if pw4 == 2 && ph4 == 4 {
                    (p * 2, 0, Some(if p == 0 { Directional::Left } else { Directional::AboveRight })) // 8×16
                } else {
                    (0, 0, None) // 16×16
                };
                plan.push((mbx4 + dx, mby4 + dy, pw4, ph4, Plan::Explicit { pdir: pdir[p], dir, mv0: (0, 0), mv1: (0, 0) }));
            }
        }

        // Read mvd for explicit partitions: list-major (L0 all, then L1).
        for list in 0..2usize {
            for (gx, gy, w4, h4, pk) in plan.iter_mut() {
                if let Plan::Explicit { pdir, dir, mv0, mv1 } = pk {
                    if (*pdir as usize) != list && *pdir != 2 {
                        continue;
                    }
                    let lmvd = nb_mvd(&g.mvd[list], &g.refi[list], *gx as i32 - 1, *gy as i32, bw4);
                    let umvd = nb_mvd(&g.mvd[list], &g.refi[list], *gx as i32, *gy as i32 - 1, bw4);
                    let incx = mvd_ctx_inc(lmvd.0.abs() + umvd.0.abs());
                    let incy = mvd_ctx_inc(lmvd.1.abs() + umvd.1.abs());
                    let mvd = (decode_mvd_component(&mut e, &mut ctx, 0, incx) as i32, decode_mvd_component(&mut e, &mut ctx, 1, incy) as i32);
                    let a = g.nb(list, *gx as i32 - 1, *gy as i32);
                    let b = g.nb(list, *gx as i32, *gy as i32 - 1);
                    let c = {
                        let cc = g.nb(list, *gx as i32 + *w4 as i32, *gy as i32 - 1);
                        if cc.is_some() { cc } else { g.nb(list, *gx as i32 - 1, *gy as i32 - 1) }
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

        if !is_skip {
            let up = if addr >= width { Some(cbpv[addr - width]) } else { None };
            let left = if mbx != 0 { Some(cbpv[addr - 1]) } else { None };
            cbp = decode_cbp_ctx(&mut e, &mut ctx.cbp, up, left);
            // transform_size_8x8 allowed if all sub-partitions >= 8×8.
            let all_ge_8 = mb_type != 22 || plan.iter().all(|(_, _, w4, h4, _)| *w4 >= 2 && *h4 >= 2);
            if cbp & 0x0f != 0 && pps.transform_8x8_mode_flag && all_ge_8 {
                let lt = if mbx != 0 { t8grid[addr - 1] as usize } else { 0 };
                let ut = if addr >= width { t8grid[addr - width] as usize } else { 0 };
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
                up: if addr >= width { Some(cbp_grid[addr - width]) } else { None },
            };
            let mut sink = Vec::new();
            res = decode_mb_residual(&mut e, &mut rctx, &info, &mut rneigh, qp, pps.chroma_qp_index_offset, true, &ripsaw::mvc::scaling::ScalingLists::flat(), &mut sink);
            cbp_grid.push(rneigh.cur);
            cbpv.push(cbp as u8);
            t8grid.push(transform8x8);
        } else {
            cbp_grid.push(CbpBits::default());
            cbpv.push(0);
            t8grid.push(false);
        }
        let _ = (cbp, transform8x8);

        // Reconstruct each partition.
        for (gx, gy, w4, h4, pk) in &plan {
            match pk {
                Plan::Direct { b8 } => {
                    // direct_8x8_inference: colZeroFlag from the MB's outer
                    // corner 4×4 of this 8×8 (b8), applied to the whole 8×8.
                    let ccol = mbx4 + if b8 & 1 == 0 { 0 } else { 3 };
                    let crow = mby4 + if b8 >> 1 == 0 { 0 } else { 3 };
                    let cidx = crow * bw4 + ccol;
                    let colzero = col.refidx[cidx] == 0 && col.mv[cidx].0.abs() <= 1 && col.mv[cidx].1.abs() <= 1;
                    let (mv0, mv1, use0, use1) = direct.resolve(colzero);
                    g.fill(0, *gx, *gy, *w4, *h4, mv0, if use0 { 0 } else { -1 }, (0, 0));
                    g.fill(1, *gx, *gy, *w4, *h4, mv1, if use1 { 0 } else { -1 }, (0, 0));
                    recon_block(&mut y, &mut cb, &mut cr, &pl, gx * 4, gy * 4, w4 * 4, h4 * 4, mv0, mv1, use0, use1, &res, fw, cw, (w0, w1));
                }
                Plan::Explicit { pdir, mv0, mv1, .. } => {
                    let (use0, use1) = (*pdir == 0 || *pdir == 2, *pdir == 1 || *pdir == 2);
                    recon_block(&mut y, &mut cb, &mut cr, &pl, gx * 4, gy * 4, w4 * 4, h4 * 4, *mv0, *mv1, use0, use1, &res, fw, cw, (w0, w1));
                }
            }
        }

        let eos = e.decode_terminate();
        if eos == 1 {
            break;
        }
        addr += 1;
    }

    Ok(Frame {
        y,
        cb,
        cr,
        fw,
        fh,
        cw,
        ch,
        width_mbs: width,
        mb_info: Vec::new(),
        qp: Vec::new(),
        disable_deblock_idc: sh.disable_deblocking_filter_idc,
        slice_alpha_c0_offset_div2: sh.slice_alpha_c0_offset_div2,
        slice_beta_offset_div2: sh.slice_beta_offset_div2,
    })
}

/// Spatial-direct derivation result (per MB). Holds refIdx + median predictor
/// per list; `resolve(colZeroFlag)` gives the per-4×4 MVs and pred flags.
struct Direct {
    ref0: i32,
    ref1: i32,
    mvp0: (i32, i32),
    mvp1: (i32, i32),
    zero: bool, // directZeroPredictionFlag
}
impl Direct {
    fn resolve(&self, colzero: bool) -> ((i32, i32), (i32, i32), bool, bool) {
        let mv0 = if self.zero || self.ref0 < 0 || colzero { (0, 0) } else { self.mvp0 };
        let mv1 = if self.zero || self.ref1 < 0 || colzero { (0, 0) } else { self.mvp1 };
        (mv0, mv1, self.ref0 >= 0, self.ref1 >= 0)
    }
}

fn spatial_direct(g: &BGrids, mbx4: usize, mby4: usize) -> Direct {
    let (x, yy) = (mbx4 as i32, mby4 as i32);
    let mut d = Direct { ref0: -1, ref1: -1, mvp0: (0, 0), mvp1: (0, 0), zero: false };
    for list in 0..2 {
        let a = g.nb(list, x - 1, yy);
        let b = g.nb(list, x, yy - 1);
        let c = {
            let cc = g.nb(list, x + 4, yy - 1);
            if cc.is_some() { cc } else { g.nb(list, x - 1, yy - 1) }
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

fn nb_mvd(g_mvd: &[(i32, i32)], g_ref: &[i32], bx: i32, by: i32, bw4: usize) -> (i32, i32) {
    if bx < 0 || by < 0 {
        return (0, 0);
    }
    let i = by as usize * bw4 + bx as usize;
    if i >= g_ref.len() || g_ref[i] < 0 { (0, 0) } else { g_mvd[i] }
}

#[allow(clippy::too_many_arguments)]
fn recon_block(
    y: &mut [u8], cb: &mut [u8], cr: &mut [u8],
    pl: &[(Plane, Plane, Plane); 2],
    px: usize, py: usize, w: usize, h: usize,
    mv0: (i32, i32), mv1: (i32, i32), use0: bool, use1: bool,
    res: &ripsaw::mvc::mb_residual::MbResidual, fw: usize, cw: usize, wt: (i32, i32),
) {
    // Luma prediction (uni, or implicit-weighted bi-pred).
    let p0 = if use0 { Some(mc_luma(&pl[0].0, px as i32, py as i32, mv0.0, mv0.1, w, h)) } else { None };
    let p1 = if use1 { Some(mc_luma(&pl[1].0, px as i32, py as i32, mv1.0, mv1.1, w, h)) } else { None };
    let (rx, ry) = (px % 16, py % 16);
    for j in 0..h {
        for i in 0..w {
            let pred = pred_sample(&p0, &p1, j * w + i, wt);
            y[(py + j) * fw + px + i] = (pred + res.luma[ry + j][rx + i]).clamp(0, 255) as u8;
        }
    }
    // Chroma (4:2:0).
    let (cpx, cpy, cww, chh) = (px / 2, py / 2, w / 2, h / 2);
    let (crx, cry) = (cpx % 8, cpy % 8);
    for plane_idx in 0..2usize {
        let rp0: &Plane = if plane_idx == 0 { &pl[0].1 } else { &pl[0].2 };
        let rp1: &Plane = if plane_idx == 0 { &pl[1].1 } else { &pl[1].2 };
        let plane: &mut [u8] = if plane_idx == 0 { &mut *cb } else { &mut *cr };
        let c0 = if use0 { Some(mc_chroma(rp0, cpx as i32, cpy as i32, mv0.0, mv0.1, cww, chh)) } else { None };
        let c1 = if use1 { Some(mc_chroma(rp1, cpx as i32, cpy as i32, mv1.0, mv1.1, cww, chh)) } else { None };
        let resc = if plane_idx == 0 { &res.cb } else { &res.cr };
        for j in 0..chh {
            for i in 0..cww {
                let pred = pred_sample(&c0, &c1, j * cww + i, wt);
                plane[(cpy + j) * cw + cpx + i] = (pred + resc[cry + j][crx + i]).clamp(0, 255) as u8;
            }
        }
    }
}

fn pred_sample(p0: &Option<Vec<u8>>, p1: &Option<Vec<u8>>, k: usize, wt: (i32, i32)) -> i32 {
    match (p0, p1) {
        // Implicit weighted bi-pred (§ 8.4.2.3.2): (L0·w0 + L1·w1 + 32) >> 6.
        (Some(a), Some(b)) => ((a[k] as i32 * wt.0 + b[k] as i32 * wt.1 + 32) >> 6).clamp(0, 255),
        (Some(a), None) => a[k] as i32,
        (None, Some(b)) => b[k] as i32,
        (None, None) => 0,
    }
}
