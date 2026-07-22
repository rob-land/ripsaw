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
use std::sync::Arc;

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
    // The PPS active for each view when this AU's slices were parsed. A stream
    // re-sends its PPS (same id) with a changed pic_init_qp over time (rate
    // control), so the PPS in effect is per-AU, not a single global one.
    bpps: Option<Pps>,
    dpps: Option<Pps>,
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
    // The dependent subset-SPS id: a PPS whose seq_parameter_set_id matches is a
    // dependent-view PPS, otherwise base. This is order-independent (unlike the
    // former "PPS after the subset SPS" heuristic, which mis-classified a base
    // PPS re-sent in a later AU as the dependent PPS).
    dep_sps_id: Option<u32>,
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
                let sps = parse_subset_sps_rbsp(&rbsp)?.sps;
                self.dep_sps_id = Some(sps.seq_parameter_set_id);
                self.dsps = Some(sps);
            }
            8 => {
                let chroma = self.bsps.as_ref().map(|s| s.chroma_format_idc).unwrap_or(1);
                let p = parse_pic_parameter_set(&mut BitReader::new(&rbsp), chroma)?;
                // A PPS referencing the dependent subset-SPS is a dependent-view
                // PPS; otherwise it's base. Both views re-send their PPS (same id,
                // changed pic_init_qp) over the stream, so this only updates the
                // active set — each AU snapshots it below.
                if self.dep_sps_id == Some(p.seq_parameter_set_id) {
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
                if au.bpps.is_none() {
                    au.bpps = self.bpps.clone();
                }
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
                if au.dpps.is_none() {
                    au.dpps = self.dpps.clone();
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
            && parse_slice_header(&mut BitReader::new(&au.base[0]), au.base_idr, au.base_idc, &bsps, au.bpps.as_ref().unwrap_or(&bpps))
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
        // Use the PPS active for this AU (a stream re-sends its PPS with a
        // changed pic_init_qp over time); fall back to the global set.
        let bpps = au.bpps.as_ref().unwrap_or(bpps);
        let dpps = au.dpps.as_ref().unwrap_or(dpps);
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
// Reference-picture DPB entry: (POC, decoded frame, co-located motion field).
// The frame + motion are `Arc`-shared so a background (non-reference-frame)
// decode can snapshot the references it reads for the cost of an Arc clone,
// without deep-copying whole 1080p frames or blocking DPB mutation.
type ViewRef = (i32, Arc<Frame>, Arc<MotionField>);

/// Decode one hierarchical-GOP view frame. `inter_view` = the base frame for a
/// dependent anchor (inter-view L0); otherwise `refs` are temporal. B uses
/// L0 = nearest-past, L1 = nearest-future (num_ref 1; empty
/// ref_pic_list_modification → default temporal list).
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_arguments)]
fn decode_hier_view(
    slices: &[&[u8]], idr: bool, idc: u8, st: u32, poc: i32,
    num_ref_l0: usize, num_ref_l1: usize, sps: &Sps, pps: &Pps,
    refs: &[ViewRef], inter_view: Option<&Frame>,
    mods0: &Option<Vec<RefPicListModification>>, mods1: &Option<Vec<RefPicListModification>>,
) -> anyhow::Result<(Frame, Option<MotionField>)> {
    if st == 2 {
        let mut f = decode_intra_frame(slices, idc, idr, sps, pps)?;
        f.deblock_intra(pps.chroma_qp_index_offset);
        return Ok((f, None));
    }
    // Build one reference list: the default temporal short-term list
    // (§ 8.2.4.2.3 — L0 = past POC-descending then future POC-ascending;
    // L1 = future POC-ascending then past POC-descending) with the inter-view
    // base appended AFTER the temporal refs when that list's
    // ref_pic_list_modification adds it (the `[PicNum.., InterViewAdd]` order
    // these dependent streams use; the InterViewAdd sits at the last refIdx, so
    // `num_ref - 1` temporal refs precede it). A dependent anchor is
    // inter-view-only (num_ref_l0 = 1, mod = [InterViewAdd] → 0 temporal). The
    // returned mf is the temporal ref's L0 motion field (for a B's spatial-
    // direct co-located source). Open-GOP leading frames fall back cleanly:
    // no past ref → the list starts with the nearest future.
    let build = |is_l1: bool, num_ref: usize, mods: &Option<Vec<RefPicListModification>>, force_iv: bool| -> Vec<(&Frame, i32, Option<&MotionField>)> {
        let mut past: Vec<&ViewRef> = refs.iter().filter(|(p, ..)| *p < poc).collect();
        past.sort_by(|a, b| b.0.cmp(&a.0));
        let mut fut: Vec<&ViewRef> = refs.iter().filter(|(p, ..)| *p > poc).collect();
        fut.sort_by(|a, b| a.0.cmp(&b.0));
        let temporal: Vec<&ViewRef> = if is_l1 { fut.into_iter().chain(past).collect() } else { past.into_iter().chain(fut).collect() };
        // The inter-view base joins THIS list only when its ref_pic_list_
        // modification adds it (the `[PicNum.., InterViewAdd]` order → it sits
        // after the temporal refs, last of `num_ref` slots) — OR, for a P
        // frame (`force_iv`), by default even with an empty modification (a
        // dependent anchor whose sole ref is the inter-view base). A B's L1
        // here has an empty modification, so it stays temporal-only.
        let mod_iv = mods.as_ref().map(|m| m.iter().any(|c| matches!(c, RefPicListModification::InterViewAdd { .. } | RefPicListModification::InterViewSub { .. }))).unwrap_or(false);
        let has_iv = inter_view.is_some() && (force_iv || mod_iv);
        let ntemp = if has_iv { num_ref.saturating_sub(1) } else { num_ref };
        let mut list: Vec<(&Frame, i32, Option<&MotionField>)> = temporal.iter().take(ntemp).map(|vr| (vr.1.as_ref(), vr.0, Some(vr.2.as_ref()))).collect();
        if has_iv {
            list.push((inter_view.unwrap(), poc, None));
        }
        list.truncate(num_ref.max(1));
        list
    };
    if st == 0 {
        let l0 = build(false, num_ref_l0, mods0, true);
        anyhow::ensure!(!l0.is_empty(), "no L0 for a P frame (poc={poc})");
        let l0f: Vec<&Frame> = l0.iter().map(|(f, ..)| *f).collect();
        let (mut f, mut mf) = decode_p_frame(slices, idc, idr, sps, pps, &l0f)?;
        // Resolve each block's referenced POC (for temporal-direct MapColToList0
        // when this frame is later a B's co-located source) from this P's own L0.
        let l0_pocs: Vec<i32> = l0.iter().map(|(_, p, _)| *p).collect();
        mf.resolve_refpoc(&l0_pocs);
        deblock_inter(&mut f, &mf, pps.chroma_qp_index_offset);
        Ok((f, Some(mf)))
    } else {
        let l0 = build(false, num_ref_l0, mods0, false);
        let l1 = build(true, num_ref_l1, mods1, false);
        anyhow::ensure!(!l0.is_empty() && !l1.is_empty(), "no refs for B");
        let l0p: Vec<(&Frame, i32)> = l0.iter().map(|(f, p, _)| (*f, *p)).collect();
        let l1p: Vec<(&Frame, i32)> = l1.iter().map(|(f, p, _)| (*f, *p)).collect();
        // Spatial-direct co-located source = RefPicList1[0]'s L0 motion field.
        let col: MotionField = l1[0].2.cloned().unwrap_or_else(empty_motion_field);
        let (mut f, bmf) = decode_b_frame(slices, idc, idr, sps, pps, &l0p, &l1p, poc, &col, (32, 32))?;
        deblock_b(&mut f, &bmf, pps.chroma_qp_index_offset);
        // A b-pyramid *referenced* B (idc != 0) can be a later frame's co-located
        // picture, so surface its reduced motion field (§ 8.4.1.2.1). Non-ref B
        // frames aren't stored by the caller, so this is a no-op for them.
        Ok((f, Some(bmf.colocated())))
    }
}

/// Does this AU's dependent view inter-view predict from the base of the SAME
/// AU (anchor, or a non-anchor whose ref-list mod adds the inter-view ref)?
fn dep_is_inter_view(au: &Au, dsps: &Sps, dpps_global: &Pps) -> anyhow::Result<bool> {
    if au.dep_anchor {
        return Ok(true);
    }
    let dpps = au.dpps.as_ref().unwrap_or(dpps_global);
    let dsh = parse_slice_header(&mut BitReader::new(&au.dep[0]), au.dep_idr, au.dep_idc, dsps, dpps)?;
    let has_iv = |m: &Option<Vec<RefPicListModification>>| m.as_ref().map(|c| c.iter().any(|x| matches!(x, RefPicListModification::InterViewAdd { .. } | RefPicListModification::InterViewSub { .. }))).unwrap_or(false);
    Ok(has_iv(&dsh.ref_pic_list_modifications.list0) || has_iv(&dsh.ref_pic_list_modifications.list1))
}

/// Decode one AU's base view at display POC `poc`, reading the immutable base
/// DPB `brefs`.
fn decode_base(au: &Au, poc: i32, bsps: &Sps, bpps_global: &Pps, brefs: &[ViewRef]) -> anyhow::Result<(Frame, Option<MotionField>)> {
    let bpps = au.bpps.as_ref().unwrap_or(bpps_global);
    let bs: Vec<&[u8]> = au.base.iter().map(|s| s.as_slice()).collect();
    let bsh = parse_slice_header(&mut BitReader::new(&au.base[0]), au.base_idr, au.base_idc, bsps, bpps)?;
    let bnum = (bsh.num_ref_idx_l0_active_minus1 + 1) as usize;
    let bnum1 = (bsh.num_ref_idx_l1_active_minus1 + 1) as usize;
    let (bm0, bm1) = (&bsh.ref_pic_list_modifications.list0, &bsh.ref_pic_list_modifications.list1);
    decode_hier_view(&bs, au.base_idr, au.base_idc, bsh.slice_type % 5, poc, bnum, bnum1, bsps, bpps, brefs, None, bm0, bm1)
}

/// Decode one AU's dependent view at display POC `poc`, reading the immutable
/// dependent DPB `drefs`. `base` is this AU's decoded base frame — used as the
/// inter-view reference iff the dependent view is inter-view predicted.
fn decode_dep(au: &Au, poc: i32, dsps: &Sps, dpps_global: &Pps, drefs: &[ViewRef], base: Option<&Frame>) -> anyhow::Result<(Frame, Option<MotionField>)> {
    let dpps = au.dpps.as_ref().unwrap_or(dpps_global);
    let ds: Vec<&[u8]> = au.dep.iter().map(|s| s.as_slice()).collect();
    let dsh = parse_slice_header(&mut BitReader::new(&au.dep[0]), au.dep_idr, au.dep_idc, dsps, dpps)?;
    let dnum = (dsh.num_ref_idx_l0_active_minus1 + 1) as usize;
    let dnum1 = (dsh.num_ref_idx_l1_active_minus1 + 1) as usize;
    let (dm0, dm1) = (&dsh.ref_pic_list_modifications.list0, &dsh.ref_pic_list_modifications.list1);
    let has_iv = |m: &Option<Vec<RefPicListModification>>| m.as_ref().map(|c| c.iter().any(|x| matches!(x, RefPicListModification::InterViewAdd { .. } | RefPicListModification::InterViewSub { .. }))).unwrap_or(false);
    let inter_view = au.dep_anchor || has_iv(dm0) || has_iv(dm1);
    let iv = if inter_view { base } else { None };
    decode_hier_view(&ds, au.dep_idr, au.dep_idc, dsh.slice_type % 5, poc, dnum, dnum1, dsps, dpps, drefs, iv, dm0, dm1)
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

    // Broken leading pictures at stream start: when the stream is extracted from
    // the middle of an open GOP (no leading IDR), the first anchor is a plain I
    // frame and the B(s) displayed just before it reference a picture preceding
    // the extract — which was never decoded. JM decodes them (concealed) but does
    // not output them. We track the first-decoded POC of the current clean run
    // (reset at each IDR); any picture displayed before it, before any IDR has
    // established a self-contained start, is such a broken leading picture and is
    // decoded (for anyone referencing it — these are non-reference in practice)
    // but withheld from output.
    let mut first_decoded_poc: Option<i32> = None;
    let mut seen_idr = false;
    let flush = |pending: &mut Vec<(i32, Frame, Frame)>, on_frame: &mut F| -> anyhow::Result<()> {
        pending.sort_by_key(|(p, ..)| *p);
        for (_, b, d) in pending.drain(..) {
            on_frame(&b, &d, dw, dh)?;
        }
        Ok(())
    };

    // Reorder push + bounded-buffer bump-emit for one decoded frame.
    let push_emit = |pending: &mut Vec<(i32, Frame, Frame)>, on_frame: &mut F, poc: i32, leading: bool, bf: Frame, df: Frame| -> anyhow::Result<()> {
        if !leading {
            pending.push((poc, bf, df));
        }
        // Bump the lowest-POC frame out once the buffer exceeds the reorder
        // depth — keeps memory bounded instead of holding a whole GOP.
        while pending.len() > reorder_cap {
            let i = pending.iter().enumerate().min_by_key(|(_, (p, ..))| *p).map(|(i, _)| i).unwrap();
            let (_, b, d) = pending.remove(i);
            on_frame(&b, &d, dw, dh)?;
        }
        Ok(())
    };

    // Base∥dependent software pipeline. The dependent view of an inter-view AU
    // needs that AU's base, so within an AU base→dep is serial — the dominant
    // cost. But dep_n (needs base_n + the dep DPB) and base_{n+1} (needs the base
    // DPB, already holding base_n) are INDEPENDENT, so they decode concurrently.
    // We keep the base one AU ahead: `held` is a decoded base whose dependent
    // view is decoded — alongside the NEXT base — one step later. Every frame's
    // DPB snapshot is identical to serial decode (see the reasoning in the loop),
    // so output is bit-exact; the reorder buffer just trails by one AU.
    struct Held {
        au: Au,
        poc: i32,
        leading: bool,
        bf: Frame,
    }
    let mut held: Option<Held> = None;
    // Decode one held base's dependent view against the current dep DPB, update
    // the dep DPB, and emit the (base, dep) pair.
    let finish_held = |h: Held, drefs: &mut Vec<ViewRef>, pending: &mut Vec<(i32, Frame, Frame)>, on_frame: &mut F, frames: &mut usize| -> anyhow::Result<()> {
        let (df, dmf) = decode_dep(&h.au, h.poc, dsps, dpps, drefs, Some(&h.bf))?;
        if h.au.dep_idc != 0 {
            drefs.push((h.poc, Arc::new(clone_frame(&df)), Arc::new(dmf.unwrap_or_else(empty_motion_field))));
            if drefs.len() > ref_window {
                drefs.remove(0);
            }
        }
        *frames += 1;
        push_emit(pending, on_frame, h.poc, h.leading, h.bf, df)
    };

    for au in aus {
        let au = au?;
        let bpps_au = au.bpps.as_ref().unwrap_or(bpps);
        let bsh = parse_slice_header(&mut BitReader::new(&au.base[0]), au.base_idr, au.base_idc, bsps, bpps_au)?;
        let lsb = bsh.pic_order_cnt_lsb.unwrap_or(0) as i32;
        let msb_of = |pmsb: i32, plsb: i32| {
            if lsb < plsb && plsb - lsb >= max_lsb / 2 { pmsb + max_lsb }
            else if lsb > plsb && lsb - plsb > max_lsb / 2 { pmsb - max_lsb }
            else { pmsb }
        };

        // An IDR resets the GOP: finish the held base (previous GOP) against the
        // pre-reset DPB, flush the reorder buffer, then reset.
        if au.base_idr {
            if let Some(h) = held.take() {
                finish_held(h, &mut drefs, &mut pending, &mut on_frame, &mut frames)?;
            }
            flush(&mut pending, &mut on_frame)?;
            brefs.clear();
            drefs.clear();
            pmsb = 0;
            plsb = 0;
            seen_idr = true;
            first_decoded_poc = None;
        }

        let msb = if au.base_idr { 0 } else { msb_of(pmsb, plsb) };
        let poc = msb + lsb;
        if au.base_idc != 0 {
            pmsb = msb;
            plsb = lsb;
        }
        let leading = !seen_idr && first_decoded_poc.map(|fp| poc < fp).unwrap_or(false);
        if first_decoded_poc.is_none() {
            first_decoded_poc = Some(poc);
        }

        // Decode this AU's BASE concurrently with the held AU's DEPENDENT view.
        // brefs already holds the held base, so this base's references are
        // complete; the held dep reads drefs. Both DPBs are immutable here (only
        // mutated below), so each view sees exactly the DPB it would in serial
        // decode — the output is identical, just pipelined by one AU.
        #[allow(clippy::type_complexity)]
        let (base_res, dep_res): (
            anyhow::Result<(Frame, Option<MotionField>)>,
            Option<anyhow::Result<(Frame, Option<MotionField>)>>,
        ) = std::thread::scope(|scope| {
            let (brefs_r, drefs_r) = (&brefs, &drefs);
            let dep_handle = held.as_ref().map(|h| scope.spawn(move || decode_dep(&h.au, h.poc, dsps, dpps, drefs_r, Some(&h.bf))));
            let base_res = decode_base(&au, poc, bsps, bpps, brefs_r);
            let dep_res = dep_handle.map(|hd| hd.join().unwrap_or_else(|_| Err(anyhow::anyhow!("dependent-view pipeline thread panicked"))));
            (base_res, dep_res)
        });
        let (bf, bmf) = base_res?;

        // Finish (DPB-update + emit) the held pair, which precedes this AU in
        // decode order, then add this base to the base DPB and hold it.
        if let Some(h) = held.take() {
            let (df, dmf) = dep_res.expect("a dep is decoded iff a base was held")?;
            if h.au.dep_idc != 0 {
                drefs.push((h.poc, Arc::new(clone_frame(&df)), Arc::new(dmf.unwrap_or_else(empty_motion_field))));
                if drefs.len() > ref_window {
                    drefs.remove(0);
                }
            }
            frames += 1;
            push_emit(&mut pending, &mut on_frame, h.poc, h.leading, h.bf, df)?;
        }
        if au.base_idc != 0 {
            brefs.push((poc, Arc::new(clone_frame(&bf)), Arc::new(bmf.unwrap_or_else(empty_motion_field))));
            if brefs.len() > ref_window {
                brefs.remove(0);
            }
        }
        held = Some(Held { au, poc, leading, bf });
    }
    if let Some(h) = held.take() {
        finish_held(h, &mut drefs, &mut pending, &mut on_frame, &mut frames)?;
    }
    flush(&mut pending, &mut on_frame)?;

    Ok(ClipInfo { width: bsps.width, height: bsps.height, frames })
}

fn empty_motion_field() -> MotionField {
    MotionField { mv: Vec::new(), refidx: Vec::new(), refpoc: Vec::new(), nz: Vec::new(), bw4: 0, bh4: 0 }
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

/// Write a stereo pair as one full-side-by-side (2`w`×`h`) planar YUV 4:2:0
/// frame: each plane row is the left view's cropped row followed by the right
/// view's, so the packed frame is view 0 in the left half and view 1 in the
/// right half. Feeding this straight to ffmpeg lets the whole pipeline avoid a
/// tens-of-GB intermediate YUV on disk — the decoder streams composed frames
/// into the encoder.
pub fn write_fsbs_yuv420(left: &Frame, right: &Frame, w: usize, h: usize, out: &mut impl Write) -> io::Result<()> {
    for y in 0..h {
        out.write_all(&left.y[y * left.fw..y * left.fw + w])?;
        out.write_all(&right.y[y * right.fw..y * right.fw + w])?;
    }
    let (cw, ch) = (w / 2, h / 2);
    for y in 0..ch {
        out.write_all(&left.cb[y * left.cw..y * left.cw + cw])?;
        out.write_all(&right.cb[y * right.cw..y * right.cw + cw])?;
    }
    for y in 0..ch {
        out.write_all(&left.cr[y * left.cw..y * left.cw + cw])?;
        out.write_all(&right.cr[y * right.cw..y * right.cw + cw])?;
    }
    Ok(())
}

/// Pixel format the FSBS packer emits into the encode pipe. `Yuv420p` is the
/// planar form software encoders (libx264/265) consume directly. `Nv12` is the
/// semi-planar form VAAPI (and other HW encoders) require — emitting it here
/// fuses the yuv420p→nv12 colour-space conversion into the packer, so ffmpeg's
/// swscale doesn't run a separate CSC pass before `hwupload`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FsbsPixFmt {
    Yuv420p,
    Nv12,
}

impl FsbsPixFmt {
    /// The ffmpeg `-pixel_format` token for a rawvideo input of this format.
    pub fn ffmpeg_name(self) -> &'static str {
        match self {
            FsbsPixFmt::Yuv420p => "yuv420p",
            FsbsPixFmt::Nv12 => "nv12",
        }
    }
}

/// Pack a stereo pair as one full-SBS NV12 frame (view0 left / view1 right): the
/// Y plane (2w×h, left row then right row) followed by an interleaved UV plane
/// (2w×h/2). NV12's Y plane is byte-identical to yuv420p's; its UV plane is the
/// U and V samples interleaved (U0 V0 U1 V1 …) at half resolution. Producing
/// NV12 directly is the CSC-fusion: `format=nv12` in ffmpeg becomes a no-op.
pub fn write_fsbs_nv12(left: &Frame, right: &Frame, w: usize, h: usize, out: &mut impl Write) -> io::Result<()> {
    // Y plane — identical packing to yuv420p.
    for y in 0..h {
        out.write_all(&left.y[y * left.fw..y * left.fw + w])?;
        out.write_all(&right.y[y * right.fw..y * right.fw + w])?;
    }
    // Interleaved UV plane, half height. Each row is left's interleaved UV
    // (over `cw` chroma samples) then right's, for a 2w-byte row. A scratch row
    // coalesces the interleave into one write per row.
    let (cw, ch) = (w / 2, h / 2);
    let mut row = vec![0u8; 2 * w];
    for y in 0..ch {
        let lcb = &left.cb[y * left.cw..y * left.cw + cw];
        let lcr = &left.cr[y * left.cw..y * left.cw + cw];
        let rcb = &right.cb[y * right.cw..y * right.cw + cw];
        let rcr = &right.cr[y * right.cw..y * right.cw + cw];
        for i in 0..cw {
            row[2 * i] = lcb[i];
            row[2 * i + 1] = lcr[i];
            row[2 * cw + 2 * i] = rcb[i];
            row[2 * cw + 2 * i + 1] = rcr[i];
        }
        out.write_all(&row)?;
    }
    Ok(())
}

/// Pack a stereo pair as one full-SBS frame in the requested pixel format.
pub fn write_fsbs(left: &Frame, right: &Frame, w: usize, h: usize, pixfmt: FsbsPixFmt, out: &mut impl Write) -> io::Result<()> {
    match pixfmt {
        FsbsPixFmt::Yuv420p => write_fsbs_yuv420(left, right, w, h, out),
        FsbsPixFmt::Nv12 => write_fsbs_nv12(left, right, w, h, out),
    }
}

/// Decode an MVC Annex B stream (read incrementally from `reader`) and write
/// each access unit as one full-side-by-side frame ([`write_fsbs`]) to `out` in
/// `pixfmt` — the streaming decode → encode bridge used by the conversion
/// runner. `out` is consumed and flushed (then dropped by the caller for EOF).
pub fn decode_annex_b_to_fsbs_writer<R: Read, W: Write>(reader: R, out: W, pixfmt: FsbsPixFmt) -> anyhow::Result<ClipInfo> {
    let mut w = io::BufWriter::with_capacity(1 << 20, out);
    let info = decode_annex_b_reader(reader, |bf, df, dw, dh| {
        write_fsbs(bf, df, dw as usize, dh as usize, pixfmt, &mut w)?;
        Ok(())
    })?;
    w.flush()?;
    Ok(info)
}

#[cfg(test)]
mod fsbs_tests {
    use super::*;
    use crate::mvc::recon::Frame;

    /// A `w×h` frame whose Y/Cb/Cr are filled with distinct position-dependent
    /// ramps (so plane mix-ups are caught). `fw`/`cw` add right-padding to
    /// exercise the packer's stride handling (fw > w).
    fn ramp_frame(w: usize, h: usize, base: u8) -> Frame {
        let (fw, cw, ch) = (w + 8, w / 2 + 4, h / 2);
        let mut y = vec![0u8; fw * h];
        for row in 0..h {
            for col in 0..w {
                y[row * fw + col] = base.wrapping_add((row * 7 + col * 3) as u8);
            }
        }
        let mut cb = vec![0u8; cw * ch];
        let mut cr = vec![0u8; cw * ch];
        for row in 0..ch {
            for col in 0..w / 2 {
                cb[row * cw + col] = base.wrapping_add(0x40).wrapping_add((row + col) as u8);
                cr[row * cw + col] = base.wrapping_add(0x80).wrapping_add((row * 2 + col) as u8);
            }
        }
        Frame {
            y, cb, cr, fw, fh: h, cw, ch,
            width_mbs: w / 16, mb_info: Vec::new(), qp: Vec::new(),
            disable_deblock_idc: 0, slice_alpha_c0_offset_div2: 0, slice_beta_offset_div2: 0,
        }
    }

    /// NV12 packing must equal the yuv420p→nv12 transform of the SAME FSBS
    /// frame: identical Y plane, and a UV plane that is exactly the yuv420p U/V
    /// planes interleaved (U0 V0 U1 V1 …). This is what makes ffmpeg's
    /// `format=nv12` a no-op — the CSC-fusion correctness guarantee.
    #[test]
    fn nv12_equals_yuv420p_to_nv12_transform() {
        let (w, h) = (32usize, 16usize);
        let (left, right) = (ramp_frame(w, h, 10), ramp_frame(w, h, 200));

        let mut p420 = Vec::new();
        write_fsbs_yuv420(&left, &right, w, h, &mut p420).unwrap();
        let mut pnv12 = Vec::new();
        write_fsbs_nv12(&left, &right, w, h, &mut pnv12).unwrap();

        let fw = 2 * w; // packed FSBS width
        let (cw, ch) = (fw / 2, h / 2);
        let ysz = fw * h;
        let planesz = cw * ch;
        assert_eq!(p420.len(), ysz + 2 * planesz);
        assert_eq!(pnv12.len(), ysz + 2 * planesz);

        // Y planes identical.
        assert_eq!(&pnv12[..ysz], &p420[..ysz], "Y plane differs");

        // NV12 UV plane == interleave(yuv420p U, yuv420p V).
        let u = &p420[ysz..ysz + planesz];
        let v = &p420[ysz + planesz..ysz + 2 * planesz];
        let uv = &pnv12[ysz..ysz + 2 * planesz];
        for i in 0..planesz {
            assert_eq!(uv[2 * i], u[i], "UV[{}] U mismatch", 2 * i);
            assert_eq!(uv[2 * i + 1], v[i], "UV[{}] V mismatch", 2 * i + 1);
        }
    }
}
