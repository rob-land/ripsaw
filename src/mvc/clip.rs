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

use std::io::{self, Read, Write};

use crate::mvc::annexb::NalReader;
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
/// Streaming access-unit assembler: fed NALs one at a time, it parses the
/// stream's SPS / PPS / subset-SPS / dependent-PPS and groups slice NALs into
/// access units, emitting a completed [`Au`] when the next one begins. Same
/// grouping as the former whole-buffer parse, but incremental — nothing beyond
/// the current AU (and the four header sets) is ever held.
#[derive(Default)]
struct AuAssembler {
    bsps: Option<Sps>,
    bpps: Option<Pps>,
    dsps: Option<Sps>,
    dpps: Option<Pps>,
    seen_subset: bool,
    cur: Option<Au>,
}

impl AuAssembler {
    /// Feed one NAL. Returns the just-completed access unit if this NAL begins a
    /// new one (an AUD, or a base slice while the current AU already holds both
    /// views); SPS/PPS NALs update header state and return `None`.
    fn feed(&mut self, nal: &[u8]) -> anyhow::Result<Option<Au>> {
        let Ok((hdr, consumed)) = parse_nal_unit_header(nal) else { return Ok(None) };
        let rbsp = extract_rbsp(&nal[consumed..]);
        match hdr.nal_unit_type {
            9 => {
                // Access unit delimiter — the current AU (if coded) is complete.
                let done = self.cur.take().filter(|a| !a.base.is_empty());
                self.cur = Some(Au::default());
                return Ok(done);
            }
            7 => self.bsps = Some(parse_seq_parameter_set_data(&mut BitReader::new(&rbsp))?),
            15 => {
                self.dsps = Some(parse_subset_sps_rbsp(&rbsp)?.sps);
                self.seen_subset = true;
            }
            8 => {
                let chroma = self.bsps.as_ref().map(|s| s.chroma_format_idc).unwrap_or(1);
                let p = parse_pic_parameter_set(&mut BitReader::new(&rbsp), chroma)?;
                // The dependent PPS is the NAL 8 that follows the subset SPS.
                if self.seen_subset {
                    self.dpps = Some(p);
                } else {
                    self.bpps = Some(p);
                }
            }
            5 | 1 => {
                // A base slice starts a fresh AU when the current one already
                // has both views (or there is none) — i.e. no preceding AUD.
                let mut done = None;
                if self.cur.as_ref().map(|a| !a.base.is_empty() && !a.dep.is_empty()).unwrap_or(true) {
                    done = self.cur.take().filter(|a| !a.base.is_empty());
                    self.cur = Some(Au::default());
                }
                let au = self.cur.get_or_insert_with(Au::default);
                au.base_idr = hdr.nal_unit_type == 5;
                au.base_idc = hdr.nal_ref_idc;
                au.base.push(rbsp.to_vec());
                return Ok(done);
            }
            20 => {
                let au = self.cur.get_or_insert_with(Au::default);
                au.dep_idc = hdr.nal_ref_idc;
                if let Some(ext) = &hdr.mvc_extension {
                    au.dep_idr = !ext.non_idr_flag;
                    au.dep_anchor = ext.anchor_pic_flag;
                }
                au.dep.push(rbsp.to_vec());
            }
            _ => {}
        }
        Ok(None)
    }

    /// The final (unterminated) access unit at end of stream, if coded.
    fn finish(&mut self) -> Option<Au> {
        self.cur.take().filter(|a| !a.base.is_empty())
    }
}

/// Pulls complete access units from a byte stream incrementally via
/// [`NalReader`], so only the current AU (plus the header sets) is resident.
struct AuSource<R: Read> {
    nals: NalReader<R>,
    asm: AuAssembler,
    done: bool,
}

impl<R: Read> AuSource<R> {
    fn new(reader: R) -> Self {
        Self { nals: NalReader::new(reader), asm: AuAssembler::default(), done: false }
    }

    /// The next complete access unit, or `None` at end of stream.
    fn pull(&mut self) -> anyhow::Result<Option<Au>> {
        if self.done {
            return Ok(None);
        }
        while let Some(nal) = self.nals.next() {
            let nal = nal?;
            if nal.is_empty() {
                continue;
            }
            if let Some(au) = self.asm.feed(&nal)? {
                return Ok(Some(au));
            }
        }
        self.done = true;
        Ok(self.asm.finish())
    }
}

/// Decode a full MVC Annex B elementary stream from an in-memory buffer. Thin
/// wrapper over [`decode_annex_b_reader`]; prefer the reader form for large
/// streams so the bytes are not all held at once.
pub fn decode_annex_b<F>(data: &[u8], on_frame: F) -> anyhow::Result<ClipInfo>
where
    F: FnMut(&Frame, &Frame, u32, u32) -> anyhow::Result<()>,
{
    decode_annex_b_reader(io::Cursor::new(data), on_frame)
}

/// Decode a full MVC Annex B elementary stream read incrementally from any
/// `Read`. `on_frame(base, dep, w, h)` is invoked once per access unit in
/// DISPLAY order with both reconstructed, deblocked views (`base` = left /
/// view 0, `dep` = right / view 1) and the cropped display size. Frames carry
/// MB-padded planes (`fw`/`fh` may exceed the display size); use
/// [`write_cropped_yuv420`] to trim.
///
/// Memory stays bounded regardless of stream length: NALs are pulled one at a
/// time ([`AuSource`]) and only a handful of reference/reorder frames are held
/// — so a full retail movie decodes without loading it into RAM.
pub fn decode_annex_b_reader<R: Read, F>(reader: R, on_frame: F) -> anyhow::Result<ClipInfo>
where
    F: FnMut(&Frame, &Frame, u32, u32) -> anyhow::Result<()>,
{
    let mut src = AuSource::new(reader);
    // Buffer the first GOP (until the 2nd base IDR, or a cap) to detect B-slices
    // for routing. This is bounded — one GOP, not the whole stream — and the GOP
    // structure is uniform across these streams, so it is representative.
    const DETECT_CAP: usize = 96;
    let mut buffered: Vec<Au> = Vec::new();
    let mut idrs = 0usize;
    while let Some(au) = src.pull()? {
        let is_idr = au.base_idr;
        buffered.push(au);
        if is_idr {
            idrs += 1;
            if idrs >= 2 {
                break;
            }
        }
        if buffered.len() >= DETECT_CAP {
            break;
        }
    }
    // Header sets are parsed as their NALs stream by, ahead of the first slice.
    let bsps = src.asm.bsps.clone().ok_or_else(|| anyhow::anyhow!("no base SPS (NAL 7) in stream"))?;
    let bpps = src.asm.bpps.clone().ok_or_else(|| anyhow::anyhow!("no base PPS (NAL 8) in stream"))?;
    let dsps = src.asm.dsps.clone().ok_or_else(|| anyhow::anyhow!("no subset SPS (NAL 15) — not an MVC stream"))?;
    let dpps = src.asm.dpps.clone().ok_or_else(|| anyhow::anyhow!("no dependent PPS in stream"))?;

    // Route by GOP structure: a hierarchical stream (base B-slices) uses the
    // POC-ordered path (temporal L0/L1, display-order reorder); an all-I/P
    // stream uses the sliding-window path (decode order == display order).
    let has_b = buffered.iter().any(|au| {
        !au.base.is_empty()
            && parse_slice_header(&mut BitReader::new(&au.base[0]), au.base_idr, au.base_idc, &bsps, &bpps)
                .map(|sh| sh.slice_type % 5 == 1)
                .unwrap_or(false)
    });

    // The buffered first-GOP AUs, then the rest of the stream still pulled one
    // AU at a time — both as a single `Result<Au>` iterator.
    let rest = std::iter::from_fn(move || src.pull().transpose());
    let aus = buffered.into_iter().map(Ok).chain(rest);

    if has_b {
        decode_hier_stream(aus, &bsps, &bpps, &dsps, &dpps, on_frame)
    } else {
        decode_ip_stream(aus, &bsps, &bpps, &dsps, &dpps, on_frame)
    }
}

/// Sliding-window decode for an all-I/P MVC stream (decode order == display
/// order): base view is an I/P chain against a sliding DPB; the dependent view
/// builds L0 from its `ref_pic_list_modification` (inter-view base + temporal).
fn decode_ip_stream<I, F>(aus: I, bsps: &Sps, bpps: &Pps, dsps: &Sps, dpps: &Pps, mut on_frame: F) -> anyhow::Result<ClipInfo>
where
    I: Iterator<Item = anyhow::Result<Au>>,
    F: FnMut(&Frame, &Frame, u32, u32) -> anyhow::Result<()>,
{
    let (dw, dh) = (bsps.width, bsps.height);
    let max_ref = bsps.max_num_ref_frames.max(1) as usize;
    let mut base_dpb: Vec<Frame> = Vec::new();
    let mut dep_dpb: Vec<Frame> = Vec::new();
    let mut frames = 0usize;

    for au in aus {
        let au = au?;
        // ---- base view ---- route by slice_type: I-slices (IDR or not)
        // decode intra; P-slices reference the base DPB.
        let bsh = parse_slice_header(&mut BitReader::new(&au.base[0]), au.base_idr, au.base_idc, bsps, bpps)?;
        // libmvc decodes I and P slices; B-slices (hierarchical GOPs) aren't
        // supported yet. Bail cleanly so the caller can fall back.
        anyhow::ensure!(bsh.slice_type % 5 != 1, "B-slices not supported by libmvc yet (base view)");
        let base_intra = bsh.slice_type % 5 == 2;
        let bs: Vec<&[u8]> = au.base.iter().map(|s| s.as_slice()).collect();
        let bf = if base_intra {
            let mut f = decode_intra_frame(&bs, au.base_idc, au.base_idr, bsps, bpps)?;
            f.deblock_intra(bpps.chroma_qp_index_offset);
            f
        } else {
            let refs: Vec<&Frame> = base_dpb.iter().collect();
            let (mut f, mf) = decode_p_frame(&bs, au.base_idc, false, bsps, bpps, &refs)?;
            deblock_inter(&mut f, &mf, bpps.chroma_qp_index_offset);
            f
        };

        // ---- dependent view ---- L0 per the slice's ref_pic_list_modification.
        let ds: Vec<&[u8]> = au.dep.iter().map(|s| s.as_slice()).collect();
        let dsh = parse_slice_header(&mut BitReader::new(&au.dep[0]), au.dep_idr, au.dep_idc, dsps, dpps)?;
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
        let (mut df, dmf) = decode_p_frame(&ds, au.dep_idc, au.dep_idr, dsps, dpps, &drefs)?;
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
fn decode_hier_stream<I, F>(aus: I, bsps: &Sps, bpps: &Pps, dsps: &Sps, dpps: &Pps, mut on_frame: F) -> anyhow::Result<ClipInfo>
where
    I: Iterator<Item = anyhow::Result<Au>>,
    F: FnMut(&Frame, &Frame, u32, u32) -> anyhow::Result<()>,
{
    let (dw, dh) = (bsps.width, bsps.height);
    let max_lsb = 1i32 << (bsps.log2_max_pic_order_cnt_lsb_minus4 + 4);
    // Bound memory to a handful of frames regardless of GOP length: keep only
    // the most-recent references (a P references the previous anchor; a B its
    // two surrounding anchors — all recent in decode order), and hold at most
    // `reorder_cap` frames in the reorder buffer. Both are sized by the DPB
    // bound (max_num_ref_frames), which upper-bounds the real reference count
    // and reorder depth for these streams.
    let ref_window = (bsps.max_num_ref_frames as usize).max(2);
    let reorder_cap = (bsps.max_num_ref_frames as usize).max(2);
    let mut brefs: Vec<ViewRef> = Vec::new();
    let mut drefs: Vec<ViewRef> = Vec::new();
    let (mut pmsb, mut plsb) = (0i32, 0i32);
    // Reorder buffer: (display POC, base, dep). Frames are bump-emitted in
    // display order as it fills; flushed at each IDR (POC resets there) + end.
    let mut pending: Vec<(i32, Frame, Frame)> = Vec::new();
    let mut frames = 0usize;

    let flush = |pending: &mut Vec<(i32, Frame, Frame)>, on_frame: &mut F| -> anyhow::Result<()> {
        pending.sort_by_key(|(p, ..)| *p);
        for (_, b, d) in pending.drain(..) {
            on_frame(&b, &d, dw, dh)?;
        }
        Ok(())
    };

    for au in aus {
        let au = au?;
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

        let bs: Vec<&[u8]> = au.base.iter().map(|s| s.as_slice()).collect();
        let ds: Vec<&[u8]> = au.dep.iter().map(|s| s.as_slice()).collect();
        let dsh = parse_slice_header(&mut BitReader::new(&au.dep[0]), au.dep_idr, au.dep_idc, dsps, dpps)?;
        let (bst, dst) = (bsh.slice_type % 5, dsh.slice_type % 5);
        // Base and dependent views are independent to decode EXCEPT at a
        // dependent anchor (inter-view predicted from the base of the same
        // AU). Decode the two views in parallel where possible.
        let ((bf, bmf), (df, dmf)) = if au.dep_anchor {
            let base = decode_hier_view(&bs, au.base_idr, au.base_idc, bst, poc, bsps, bpps, &brefs, None)?;
            let dep = decode_hier_view(&ds, au.dep_idr, au.dep_idc, dst, poc, dsps, dpps, &drefs, Some(&base.0))?;
            (base, dep)
        } else {
            std::thread::scope(|scope| -> anyhow::Result<_> {
                let bh = scope.spawn(|| decode_hier_view(&bs, au.base_idr, au.base_idc, bst, poc, bsps, bpps, &brefs, None));
                let dep = decode_hier_view(&ds, au.dep_idr, au.dep_idc, dst, poc, dsps, dpps, &drefs, None)?;
                let base = bh.join().map_err(|_| anyhow::anyhow!("base-view decode thread panicked"))??;
                Ok((base, dep))
            })?
        };

        if au.base_idc != 0 {
            brefs.push((poc, clone_frame(&bf), bmf.unwrap_or_else(empty_motion_field)));
            if brefs.len() > ref_window {
                brefs.remove(0); // drop the oldest (lowest-POC) reference
            }
        }
        if au.dep_idc != 0 {
            drefs.push((poc, clone_frame(&df), dmf.unwrap_or_else(empty_motion_field)));
            if drefs.len() > ref_window {
                drefs.remove(0);
            }
        }
        pending.push((poc, bf, df));
        // Bump the lowest-POC frame out once the buffer exceeds the reorder
        // depth — keeps memory bounded instead of holding a whole GOP.
        while pending.len() > reorder_cap {
            let i = pending.iter().enumerate().min_by_key(|(_, (p, ..))| *p).map(|(i, _)| i).unwrap();
            let (_, b, d) = pending.remove(i);
            on_frame(&b, &d, dw, dh)?;
        }
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
    decode_annex_b_to_yuv_files_reader(io::Cursor::new(data), view0, view1)
}

/// Streaming form of [`decode_annex_b_to_yuv_files`]: reads the elementary
/// stream incrementally from `reader` (e.g. a `BufReader<File>`) so a full
/// retail-length movie decodes without loading the stream into memory.
pub fn decode_annex_b_to_yuv_files_reader<R: Read>(reader: R, view0: &std::path::Path, view1: &std::path::Path) -> anyhow::Result<ClipInfo> {
    let mut w0 = io::BufWriter::new(std::fs::File::create(view0)?);
    let mut w1 = io::BufWriter::new(std::fs::File::create(view1)?);
    let info = decode_annex_b_reader(reader, |bf, df, w, h| {
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
