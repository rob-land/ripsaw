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
use crate::mvc::mb_residual::{decode_mb_residual, CbfNeighbours, CbpBits, MbResidual, ResidualContexts};
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

/// A single reconstructed plane (owned samples). Public so the inter decoder
/// can reuse `reconstruct_intra_mb` for intra MBs inside a P-slice.
/// A mutable view into a contiguous *band* of a larger buffer, indexed by the
/// element's GLOBAL position. `base` is the global index of the band's first
/// element; every index subtracts it. This lets the parallel per-slice decoders
/// each write a disjoint MB-row band of one shared allocation using their
/// unchanged global-coordinate indexing — no per-slice buffer, no merge. Safe:
/// `split_at_mut` hands out non-overlapping `&mut` bands, and `Index`/`IndexMut`
/// are bounds-checked (an out-of-band index — e.g. a mid-slice desync writing
/// past its rows — underflows/overflows into a panic, caught as a thread error).
pub struct Band<'a, T> {
    d: &'a mut [T],
    base: usize,
}
impl<'a, T> Band<'a, T> {
    pub fn new(d: &'a mut [T], base: usize) -> Self {
        Band { d, base }
    }
}
impl<T> std::ops::Index<usize> for Band<'_, T> {
    type Output = T;
    #[inline]
    fn index(&self, i: usize) -> &T {
        &self.d[i - self.base]
    }
}
impl<T> std::ops::IndexMut<usize> for Band<'_, T> {
    #[inline]
    fn index_mut(&mut self, i: usize) -> &mut T {
        &mut self.d[i - self.base]
    }
}

/// Split `buf` into `bounds.len()` contiguous sub-slices at the ascending
/// element boundaries `bounds = [end0, end1, …, endN=buf.len()]` (band `k` is
/// `[bounds[k-1], bounds[k])`, band 0 is `[0, bounds[0])`). Used to hand each
/// parallel slice decoder a disjoint MB-row band of one shared allocation.
pub fn split_bands<'a, T>(buf: &'a mut [T], bounds: &[usize]) -> Vec<&'a mut [T]> {
    let mut rest = buf;
    let mut out = Vec::with_capacity(bounds.len());
    let mut prev = 0;
    for &b in bounds {
        let (head, tail) = rest.split_at_mut(b - prev);
        out.push(head);
        rest = tail;
        prev = b;
    }
    out
}

/// A reconstructed luma/chroma plane as a borrowed band (see [`Band`]) — the
/// current frame's samples, addressed by global pixel coordinate. `base` is the
/// global sample offset of the band's first row.
pub struct Plane<'a> {
    pub d: &'a mut [u8],
    pub w: usize,
    pub base: usize,
}
impl Plane<'_> {
    pub(crate) fn at(&self, x: usize, y: usize) -> i32 {
        self.d[y * self.w + x - self.base] as i32
    }
    pub(crate) fn set(&mut self, x: usize, y: usize, v: i32) {
        self.d[y * self.w + x - self.base] = v.clamp(0, 255) as u8;
    }
}

/// Reconstruct one intra MB (I_4x4 / I_8x8 / I_16x16 + chroma) into the
/// planes and update the per-4×4 mode grid. Shared by the intra-frame decoder
/// and the P-slice decoder (intra MBs in a P-slice). `raw` are the decoded
/// intra pred-mode syntax values (rem/−1) for I_NxN; `mb_top` = the MB above
/// is in-slice (left is always in-slice for MB-row-contiguous slices).
#[allow(clippy::too_many_arguments)]
pub fn reconstruct_intra_mb<'a>(
    y: &mut Plane<'a>,
    cb: &mut Plane<'a>,
    cr: &mut Plane<'a>,
    modes: &mut Band<Option<u8>>,
    info: &MbInfo,
    raw: &[i64],
    res: &MbResidual,
    mbx: usize,
    mby: usize,
    width: usize,
    bw4: usize,
    fw: usize,
    mb_top: bool,
) {
    let (mpx, mpy) = (mbx * 16, mby * 16);
    if !info.i_nxn {
        let mode = PlaneMode::from_index(info.i16_pred as u32).unwrap();
        let n = gather_nxn(y, mpx, mpy, 16, mb_top);
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
            let mode = derive_intra_mode(left_cell(modes, cx, cy, bw4), up_cell(modes, cx, cy, bw4, mb_top), raw[b8]);
            let n = gather_8x8(y, gbx, gby, fw, bw4 / 2, seq8(gbx, gby, width), mb_top);
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
                let mode = derive_intra_mode(left_cell(modes, cx, cy, bw4), up_cell(modes, cx, cy, bw4, mb_top), raw[idx]);
                let n = gather_4x4(y, cx, cy, fw, bw4, seq4(cx, cy, width), mb_top);
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

    let cmode = match info.c_ipred {
        0 => PlaneMode::Dc,
        1 => PlaneMode::Horizontal,
        2 => PlaneMode::Vertical,
        _ => PlaneMode::Plane,
    };
    for (plane, cres) in [(&mut *cb, &res.cb), (&mut *cr, &res.cr)] {
        let n = gather_nxn(plane, mbx * 8, mby * 8, 8, mb_top);
        let pred = predict_chroma_8x8(cmode, &n);
        for yy in 0..8 {
            for xx in 0..8 {
                plane.set(mbx * 8 + xx, mby * 8 + yy, pred[yy][xx] + cres[yy][xx]);
            }
        }
    }
}

/// The per-slice band views ([`Band`]/[`Plane`]) an intra-slice decoder writes
/// into — disjoint MB-row bands of the frame's shared buffers (no merge).
struct IBufs<'a> {
    y: Plane<'a>,
    cb: Plane<'a>,
    cr: Plane<'a>,
    modes: Band<'a, Option<u8>>,
    grid: Band<'a, MbInfo>,
    cbp_grid: Band<'a, CbpBits>,
    qp_grid: Band<'a, i32>,
}

/// Decode one intra (IDR or I) slice's RBSP into a reconstructed frame.
/// One IDR/I access unit's slices (`slices` in decode order, `first_mb_in_slice`
/// increasing). A single-slice frame is `&[rbsp]`; a multi-slice frame passes
/// all its slices. Slice boundaries break intra prediction (cross-slice
/// neighbours are unavailable). Pre-deblock.
fn decode_intra_frame_one(slices: &[&[u8]], nal_ref_idc: u8, idr: bool, sps: &Sps, pps: &Pps, bufs: IBufs) -> anyhow::Result<(usize, (u32, i32, i32))> {
    let width = sps.pic_width_in_mbs as usize;
    let fw = width * 16;
    // Inverse-quant scaling lists: PPS overrides SPS overrides flat.
    let scaling = pps.scaling.clone().or_else(|| sps.scaling.clone()).unwrap_or_else(ScalingLists::flat);

    let IBufs { mut y, mut cb, mut cr, mut modes, mut grid, mut cbp_grid, mut qp_grid } = bufs;
    let bw4 = width * 4;
    let mut decoded_mbs = 0usize;
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
        let left_i = if mbx != 0 { Some(grid[addr - 1]) } else { None };
        let up_i = if mb_top { Some(grid[addr - width]) } else { None };
        let mut header: Vec<(String, i64)> = Vec::new();
        let info = decode_mb_header(&mut e, &mut hctx, &Neighbors { left: left_i, up: up_i }, &mut last_dquant, &mut header);
        if let Some((_, d)) = header.iter().find(|(n, _)| n == "mb_qp_delta") {
            qp = (qp + *d as i32).rem_euclid(52);
        }
        let raw: Vec<i64> = header.iter().filter(|(n, _)| n == "intra4x4_pred_mode").map(|(_, v)| *v).collect();

        let mut neigh = CbfNeighbours {
            cur: CbpBits::default(),
            left: if mbx != 0 { Some(cbp_grid[addr - 1]) } else { None },
            up: if mb_top { Some(cbp_grid[addr - width]) } else { None },
        };
        let res = decode_mb_residual(&mut e, &mut rctx, &info, &mut neigh, qp, pps.chroma_qp_index_offset, false, &scaling, &mut sink);

        reconstruct_intra_mb(&mut y, &mut cb, &mut cr, &mut modes, &info, &raw, &res, mbx, mby, width, bw4, fw, mb_top);

        let eos = e.decode_terminate();
        grid[addr] = info;
        cbp_grid[addr] = neigh.cur;
        qp_grid[addr] = qp;
        decoded_mbs += 1;
        if eos == 1 {
            break;
        }
        addr += 1;
    }
    } // per-slice loop

    let sh = sh_last.expect("at least one slice");
    Ok((decoded_mbs, (sh.disable_deblocking_filter_idc, sh.slice_alpha_c0_offset_div2, sh.slice_beta_offset_div2)))
}

/// Decode an intra (I) frame (pre-deblock). The frame's buffers are allocated
/// once and split into disjoint per-slice MB-row bands ([`split_bands`]); the
/// independent slices decode into their bands in parallel, assembling the frame
/// directly with no merge. Single-slice frames decode inline. Verifies coverage.
pub fn decode_intra_frame(slices: &[&[u8]], nal_ref_idc: u8, idr: bool, sps: &Sps, pps: &Pps) -> anyhow::Result<Frame> {
    let width = sps.pic_width_in_mbs as usize;
    let (fw, fh) = (width * 16, sps.pic_height_in_map_units as usize * 16);
    let (cw, ch) = (fw / 2, fh / 2);
    let bw4 = width * 4;
    let num_mbs = width * (fh / 16);

    let mut y = vec![0u8; fw * fh];
    let mut cb = vec![0u8; cw * ch];
    let mut cr = vec![0u8; cw * ch];
    let mut modes = vec![None::<u8>; bw4 * (fh / 4)];
    let default_mb = MbInfo { i_nxn: false, transform8x8: false, c_ipred: 0, cbp: 0, i16_pred: 0 };
    let mut grid = vec![default_mb; num_mbs];
    let mut cbp_grid = vec![CbpBits::default(); num_mbs];
    let mut qp_grid = vec![0i32; num_mbs];

    let (n_total, dp) = {
        let firsts: Vec<usize> = slices
            .iter()
            .map(|sl| Ok::<_, anyhow::Error>(parse_slice_header(&mut BitReader::new(sl), idr, nal_ref_idc, sps, pps)?.first_mb_in_slice as usize))
            .collect::<Result<_, _>>()?;
        anyhow::ensure!(firsts.iter().all(|&f| f % width == 0), "non-MB-row-aligned slice (unsupported)");
        let row_ends: Vec<usize> = (0..slices.len()).map(|k| firsts.get(k + 1).copied().unwrap_or(num_mbs) / width).collect();
        let yb: Vec<usize> = row_ends.iter().map(|&r| r * 16 * fw).collect();
        let cbnd: Vec<usize> = row_ends.iter().map(|&r| r * 8 * cw).collect();
        let g4: Vec<usize> = row_ends.iter().map(|&r| r * 4 * bw4).collect();
        let mbb: Vec<usize> = row_ends.iter().map(|&r| r * width).collect();

        let mut yv = split_bands(&mut y, &yb).into_iter();
        let mut cbv = split_bands(&mut cb, &cbnd).into_iter();
        let mut crv = split_bands(&mut cr, &cbnd).into_iter();
        let mut modv = split_bands(&mut modes, &g4).into_iter();
        let mut grv = split_bands(&mut grid, &mbb).into_iter();
        let mut cbgv = split_bands(&mut cbp_grid, &mbb).into_iter();
        let mut qpv = split_bands(&mut qp_grid, &mbb).into_iter();

        let mut bufs_list: Vec<IBufs> = Vec::with_capacity(slices.len());
        for &f in &firsts {
            let r0 = f / width;
            bufs_list.push(IBufs {
                y: Plane { d: yv.next().unwrap(), w: fw, base: r0 * 16 * fw },
                cb: Plane { d: cbv.next().unwrap(), w: cw, base: r0 * 8 * cw },
                cr: Plane { d: crv.next().unwrap(), w: cw, base: r0 * 8 * cw },
                modes: Band::new(modv.next().unwrap(), r0 * 4 * bw4),
                grid: Band::new(grv.next().unwrap(), r0 * width),
                cbp_grid: Band::new(cbgv.next().unwrap(), r0 * width),
                qp_grid: Band::new(qpv.next().unwrap(), r0 * width),
            });
        }

        if slices.len() < 2 {
            let bufs = bufs_list.pop().unwrap();
            decode_intra_frame_one(slices, nal_ref_idc, idr, sps, pps, bufs)?
        } else {
            let results: Vec<anyhow::Result<(usize, (u32, i32, i32))>> = std::thread::scope(|scope| {
                let handles: Vec<_> = slices
                    .iter()
                    .zip(bufs_list)
                    .map(|(sl, bufs)| scope.spawn(move || decode_intra_frame_one(std::slice::from_ref(sl), nal_ref_idc, idr, sps, pps, bufs)))
                    .collect();
                handles.into_iter().map(|h| h.join().unwrap_or_else(|_| Err(anyhow::anyhow!("intra-slice decode thread panicked")))).collect()
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
    anyhow::ensure!(n_total == num_mbs, "intra slice desync — {n_total}/{num_mbs} MBs decoded");

    let (disable, alpha, beta) = dp;
    Ok(Frame {
        y,
        cb,
        cr,
        fw,
        fh,
        cw,
        ch,
        width_mbs: width,
        mb_info: grid,
        qp: qp_grid,
        disable_deblock_idc: disable,
        slice_alpha_c0_offset_div2: alpha,
        slice_beta_offset_div2: beta,
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
        // Deblock runs on the whole assembled frame — base 0 (identity offset).
        let mut y = Plane { d: &mut self.y, w: fw, base: 0 };
        let mut cb = Plane { d: &mut self.cb, w: cw, base: 0 };
        let mut cr = Plane { d: &mut self.cr, w: cw, base: 0 };
        deblock_intra(&mut y, &mut cb, &mut cr, &self.mb_info, &self.qp, width, chroma_off, off_a, off_b);
    }
}

fn chroma_qp_jm(qpy: i32, offset: i32) -> i32 {
    const MAP: [i32; 22] = [29, 30, 31, 32, 32, 33, 34, 34, 35, 35, 36, 36, 37, 37, 37, 38, 38, 38, 39, 39, 39, 39];
    let qpi = (qpy + offset).clamp(0, 51);
    if qpi < 30 { qpi } else { MAP[(qpi - 30) as usize] }
}

#[allow(clippy::too_many_arguments)]
fn deblock_intra<'a>(y: &mut Plane<'a>, cb: &mut Plane<'a>, cr: &mut Plane<'a>, grid: &[MbInfo], qp_grid: &[i32], width: usize, chroma_off: i32, off_a: i32, off_b: i32) {
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
fn left_cell(modes: &Band<Option<u8>>, cx: usize, cy: usize, bw4: usize) -> Option<u8> {
    if cx == 0 { None } else { modes[cy * bw4 + cx - 1] }
}
fn up_cell(modes: &Band<Option<u8>>, cx: usize, cy: usize, bw4: usize, mb_top: bool) -> Option<u8> {
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

fn gather_nxn(p: &Plane, px: usize, py: usize, n: usize, mb_top: bool) -> NeighborsNxN {
    // The whole-MB (16×16 / chroma 8×8) block's top edge is always the MB
    // boundary, so its top is available only if the MB above is in-slice.
    let top_avail = py > 0 && mb_top;
    let left_avail = px > 0;
    let mut top = [0i32; 16];
    let mut left = [0i32; 16];
    for i in 0..n {
        top[i] = if top_avail { p.at(px + i, py - 1) } else { 0 };
        left[i] = if left_avail { p.at(px - 1, py + i) } else { 0 };
    }
    let corner = if top_avail && left_avail { p.at(px - 1, py - 1) } else { 0 };
    NeighborsNxN { top, left, corner, top_avail, left_avail }
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
