// Full temporal MVC clip decode with a minimal DPB — BOTH views, multiple
// frames. The base view is a single-ref P chain. The dependent view: the
// anchor (aligned with the base IDR) predicts inter-view from the base; each
// later dependent frame is a 2-ref P — L0 = [temporal previous dependent,
// inter-view current base] — decoded with ref_idx per partition. Diffs every
// frame of both views against JM's per-view ground truth.
//
//   decode_mvc_clip -- au012.h264 mvc2_ViewId0000.yuv mvc2_ViewId0001.yuv

use ripsaw::mvc::annexb::NalSplitter;
use ripsaw::mvc::bitstream::BitReader;
use ripsaw::mvc::nal::parse_nal_unit_header;
use ripsaw::mvc::pps::{parse_pic_parameter_set, Pps};
use ripsaw::mvc::rbsp::extract_rbsp;
use ripsaw::mvc::recon::{decode_intra_frame, Frame};
use ripsaw::mvc::recon_inter::{deblock_inter, decode_p_frame};
use ripsaw::mvc::sps::{parse_seq_parameter_set_data, parse_subset_sps_rbsp, Sps};
use ripsaw::mvc::ref_pic_list_modification::RefPicListModification;

/// One reference-list entry, unresolved: either the single inter-view
/// candidate (the current base frame) or a temporal candidate indexed into
/// the dependent-view DPB (recent-first, so index 0 = the most recent).
#[derive(Clone, Copy, PartialEq, Eq)]
enum DRef {
    InterView,
    Temporal(usize),
}

/// Build the dependent view's L0 honoring `ref_pic_list_modification`
/// (H.264 §8.2.4.3 + Annex G for inter-view IDCs 4/5). The initial list is
/// the temporal short-term refs (PicNum-descending == DPB recent-first) with
/// the inter-view ref appended, truncated to `num_ref`; each modification
/// command then moves its target picture to the running refIdx position.
fn build_dep_l0(mods: &Option<Vec<RefPicListModification>>, num_ref: usize, dpb_len: usize, anchor: bool) -> Vec<DRef> {
    use RefPicListModification::*;
    let mut list: Vec<DRef> = if anchor { Vec::new() } else { (0..dpb_len).map(DRef::Temporal).collect() };
    list.push(DRef::InterView);
    list.truncate(num_ref.max(1));
    if let Some(cmds) = mods {
        let mut refidx = 0usize;
        // picNumL0Pred tracked relative to CurrPicNum (0 = current); each
        // PicNumSub lowers it, and the DPB index of a short-term ref with
        // PicNum = CurrPicNum + rel is (-rel - 1) since DPB is recent-first.
        let mut picnum_rel: i64 = 0;
        for cmd in cmds {
            let target = match cmd {
                PicNumSub { abs_diff_pic_num_minus1: d } => {
                    picnum_rel -= *d as i64 + 1;
                    DRef::Temporal((-picnum_rel - 1) as usize)
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

fn main() -> anyhow::Result<()> {
    let h264 = std::env::args().nth(1).unwrap();
    let base_truth = std::env::args().nth(2).unwrap();
    let dep_truth = std::env::args().nth(3).unwrap();
    let data = std::fs::read(&h264)?;
    let (mut bsps, mut bpps): (Option<Sps>, Option<Pps>) = (None, None);
    let (mut dsps, mut dpps): (Option<Sps>, Option<Pps>) = (None, None);
    let mut seen_subset = false;

    let mut aus: Vec<Au> = Vec::new();
    for nal in NalSplitter::new(&data) {
        if nal.is_empty() {
            continue;
        }
        let Ok((hdr, consumed)) = parse_nal_unit_header(nal) else { continue };
        let rbsp = extract_rbsp(&nal[consumed..]);
        match hdr.nal_unit_type {
            9 => aus.push(Au::default()),
            7 => bsps = Some(parse_seq_parameter_set_data(&mut BitReader::new(&rbsp))?),
            15 => {
                dsps = Some(parse_subset_sps_rbsp(&rbsp)?.sps);
                seen_subset = true;
            }
            8 => {
                let chroma = bsps.as_ref().unwrap().chroma_format_idc;
                let p = parse_pic_parameter_set(&mut BitReader::new(&rbsp), chroma)?;
                if seen_subset {
                    dpps = Some(p);
                } else {
                    bpps = Some(p);
                }
            }
            5 | 1 => {
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

    let (bsps, bpps) = (bsps.as_ref().unwrap(), bpps.as_ref().unwrap());
    let (dsps, dpps) = (dsps.as_ref().unwrap(), dpps.as_ref().unwrap());
    let fw = bsps.pic_width_in_mbs as usize * 16;
    let cw = fw / 2;
    let bjm = std::fs::read(&base_truth)?;
    let djm = std::fs::read(&dep_truth)?;
    let fsz = fw * 1080 + 2 * (cw * 540);

    let cmp = |tag: &str, f: &Frame, jm: &[u8], k: usize| -> bool {
        let off = k * fsz;
        for yy in 0..1080 {
            for xx in 0..fw {
                if f.y[yy * fw + xx] != jm[off + yy * fw + xx] {
                    eprintln!("✗ {tag} frame {k} Y mismatch ({xx},{yy}) [MB ({},{})]: {} vs JM {}", xx / 16, yy / 16, f.y[yy * fw + xx], jm[off + yy * fw + xx]);
                    return false;
                }
            }
        }
        // chroma
        for (plane, base) in [(&f.cb, fw * 1080), (&f.cr, fw * 1080 + cw * 540)] {
            for yy in 0..540 {
                for xx in 0..cw {
                    if plane[yy * cw + xx] != jm[off + base + yy * cw + xx] {
                        eprintln!("✗ {tag} frame {k} C mismatch ({xx},{yy})");
                        return false;
                    }
                }
            }
        }
        true
    };

    // Per-view sliding-window DPB, most-recent-first (decode order == PicNum
    // order for this all-P/no-reorder stream). L0 = these, decode_p_frame
    // takes the first num_ref_idx_l0_active by ref_idx.
    let max_ref = bsps.max_num_ref_frames.max(1) as usize;
    let mut base_dpb: Vec<Frame> = Vec::new();
    let mut dep_dpb: Vec<Frame> = Vec::new();
    let mut ok = true;
    let mut decoded = 0usize;
    for (k, au) in aus.iter().enumerate() {
        // ---- base view ---- (route by slice_type: I-slices — IDR or not —
        // decode intra; P-slices reference the DPB).
        use ripsaw::mvc::slice_header::parse_slice_header;
        let bsh = parse_slice_header(&mut BitReader::new(&au.base[0]), au.base_idr, au.base_idc, bsps, bpps)?;
        let base_intra = bsh.slice_type % 5 == 2;
        let bs: Vec<&[u8]> = au.base.iter().map(|s| s.as_slice()).collect();
        let base_res = if base_intra {
            decode_intra_frame(&bs, au.base_idc, au.base_idr, bsps, bpps).map(|mut f| {
                f.deblock_intra(bpps.chroma_qp_index_offset);
                f
            })
        } else {
            let refs: Vec<&Frame> = base_dpb.iter().collect();
            decode_p_frame(&bs, au.base_idc, false, bsps, bpps, &refs).map(|(mut f, mf)| {
                deblock_inter(&mut f, &mf, bpps.chroma_qp_index_offset);
                f
            })
        };
        let bf = match base_res {
            Ok(f) => f,
            Err(e) => {
                eprintln!("… stopped at frame {k} (base): {e}");
                break;
            }
        };
        // ---- dependent view ----
        // Build L0 by honoring the slice's ref_pic_list_modification: for this
        // stream every non-anchor dependent slice carries [InterViewAdd,
        // PicNumSub], i.e. L0 = [inter-view base, temporal previous dependent].
        // The anchor is inter-view only (L0 = [base]).
        let ds: Vec<&[u8]> = au.dep.iter().map(|s| s.as_slice()).collect();
        let dsh0 = ripsaw::mvc::slice_header::parse_slice_header(&mut BitReader::new(&au.dep[0]), au.dep_idr, au.dep_idc, dsps, dpps)?;
        let dnum = (dsh0.num_ref_idx_l0_active_minus1 + 1) as usize;
        let dl0 = build_dep_l0(&dsh0.ref_pic_list_modifications.list0, dnum, dep_dpb.len(), au.dep_anchor);
        let drefs: Vec<&Frame> = dl0
            .iter()
            .map(|r| match r {
                DRef::InterView => &bf,
                DRef::Temporal(i) => &dep_dpb[*i],
            })
            .collect();
        let df = match decode_p_frame(&ds, au.dep_idc, au.dep_idr, dsps, dpps, &drefs) {
            Ok((mut f, mf)) => {
                deblock_inter(&mut f, &mf, dpps.chroma_qp_index_offset);
                f
            }
            Err(e) => {
                eprintln!("… stopped at frame {k} (dep): {e}");
                break;
            }
        };
        ok &= cmp("base", &bf, &bjm, k) & cmp("dep", &df, &djm, k);
        if !ok {
            std::process::exit(1);
        }
        decoded += 1;

        // Update DPBs (sliding window, newest at front).
        base_dpb.insert(0, bf);
        base_dpb.truncate(max_ref);
        dep_dpb.insert(0, df);
        dep_dpb.truncate(max_ref);
    }
    eprintln!("✓ MVC clip: both views, {decoded}/{} frames decoded bit-exact vs JM (temporal + inter-view)", aus.len());
    if decoded == 0 {
        std::process::exit(1);
    }
    Ok(())
}
