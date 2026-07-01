//! Intra-frame reconstruction: bitstream → reconstructed (pre-deblock) YUV.
//!
//! Factored out of `examples/decode_frame_full` so libmvc can decode its own
//! base IDR (e.g. as the reference frame for a P-slice) instead of borrowing a
//! reference decoder's output. CABAC decode (`decode_mb_header` +
//! `decode_mb_residual`) + intra prediction + residual + clip, per MB, for
//! every intra MB type (I_4x4 / I_8x8 / I_16x16 + chroma). Validated
//! bit-exact vs JM `ldecod` for every MB type (see the example).
//!
//! Returns the per-MB `MbInfo`/QP grids alongside the planes so the caller can
//! run the in-loop deblocking filter (§ 8.7) afterwards.

use crate::mvc::bitstream::BitReader;
use crate::mvc::cabac::CabacEngine;
use crate::mvc::intra::{
    derive_intra_mode, predict_16x16, predict_4x4, predict_8x8, predict_chroma_8x8, Intra4x4Mode,
    Neighbors4x4, Neighbors8x8, NeighborsNxN, PlaneMode,
};
use crate::mvc::mb_header::{decode_mb_header, MbHeaderContexts, MbInfo, Neighbors};
use crate::mvc::mb_residual::{decode_mb_residual, CbfNeighbours, CbpBits, ResidualContexts};
use crate::mvc::pps::Pps;
use crate::mvc::scaling::ScalingLists;
use crate::mvc::slice_header::parse_slice_header;
use crate::mvc::sps::Sps;

/// A reconstructed frame plus the per-MB decode state the deblock needs.
pub struct Frame {
    pub y: Vec<u8>,
    pub cb: Vec<u8>,
    pub cr: Vec<u8>,
    pub fw: usize,
    pub fh: usize,
    pub cw: usize,
    pub ch: usize,
    pub width_mbs: usize,
    /// Per-MB header info (raster order).
    pub mb_info: Vec<MbInfo>,
    /// Per-MB luma QP (raster order).
    pub qp: Vec<i32>,
    /// Slice deblock parameters (idc, alpha/c0 offset, beta offset).
    pub disable_deblock_idc: u32,
    pub slice_alpha_c0_offset_div2: i32,
    pub slice_beta_offset_div2: i32,
}

struct Plane {
    d: Vec<u8>,
    w: usize,
}
impl Plane {
    fn at(&self, x: usize, y: usize) -> i32 {
        self.d[y * self.w + x] as i32
    }
    fn set(&mut self, x: usize, y: usize, v: i32) {
        self.d[y * self.w + x] = v.clamp(0, 255) as u8;
    }
}

/// Decode one intra (IDR or I) slice's RBSP into a reconstructed frame.
/// One IDR/I access unit's slices (`slices` in decode order, `first_mb_in_slice`
/// increasing). A single-slice frame is `&[rbsp]`; a multi-slice frame passes
/// all its slices. Slice boundaries break intra prediction (cross-slice
/// neighbours are unavailable). Pre-deblock.
pub fn decode_intra_frame(slices: &[&[u8]], nal_ref_idc: u8, idr: bool, sps: &Sps, pps: &Pps) -> anyhow::Result<Frame> {
    let width = sps.pic_width_in_mbs as usize;
    let (fw, fh) = (width * 16, sps.pic_height_in_map_units as usize * 16);
    let (cw, ch) = (fw / 2, fh / 2);
    // Inverse-quant scaling lists: PPS overrides SPS overrides flat.
    let scaling = pps.scaling.clone().or_else(|| sps.scaling.clone()).unwrap_or_else(ScalingLists::flat);

    let mut y = Plane { d: vec![0; fw * fh], w: fw };
    let mut cb = Plane { d: vec![0; cw * ch], w: cw };
    let mut cr = Plane { d: vec![0; cw * ch], w: cw };
    let bw4 = width * 4;
    let mut modes = vec![None::<u8>; bw4 * fh / 4];
    let mut grid: Vec<MbInfo> = Vec::new();
    let mut cbp_grid: Vec<CbpBits> = Vec::new();
    let mut qp_grid: Vec<i32> = Vec::new();
    let mut sink = Vec::new();
    // Deblock params come from the last slice header parsed (all equal here).
    let mut sh_last = None;

    for rbsp in slices {
        let mut r = BitReader::new(rbsp);
        let sh = parse_slice_header(&mut r, idr, nal_ref_idc, sps, pps)?;
        let slice_qp = 26 + pps.pic_init_qp_minus26 + sh.slice_qp_delta;
        let slice_start = sh.first_mb_in_slice as usize;
        let cabac_start = (r.position_bits() + 7) / 8;
        let mut e = CabacEngine::new(&rbsp[cabac_start..]);
        let mut hctx = MbHeaderContexts::new(slice_qp);
        let mut rctx = ResidualContexts::new(slice_qp, false);
        let mut last_dquant = 0;
        let mut qp = slice_qp;
        let mut addr = slice_start;
        sh_last = Some(sh);

    loop {
        let (mbx, mby) = (addr % width, addr / width);
        // Cross-slice neighbours (earlier slice) are unavailable. Slices are
        // MB-contiguous, so only the top neighbour can cross a boundary.
        let mb_top = addr >= width && addr - width >= slice_start;
        let left_i = if mbx != 0 { grid.get(addr - 1).copied() } else { None };
        let up_i = if mb_top { grid.get(addr - width).copied() } else { None };
        let mut header: Vec<(String, i64)> = Vec::new();
        let info = decode_mb_header(&mut e, &mut hctx, &Neighbors { left: left_i, up: up_i }, &mut last_dquant, &mut header);
        if let Some((_, d)) = header.iter().find(|(n, _)| n == "mb_qp_delta") {
            qp = (qp + *d as i32).rem_euclid(52);
        }
        let raw: Vec<i64> = header.iter().filter(|(n, _)| n == "intra4x4_pred_mode").map(|(_, v)| *v).collect();

        let mut neigh = CbfNeighbours {
            cur: CbpBits::default(),
            left: if mbx != 0 { cbp_grid.get(addr - 1).copied() } else { None },
            up: if mb_top { cbp_grid.get(addr - width).copied() } else { None },
        };
        let res = decode_mb_residual(&mut e, &mut rctx, &info, &mut neigh, qp, pps.chroma_qp_index_offset, false, &scaling, &mut sink);

        // ---- Luma reconstruction ----
        let (mpx, mpy) = (mbx * 16, mby * 16);
        if !info.i_nxn {
            let mode = PlaneMode::from_index(info.i16_pred as u32).unwrap();
            let n = gather_nxn(&y, mpx, mpy, 16, mb_top);
            let pred = predict_16x16(mode, &n);
            for yy in 0..16 {
                for xx in 0..16 {
                    y.set(mpx + xx, mpy + yy, pred[yy][xx] + res.luma[yy][xx]);
                }
            }
            for c in modes_cells(mby, mbx, bw4, 4) {
                modes[c] = Some(2);
            }
        } else if info.transform8x8 {
            for b8 in 0..4usize {
                let (bx8, by8) = (b8 & 1, b8 >> 1);
                let gbx = mbx * 2 + bx8;
                let gby = mby * 2 + by8;
                let (cx, cy) = (gbx * 2, gby * 2);
                let mode = derive_intra_mode(left_cell(&modes, cx, cy, bw4), up_cell(&modes, cx, cy, bw4, mb_top), raw[b8]);
                let n = gather_8x8(&y, gbx, gby, fw, bw4 / 2, seq8(gbx, gby, width), mb_top);
                let pred = predict_8x8(Intra4x4Mode::from_index(mode as u32).unwrap(), &n);
                for yy in 0..8 {
                    for xx in 0..8 {
                        y.set(gbx * 8 + xx, gby * 8 + yy, pred[yy][xx] + res.luma[by8 * 8 + yy][bx8 * 8 + xx]);
                    }
                }
                for sy in 0..2 {
                    for sx in 0..2 {
                        modes[(cy + sy) * bw4 + cx + sx] = Some(mode);
                    }
                }
            }
        } else {
            for region in 0..4usize {
                for sub in 0..4usize {
                    let bx = (region & 1) * 2 + (sub & 1);
                    let by = (region >> 1) * 2 + (sub >> 1);
                    let idx = region * 4 + sub;
                    let (cx, cy) = (mbx * 4 + bx, mby * 4 + by);
                    let mode = derive_intra_mode(left_cell(&modes, cx, cy, bw4), up_cell(&modes, cx, cy, bw4, mb_top), raw[idx]);
                    let n = gather_4x4(&y, cx, cy, fw, bw4, seq4(cx, cy, width), mb_top);
                    let pred = predict_4x4(Intra4x4Mode::from_index(mode as u32).unwrap(), &n);
                    for yy in 0..4 {
                        for xx in 0..4 {
                            y.set(cx * 4 + xx, cy * 4 + yy, pred[yy][xx] + res.luma[by * 4 + yy][bx * 4 + xx]);
                        }
                    }
                    modes[cy * bw4 + cx] = Some(mode);
                }
            }
        }

        // ---- Chroma reconstruction ----
        let cmode = match info.c_ipred {
            0 => PlaneMode::Dc,
            1 => PlaneMode::Horizontal,
            2 => PlaneMode::Vertical,
            _ => PlaneMode::Plane,
        };
        for (plane, cres) in [(&mut cb, &res.cb), (&mut cr, &res.cr)] {
            let n = gather_nxn(plane, mbx * 8, mby * 8, 8, mb_top);
            let pred = predict_chroma_8x8(cmode, &n);
            for yy in 0..8 {
                for xx in 0..8 {
                    plane.set(mbx * 8 + xx, mby * 8 + yy, pred[yy][xx] + cres[yy][xx]);
                }
            }
        }

        let eos = e.decode_terminate();
        grid.push(info);
        cbp_grid.push(neigh.cur);
        qp_grid.push(qp);
        if eos == 1 {
            break;
        }
        addr += 1;
    }
    } // per-slice loop

    let sh = sh_last.expect("at least one slice");
    Ok(Frame {
        y: y.d,
        cb: cb.d,
        cr: cr.d,
        fw,
        fh,
        cw,
        ch,
        width_mbs: width,
        mb_info: grid,
        qp: qp_grid,
        disable_deblock_idc: sh.disable_deblocking_filter_idc,
        slice_alpha_c0_offset_div2: sh.slice_alpha_c0_offset_div2,
        slice_beta_offset_div2: sh.slice_beta_offset_div2,
    })
}

impl Frame {
    /// Apply the in-loop deblocking filter (§ 8.7) in place, for an all-intra
    /// frame (bS = 4 on MB boundaries, 3 on internal edges; transform-8×8 MBs
    /// skip the internal 4×4 edges). After this the frame is the final
    /// (post-deblock) reconstruction — and the correct MC reference for a
    /// subsequent inter frame. No-op when the slice disabled deblocking.
    pub fn deblock_intra(&mut self, chroma_off: i32) {
        if self.disable_deblock_idc == 1 {
            return;
        }
        let off_a = self.slice_alpha_c0_offset_div2 * 2;
        let off_b = self.slice_beta_offset_div2 * 2;
        let (fw, cw, width) = (self.fw, self.cw, self.width_mbs);
        let mut y = Plane { d: std::mem::take(&mut self.y), w: fw };
        let mut cb = Plane { d: std::mem::take(&mut self.cb), w: cw };
        let mut cr = Plane { d: std::mem::take(&mut self.cr), w: cw };
        deblock_intra(&mut y, &mut cb, &mut cr, &self.mb_info, &self.qp, width, chroma_off, off_a, off_b);
        self.y = y.d;
        self.cb = cb.d;
        self.cr = cr.d;
    }
}

fn chroma_qp_jm(qpy: i32, offset: i32) -> i32 {
    const MAP: [i32; 22] = [29, 30, 31, 32, 32, 33, 34, 34, 35, 35, 36, 36, 37, 37, 37, 38, 38, 38, 39, 39, 39, 39];
    let qpi = (qpy + offset).clamp(0, 51);
    if qpi < 30 { qpi } else { MAP[(qpi - 30) as usize] }
}

#[allow(clippy::too_many_arguments)]
fn deblock_intra(y: &mut Plane, cb: &mut Plane, cr: &mut Plane, grid: &[MbInfo], qp_grid: &[i32], width: usize, chroma_off: i32, off_a: i32, off_b: i32) {
    use crate::mvc::deblock::{filter_chroma, filter_luma_normal, filter_luma_strong, ALPHA, BETA, TC0};
    let height = grid.len() / width;

    let luma_edge = |p: &mut Plane, horiz: bool, mbx: usize, mby: usize, ofs: usize, bs: usize, ia: usize, ib: usize| {
        let (alpha, beta) = (ALPHA[ia], BETA[ib]);
        if alpha == 0 {
            return;
        }
        for t in 0..16 {
            let mut s = [0i32; 8];
            let (bx, by) = if horiz { (mbx * 16 + t, mby * 16 + ofs) } else { (mbx * 16 + ofs, mby * 16 + t) };
            for (k, sl) in s.iter_mut().enumerate() {
                *sl = if horiz { p.at(bx, by - 4 + k) } else { p.at(bx - 4 + k, by) };
            }
            if bs == 4 {
                filter_luma_strong(&mut s, alpha, beta);
            } else {
                filter_luma_normal(&mut s, alpha, beta, TC0[ia][bs - 1]);
            }
            for (k, &v) in s.iter().enumerate() {
                if horiz {
                    p.set(bx, by - 4 + k, v);
                } else {
                    p.set(bx - 4 + k, by, v);
                }
            }
        }
    };

    for mby in 0..height {
        for mbx in 0..width {
            let addr = mby * width + mbx;
            let info = &grid[addr];
            let qp_q = qp_grid[addr];
            let t8 = info.transform8x8 && info.i_nxn;

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
                    let bs = if ofs == 0 { 4 } else { 3 };
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
                    luma_edge(y, horiz, mbx, mby, ofs, bs, ia, ib);
                }
            }

            let cqp_q = chroma_qp_jm(qp_q, chroma_off);
            for plane in [&mut *cb, &mut *cr] {
                for &horiz in &[false, true] {
                    for ce in 0..2usize {
                        let ofs = ce * 4;
                        let at_pic_edge = if horiz { mby == 0 } else { mbx == 0 };
                        if ofs == 0 && at_pic_edge {
                            continue;
                        }
                        let bs = if ofs == 0 { 4 } else { 3 };
                        let qp_p_y = if ofs != 0 {
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
                        for t in 0..8 {
                            let mut s = [0i32; 8];
                            let (bx, by) = if horiz { (mbx * 8 + t, mby * 8 + ofs) } else { (mbx * 8 + ofs, mby * 8 + t) };
                            for (k, sl) in s.iter_mut().enumerate() {
                                *sl = if horiz { plane.at(bx, by - 4 + k) } else { plane.at(bx - 4 + k, by) };
                            }
                            let tc0 = if bs == 4 { 0 } else { TC0[ia][bs - 1] };
                            filter_chroma(&mut s, alpha, beta, tc0, bs == 4);
                            for (k, &v) in s.iter().enumerate() {
                                if horiz {
                                    plane.set(bx, by - 4 + k, v);
                                } else {
                                    plane.set(bx - 4 + k, by, v);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn modes_cells(mby: usize, mbx: usize, bw4: usize, n: usize) -> Vec<usize> {
    let mut v = Vec::new();
    for j in 0..n {
        for i in 0..n {
            v.push((mby * 4 + j) * bw4 + mbx * 4 + i);
        }
    }
    v
}
fn left_cell(modes: &[Option<u8>], cx: usize, cy: usize, bw4: usize) -> Option<u8> {
    if cx == 0 { None } else { modes[cy * bw4 + cx - 1] }
}
fn up_cell(modes: &[Option<u8>], cx: usize, cy: usize, bw4: usize, mb_top: bool) -> Option<u8> {
    // A cell in the MB's top row (cy % 4 == 0) reads its up-mode from the MB
    // above — unavailable across a slice boundary.
    if cy == 0 || (cy % 4 == 0 && !mb_top) {
        None
    } else {
        modes[(cy - 1) * bw4 + cx]
    }
}
fn seq8(gbx: usize, gby: usize, width: usize) -> usize {
    (gby / 2 * width + gbx / 2) * 4 + (gby & 1) * 2 + (gbx & 1)
}
fn seq4(cx: usize, cy: usize, width: usize) -> usize {
    let (mbx, mby) = (cx / 4, cy / 4);
    let (lx, ly) = (cx % 4, cy % 4);
    let region = (ly / 2) * 2 + (lx / 2);
    let sub = (ly % 2) * 2 + (lx % 2);
    ((mby * width + mbx) * 4 + region) * 4 + sub
}

fn gather_nxn<'a>(p: &Plane, px: usize, py: usize, n: usize, mb_top: bool) -> NeighborsNxN<'a> {
    // The whole-MB (16×16 / chroma 8×8) block's top edge is always the MB
    // boundary, so its top is available only if the MB above is in-slice.
    let top_avail = py > 0 && mb_top;
    let left_avail = px > 0;
    let top: Vec<i32> = (0..n).map(|i| if top_avail { p.at(px + i, py - 1) } else { 0 }).collect();
    let left: Vec<i32> = (0..n).map(|j| if left_avail { p.at(px - 1, py + j) } else { 0 }).collect();
    let corner = if top_avail && left_avail { p.at(px - 1, py - 1) } else { 0 };
    NeighborsNxN {
        top: Box::leak(top.into_boxed_slice()),
        left: Box::leak(left.into_boxed_slice()),
        corner,
        top_avail,
        left_avail,
    }
}

fn gather_8x8(p: &Plane, gbx: usize, gby: usize, fw: usize, bw: usize, cur_seq: usize, mb_top: bool) -> Neighbors8x8 {
    let (px, py) = (gbx * 8, gby * 8);
    let left_avail = gbx > 0;
    // Top-row 8×8 blocks of the MB (gby even) take their top from the MB
    // above; gate on the MB above being in-slice.
    let top_avail = gby > 0 && (gby & 1 == 1 || mb_top);
    let corner_avail = left_avail && top_avail;
    let ar = top_avail && gbx + 1 < bw && seq8(gbx + 1, gby - 1, fw / 16) < cur_seq;
    let mut top = [0i32; 16];
    let mut left = [0i32; 8];
    let mut corner = 0;
    if top_avail {
        for x in 0..8 {
            top[x] = p.at(px + x, py - 1);
        }
        for x in 8..16 {
            top[x] = if ar { p.at(px + x, py - 1) } else { top[7] };
        }
    }
    if left_avail {
        for (j, s) in left.iter_mut().enumerate() {
            *s = p.at(px - 1, py + j);
        }
    }
    if corner_avail {
        corner = p.at(px - 1, py - 1);
    }
    Neighbors8x8 { top, left, corner, top_avail, left_avail, corner_avail }
}

fn gather_4x4(p: &Plane, cx: usize, cy: usize, fw: usize, bw4: usize, cur_seq: usize, mb_top: bool) -> Neighbors4x4 {
    let (px, py) = (cx * 4, cy * 4);
    let left_avail = cx > 0;
    // Top-row 4×4 cells of the MB (cy % 4 == 0) take their top from the MB
    // above; gate on the MB above being in-slice.
    let top_avail = cy > 0 && (cy % 4 != 0 || mb_top);
    let corner = if left_avail && top_avail { p.at(px - 1, py - 1) } else { 0 };
    let ar = top_avail && cx + 1 < bw4 && seq4(cx + 1, cy - 1, fw / 16) < cur_seq;
    let mut top = [0i32; 8];
    let mut left = [0i32; 4];
    if top_avail {
        for x in 0..4 {
            top[x] = p.at(px + x, py - 1);
        }
        for x in 4..8 {
            top[x] = if ar { p.at(px + x, py - 1) } else { top[3] };
        }
    }
    if left_avail {
        for (j, s) in left.iter_mut().enumerate() {
            *s = p.at(px - 1, py + j);
        }
    }
    Neighbors4x4 { top, left, corner, top_avail, left_avail }
}
