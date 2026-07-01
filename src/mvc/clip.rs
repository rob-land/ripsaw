// Native full-clip MVC decode: an Annex B elementary stream (base +
// dependent view, as produced by `mkv_extract` / mkvextract) → a stream of
// reconstructed stereo frame pairs, entirely in libmvc. This is the library
// form of `examples/decode_mvc_clip` (validated bit-exact vs JM on the real
// disc, all 96 frames, both views); the runner uses it in place of the
// external `ldecod` decode step.
//
// Scope: the real Blu-ray Stereo High streams we target — base view = a
// single-ref P chain with occasional mid-GOP I-frames, dependent view =
// anchor (inter-view only) + non-anchor (inter-view + temporal) P-frames
// with an explicit ref_pic_list_modification. One active base SPS / subset
// SPS / base PPS / dependent PPS at a time (as these streams carry).

use std::io::{self, Write};

use crate::mvc::annexb::NalSplitter;
use crate::mvc::bitstream::BitReader;
use crate::mvc::nal::parse_nal_unit_header;
use crate::mvc::pps::{parse_pic_parameter_set, Pps};
use crate::mvc::rbsp::extract_rbsp;
use crate::mvc::recon::{decode_intra_frame, Frame};
use crate::mvc::recon_inter::{deblock_b, deblock_inter, decode_b_frame, decode_p_frame, MotionField};
use crate::mvc::ref_pic_list_modification::RefPicListModification;
use crate::mvc::slice_header::parse_slice_header;
use crate::mvc::sps::{parse_seq_parameter_set_data, parse_subset_sps_rbsp, Sps};

/// One access unit's coded slices, split by view.
#[derive(Default)]
struct Au {
    base: Vec<Vec<u8>>,
    base_idr: bool,
    base_idc: u8,
    dep: Vec<Vec<u8>>,
    dep_idc: u8,
    dep_idr: bool,    // !non_idr_flag — IDR-marked header layout
    dep_anchor: bool, // anchor_pic_flag — inter-view-only ref list
}

/// Display geometry + frame count of a decoded clip.
#[derive(Debug, Clone, Copy)]
pub struct ClipInfo {
    /// Cropped luma width (per-view), e.g. 1920.
    pub width: u32,
    /// Cropped luma height (per-view), e.g. 1080.
    pub height: u32,
    /// Number of stereo frame pairs emitted.
    pub frames: usize,
}

/// One reference-list entry, unresolved: the single inter-view candidate (the
/// current base frame) or a temporal candidate indexed into the dependent
/// DPB (recent-first, index 0 = most recent).
#[derive(Clone, Copy, PartialEq, Eq)]
enum DRef {
    InterView,
    Temporal(usize),
}

/// Build the dependent view's L0 honoring `ref_pic_list_modification`
/// (H.264 §8.2.4.3 + Annex G, inter-view IDCs 4/5). The initial list is the
/// temporal short-term refs (PicNum-descending == DPB recent-first) with the
/// inter-view ref appended, truncated to `num_ref`; each command then moves
/// its target picture to the running refIdx position.
fn build_dep_l0(mods: &Option<Vec<RefPicListModification>>, num_ref: usize, dpb_len: usize, anchor: bool) -> Vec<DRef> {
    use RefPicListModification::*;
    let mut list: Vec<DRef> = if anchor { Vec::new() } else { (0..dpb_len).map(DRef::Temporal).collect() };
    list.push(DRef::InterView);
    list.truncate(num_ref.max(1));
    if let Some(cmds) = mods {
        let mut refidx = 0usize;
        // picNumL0Pred tracked relative to CurrPicNum (0 = current); a
        // PicNumSub lowers it, and a short-term ref with PicNum = CurrPicNum +
        // rel sits at DPB index (-rel - 1) since the DPB is recent-first.
        let mut picnum_rel: i64 = 0;
        for cmd in cmds {
            let target = match cmd {
                PicNumSub { abs_diff_pic_num_minus1: d } => {
                    picnum_rel -= *d as i64 + 1;
                    DRef::Temporal((-picnum_rel - 1).max(0) as usize)
                }
                PicNumAdd { abs_diff_pic_num_minus1: d } => {
                    picnum_rel += *d as i64 + 1;
                    DRef::Temporal((-picnum_rel - 1).max(0) as usize)
                }
                InterViewAdd { .. } | InterViewSub { .. } => DRef::InterView,
                LongTerm { .. } => continue,
            };
            list.retain(|r| *r != target);
            list.insert(refidx.min(list.len()), target);
            list.truncate(num_ref);
            refidx += 1;
        }
    }
    list
}

/// Split an Annex B byte stream into access units, tracking the active
/// parameter sets. Returns the AUs plus the resolved (base SPS, base PPS,
/// dependent subset-SPS, dependent PPS).
fn parse_stream(data: &[u8]) -> anyhow::Result<(Vec<Au>, Sps, Pps, Sps, Pps)> {
    let (mut bsps, mut bpps): (Option<Sps>, Option<Pps>) = (None, None);
    let (mut dsps, mut dpps): (Option<Sps>, Option<Pps>) = (None, None);
    let mut seen_subset = false;
    let mut aus: Vec<Au> = Vec::new();

    for nal in NalSplitter::new(data) {
        if nal.is_empty() {
            continue;
        }
        let Ok((hdr, consumed)) = parse_nal_unit_header(nal) else { continue };
        let rbsp = extract_rbsp(&nal[consumed..]);
        match hdr.nal_unit_type {
            9 => aus.push(Au::default()), // access unit delimiter
            7 => bsps = Some(parse_seq_parameter_set_data(&mut BitReader::new(&rbsp))?),
            15 => {
                dsps = Some(parse_subset_sps_rbsp(&rbsp)?.sps);
                seen_subset = true;
            }
            8 => {
                let chroma = bsps.as_ref().map(|s| s.chroma_format_idc).unwrap_or(1);
                let p = parse_pic_parameter_set(&mut BitReader::new(&rbsp), chroma)?;
                // The dependent PPS is the NAL 8 that follows the subset SPS.
                if seen_subset {
                    dpps = Some(p);
                } else {
                    bpps = Some(p);
                }
            }
            5 | 1 => {
                // A slice with no preceding AUD starts a fresh AU.
                if aus.last().map(|a| !a.base.is_empty() && !a.dep.is_empty()).unwrap_or(true) {
                    aus.push(Au::default());
                }
                let au = aus.last_mut().unwrap();
                au.base_idr = hdr.nal_unit_type == 5;
                au.base_idc = hdr.nal_ref_idc;
                au.base.push(rbsp.to_vec());
            }
            20 => {
                let au = aus.last_mut().unwrap();
                au.dep_idc = hdr.nal_ref_idc;
                if let Some(ext) = &hdr.mvc_extension {
                    au.dep_idr = !ext.non_idr_flag;
                    au.dep_anchor = ext.anchor_pic_flag;
                }
                au.dep.push(rbsp.to_vec());
            }
            _ => {}
        }
    }
    aus.retain(|a| !a.base.is_empty());

    let bsps = bsps.ok_or_else(|| anyhow::anyhow!("no base SPS (NAL 7) in stream"))?;
    let bpps = bpps.ok_or_else(|| anyhow::anyhow!("no base PPS (NAL 8) in stream"))?;
    let dsps = dsps.ok_or_else(|| anyhow::anyhow!("no subset SPS (NAL 15) — not an MVC stream"))?;
    let dpps = dpps.ok_or_else(|| anyhow::anyhow!("no dependent PPS in stream"))?;
    Ok((aus, bsps, bpps, dsps, dpps))
}

/// Decode a full MVC Annex B elementary stream. `on_frame(base, dep, w, h)`
/// is invoked once per access unit in decode order with both reconstructed,
/// deblocked views (`base` = left / view 0, `dep` = right / view 1) and the
/// cropped display width/height. Frames carry MB-padded planes (`fw`/`fh` may
/// exceed the display size); use [`write_cropped_yuv420`] to trim.
pub fn decode_annex_b<F>(data: &[u8], mut on_frame: F) -> anyhow::Result<ClipInfo>
where
    F: FnMut(&Frame, &Frame, u32, u32) -> anyhow::Result<()>,
{
    let (aus, bsps, bpps, dsps, dpps) = parse_stream(data)?;
    // Route by GOP structure: an all-I/P stream (decode order == display
    // order, dependent L0 built from ref_pic_list_modification) uses the
    // sliding-window path below; a hierarchical stream with B-slices uses the
    // POC-ordered path (temporal L0/L1, display-order reorder).
    let has_b = aus.iter().any(|au| {
        parse_slice_header(&mut BitReader::new(&au.base[0]), au.base_idr, au.base_idc, &bsps, &bpps)
            .map(|sh| sh.slice_type % 5 == 1)
            .unwrap_or(false)
    });
    if has_b {
        return decode_annex_b_hierarchical(&aus, &bsps, &bpps, &dsps, &dpps, on_frame);
    }

    let (dw, dh) = (bsps.width, bsps.height);
    let max_ref = bsps.max_num_ref_frames.max(1) as usize;
    let mut base_dpb: Vec<Frame> = Vec::new();
    let mut dep_dpb: Vec<Frame> = Vec::new();
    let mut frames = 0usize;

    for au in &aus {
        // ---- base view ---- route by slice_type: I-slices (IDR or not)
        // decode intra; P-slices reference the base DPB.
        let bsh = parse_slice_header(&mut BitReader::new(&au.base[0]), au.base_idr, au.base_idc, &bsps, &bpps)?;
        // libmvc decodes I and P slices; B-slices (hierarchical GOPs) aren't
        // supported yet. Bail cleanly so the caller can fall back.
        anyhow::ensure!(bsh.slice_type % 5 != 1, "B-slices not supported by libmvc yet (base view)");
        let base_intra = bsh.slice_type % 5 == 2;
        let bs: Vec<&[u8]> = au.base.iter().map(|s| s.as_slice()).collect();
        let bf = if base_intra {
            let mut f = decode_intra_frame(&bs, au.base_idc, au.base_idr, &bsps, &bpps)?;
            f.deblock_intra(bpps.chroma_qp_index_offset);
            f
        } else {
            let refs: Vec<&Frame> = base_dpb.iter().collect();
            let (mut f, mf) = decode_p_frame(&bs, au.base_idc, false, &bsps, &bpps, &refs)?;
            deblock_inter(&mut f, &mf, bpps.chroma_qp_index_offset);
            f
        };

        // ---- dependent view ---- L0 per the slice's ref_pic_list_modification.
        let ds: Vec<&[u8]> = au.dep.iter().map(|s| s.as_slice()).collect();
        let dsh = parse_slice_header(&mut BitReader::new(&au.dep[0]), au.dep_idr, au.dep_idc, &dsps, &dpps)?;
        anyhow::ensure!(dsh.slice_type % 5 != 1, "B-slices not supported by libmvc yet (dependent view)");
        let dnum = (dsh.num_ref_idx_l0_active_minus1 + 1) as usize;
        let dl0 = build_dep_l0(&dsh.ref_pic_list_modifications.list0, dnum, dep_dpb.len(), au.dep_anchor);
        let drefs: Vec<&Frame> = dl0
            .iter()
            .map(|r| match r {
                DRef::InterView => &bf,
                DRef::Temporal(i) => &dep_dpb[*i],
            })
            .collect();
        let (mut df, dmf) = decode_p_frame(&ds, au.dep_idc, au.dep_idr, &dsps, &dpps, &drefs)?;
        deblock_inter(&mut df, &dmf, dpps.chroma_qp_index_offset);

        on_frame(&bf, &df, dw, dh)?;
        frames += 1;

        // Sliding-window DPBs, newest at front.
        base_dpb.insert(0, bf);
        base_dpb.truncate(max_ref);
        dep_dpb.insert(0, df);
        dep_dpb.truncate(max_ref);
    }

    Ok(ClipInfo { width: bsps.width, height: bsps.height, frames })
}

/// One view's short-term reference: display POC, deblocked frame, L0 motion
/// field (co-located source for a B-slice's spatial direct).
type ViewRef = (i32, Frame, MotionField);

/// Decode one hierarchical-GOP view frame. `inter_view` = the base frame for a
/// dependent anchor (inter-view L0); otherwise `refs` are temporal. B uses
/// L0 = nearest-past, L1 = nearest-future (num_ref 1; empty
/// ref_pic_list_modification → default temporal list).
#[allow(clippy::too_many_arguments)]
fn decode_hier_view(slices: &[&[u8]], idr: bool, idc: u8, st: u32, poc: i32, sps: &Sps, pps: &Pps, refs: &[ViewRef], inter_view: Option<&Frame>) -> anyhow::Result<(Frame, Option<MotionField>)> {
    if st == 2 {
        let mut f = decode_intra_frame(slices, idc, idr, sps, pps)?;
        f.deblock_intra(pps.chroma_qp_index_offset);
        Ok((f, None))
    } else if st == 0 {
        let reff: &Frame = if let Some(iv) = inter_view {
            iv
        } else {
            &refs.iter().filter(|(p, ..)| *p < poc).max_by_key(|(p, ..)| *p).ok_or_else(|| anyhow::anyhow!("no past ref for P"))?.1
        };
        let (mut f, mf) = decode_p_frame(slices, idc, idr, sps, pps, &[reff])?;
        deblock_inter(&mut f, &mf, pps.chroma_qp_index_offset);
        Ok((f, Some(mf)))
    } else {
        let l0 = refs.iter().filter(|(p, ..)| *p < poc).max_by_key(|(p, ..)| *p).ok_or_else(|| anyhow::anyhow!("no L0 for B"))?;
        let l1 = refs.iter().filter(|(p, ..)| *p > poc).min_by_key(|(p, ..)| *p).ok_or_else(|| anyhow::anyhow!("no L1 for B"))?;
        let (mut f, bmf) = decode_b_frame(slices, idc, idr, sps, pps, &[(&l0.1, l0.0)], &[(&l1.1, l1.0)], &l1.2, (32, 32))?;
        deblock_b(&mut f, &bmf, pps.chroma_qp_index_offset);
        Ok((f, None))
    }
}

/// POC-ordered decode for a hierarchical (B-slice) MVC stream, emitting frame
/// pairs in DISPLAY order. Per-view DPBs are reset at each GOP boundary (base
/// IDR / dependent anchor); a per-GOP reorder buffer flushes in POC order at
/// each new GOP. The dependent view's anchor is inter-view-predicted from the
/// base frame of the same access unit; its later P/B frames are temporal.
fn decode_annex_b_hierarchical<F>(aus: &[Au], bsps: &Sps, bpps: &Pps, dsps: &Sps, dpps: &Pps, mut on_frame: F) -> anyhow::Result<ClipInfo>
where
    F: FnMut(&Frame, &Frame, u32, u32) -> anyhow::Result<()>,
{
    let (dw, dh) = (bsps.width, bsps.height);
    let max_lsb = 1i32 << (bsps.log2_max_pic_order_cnt_lsb_minus4 + 4);
    let mut brefs: Vec<ViewRef> = Vec::new();
    let mut drefs: Vec<ViewRef> = Vec::new();
    let (mut pmsb, mut plsb) = (0i32, 0i32);
    // Per-GOP reorder buffer: (display POC, base, dep). Flushed in POC order at
    // each IDR and at end.
    let mut pending: Vec<(i32, Frame, Frame)> = Vec::new();
    let mut frames = 0usize;

    let mut flush = |pending: &mut Vec<(i32, Frame, Frame)>, on_frame: &mut F| -> anyhow::Result<()> {
        pending.sort_by_key(|(p, ..)| *p);
        for (_, b, d) in pending.drain(..) {
            on_frame(&b, &d, dw, dh)?;
        }
        Ok(())
    };

    for au in aus {
        let bsh = parse_slice_header(&mut BitReader::new(&au.base[0]), au.base_idr, au.base_idc, bsps, bpps)?;
        let lsb = bsh.pic_order_cnt_lsb.unwrap_or(0) as i32;
        if au.base_idr {
            flush(&mut pending, &mut on_frame)?;
            brefs.clear();
            drefs.clear();
            pmsb = 0;
            plsb = 0;
        }
        // Full POC (§8.2.1.1): resolve the LSB wrap against the last ref pic.
        let msb = if au.base_idr {
            0
        } else if lsb < plsb && plsb - lsb >= max_lsb / 2 {
            pmsb + max_lsb
        } else if lsb > plsb && lsb - plsb > max_lsb / 2 {
            pmsb - max_lsb
        } else {
            pmsb
        };
        let poc = msb + lsb;
        if au.base_idc != 0 {
            pmsb = msb;
            plsb = lsb;
        }

        // Base view.
        let bs: Vec<&[u8]> = au.base.iter().map(|s| s.as_slice()).collect();
        let (bf, bmf) = decode_hier_view(&bs, au.base_idr, au.base_idc, bsh.slice_type % 5, poc, bsps, bpps, &brefs, None)?;

        // Dependent view: anchor is inter-view from the base just decoded.
        let ds: Vec<&[u8]> = au.dep.iter().map(|s| s.as_slice()).collect();
        let dsh = parse_slice_header(&mut BitReader::new(&au.dep[0]), au.dep_idr, au.dep_idc, dsps, dpps)?;
        let inter_view = if au.dep_anchor { Some(&bf) } else { None };
        let (df, dmf) = decode_hier_view(&ds, au.dep_idr, au.dep_idc, dsh.slice_type % 5, poc, dsps, dpps, &drefs, inter_view)?;

        if au.base_idc != 0 {
            brefs.push((poc, clone_frame(&bf), bmf.unwrap_or_else(empty_motion_field)));
        }
        if au.dep_idc != 0 {
            drefs.push((poc, clone_frame(&df), dmf.unwrap_or_else(empty_motion_field)));
        }
        pending.push((poc, bf, df));
        frames += 1;
    }
    flush(&mut pending, &mut on_frame)?;

    Ok(ClipInfo { width: bsps.width, height: bsps.height, frames })
}

fn empty_motion_field() -> MotionField {
    MotionField { mv: Vec::new(), refidx: Vec::new(), nz: Vec::new(), bw4: 0, bh4: 0 }
}

fn clone_frame(f: &Frame) -> Frame {
    Frame {
        y: f.y.clone(),
        cb: f.cb.clone(),
        cr: f.cr.clone(),
        fw: f.fw,
        fh: f.fh,
        cw: f.cw,
        ch: f.ch,
        width_mbs: f.width_mbs,
        mb_info: f.mb_info.clone(),
        qp: f.qp.clone(),
        disable_deblock_idc: f.disable_deblock_idc,
        slice_alpha_c0_offset_div2: f.slice_alpha_c0_offset_div2,
        slice_beta_offset_div2: f.slice_beta_offset_div2,
    }
}

/// Decode an MVC Annex B stream to two cropped planar YUV 4:2:0 files — view
/// 0 (base / left) to `view0`, view 1 (dependent / right) to `view1` — the
/// same per-view layout JM's `ldecod` produces, so the existing ffmpeg
/// compose step consumes them unchanged. Returns the display geometry + frame
/// count. Writers are buffered internally.
pub fn decode_annex_b_to_yuv_files(data: &[u8], view0: &std::path::Path, view1: &std::path::Path) -> anyhow::Result<ClipInfo> {
    let mut w0 = io::BufWriter::new(std::fs::File::create(view0)?);
    let mut w1 = io::BufWriter::new(std::fs::File::create(view1)?);
    let info = decode_annex_b(data, |bf, df, w, h| {
        write_cropped_yuv420(bf, w as usize, h as usize, &mut w0)?;
        write_cropped_yuv420(df, w as usize, h as usize, &mut w1)?;
        Ok(())
    })?;
    w0.flush()?;
    w1.flush()?;
    Ok(info)
}

/// Write a frame's luma + chroma as cropped planar YUV 4:2:0 (I420) to `out`:
/// `w`×`h` luma followed by (`w`/2)×(`h`/2) Cb then Cr. Rows are copied from
/// the frame's (MB-padded) planes, trimming the padding to the display size.
pub fn write_cropped_yuv420(f: &Frame, w: usize, h: usize, out: &mut impl Write) -> io::Result<()> {
    for y in 0..h {
        out.write_all(&f.y[y * f.fw..y * f.fw + w])?;
    }
    let (cw, ch) = (w / 2, h / 2);
    for y in 0..ch {
        out.write_all(&f.cb[y * f.cw..y * f.cw + cw])?;
    }
    for y in 0..ch {
        out.write_all(&f.cr[y * f.cw..y * f.cw + cw])?;
    }
    Ok(())
}
