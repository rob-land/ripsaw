// Full both-view IBP MVC clip decode (I/P/B, multi-slice, inter-view) with a
// POC-ordered DPB, validated post-deblock vs JM's per-view display-order
// output. The prototype for B-slice support in mvc::clip.
//
//   decode_bclip_mvc -- clip.h264 out_ViewId0000.yuv out_ViewId0001.yuv
//
// Base view: I → intra, P/B → temporal. Dependent view: the anchor (a P
// inter-view-predicted from the base at the same AU) starts each GOP; later
// P/B frames are temporal from the dependent DPB (num_ref 1, empty
// ref_pic_list_modification → default temporal-first list).

use ripsaw::mvc::annexb::NalSplitter;
use ripsaw::mvc::bitstream::BitReader;
use ripsaw::mvc::nal::parse_nal_unit_header;
use ripsaw::mvc::pps::{parse_pic_parameter_set, Pps};
use ripsaw::mvc::rbsp::extract_rbsp;
use ripsaw::mvc::recon::{decode_intra_frame, Frame};
use ripsaw::mvc::recon_inter::{deblock_b, deblock_inter, decode_b_frame, decode_p_frame, MotionField};
use ripsaw::mvc::slice_header::parse_slice_header;
use ripsaw::mvc::sps::{parse_seq_parameter_set_data, parse_subset_sps_rbsp, Sps};

const W: usize = 1920;
const H: usize = 1080;

#[derive(Default)]
struct Au {
    base: Vec<Vec<u8>>,
    base_idr: bool,
    base_idc: u8,
    dep: Vec<Vec<u8>>,
    dep_idc: u8,
    dep_idr: bool,
    dep_anchor: bool,
}

/// One view's short-term reference: POC, deblocked frame, L0 motion field.
type Ref = (i32, Frame, MotionField);

fn main() -> anyhow::Result<()> {
    let data = std::fs::read(std::env::args().nth(1).unwrap())?;
    let bjm = std::fs::read(std::env::args().nth(2).unwrap())?;
    let djm = std::fs::read(std::env::args().nth(3).unwrap())?;
    let (mut bsps, mut bpps, mut dsps, mut dpps): (Option<Sps>, Option<Pps>, Option<Sps>, Option<Pps>) = (None, None, None, None);
    let mut seen_subset = false;
    let mut aus: Vec<Au> = Vec::new();
    for nal in NalSplitter::new(&data) {
        if nal.is_empty() {
            continue;
        }
        let Ok((h, c)) = parse_nal_unit_header(nal) else { continue };
        let rbsp = extract_rbsp(&nal[c..]);
        match h.nal_unit_type {
            9 => aus.push(Au::default()),
            7 => bsps = Some(parse_seq_parameter_set_data(&mut BitReader::new(&rbsp))?),
            15 => {
                dsps = Some(parse_subset_sps_rbsp(&rbsp)?.sps);
                seen_subset = true;
            }
            8 => {
                let chroma = bsps.as_ref().map(|s| s.chroma_format_idc).unwrap_or(1);
                let p = parse_pic_parameter_set(&mut BitReader::new(&rbsp), chroma)?;
                if seen_subset { dpps = Some(p) } else { bpps = Some(p) }
            }
            5 | 1 => {
                let au = aus.last_mut().unwrap();
                au.base_idr = h.nal_unit_type == 5;
                au.base_idc = h.nal_ref_idc;
                au.base.push(rbsp.to_vec());
            }
            20 => {
                let au = aus.last_mut().unwrap();
                au.dep_idc = h.nal_ref_idc;
                if let Some(ext) = &h.mvc_extension {
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

    let max_lsb = 1i32 << (bsps.log2_max_pic_order_cnt_lsb_minus4 + 4);
    let mut brefs: Vec<Ref> = Vec::new();
    let mut drefs: Vec<Ref> = Vec::new();
    let mut bout: Vec<(usize, Frame)> = Vec::new();
    let mut dout: Vec<(usize, Frame)> = Vec::new();
    let mut gop_start = 0usize;
    let mut prev_gop = 0usize;
    let (mut pmsb, mut plsb) = (0i32, 0i32);

    for au in &aus {
        // POC (shared by both views — same AU).
        let bsh = parse_slice_header(&mut BitReader::new(&au.base[0]), au.base_idr, au.base_idc, bsps, bpps)?;
        let lsb = bsh.pic_order_cnt_lsb.unwrap_or(0) as i32;
        if au.base_idr {
            gop_start += prev_gop;
            prev_gop = 0;
            brefs.clear();
            drefs.clear();
            pmsb = 0;
            plsb = 0;
        }
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
        prev_gop = prev_gop.max(poc as usize / 2 + 1);
        let disp = gop_start + poc as usize / 2;

        // ---- base view ----
        let bslices: Vec<&[u8]> = au.base.iter().map(|s| s.as_slice()).collect();
        let bst = bsh.slice_type % 5;
        let bf = decode_view(&bslices, au.base_idr, au.base_idc, bst, poc, bsps, bpps, &brefs, None)?;
        if au.base_idc != 0 {
            brefs.push((poc, clone_frame(&bf.0), bf.1.clone().unwrap_or_else(empty_mf)));
        }
        bout.push((disp, bf.0));

        // ---- dependent view ----
        let dslices: Vec<&[u8]> = au.dep.iter().map(|s| s.as_slice()).collect();
        let dsh = parse_slice_header(&mut BitReader::new(&au.dep[0]), au.dep_idr, au.dep_idc, dsps, dpps)?;
        let dst = dsh.slice_type % 5;
        // Anchor: inter-view from the base frame just decoded. Non-anchor:
        // temporal from the dependent DPB (like the base view).
        let inter_view = if au.dep_anchor { Some(brefs.last().map(|r| &r.1).unwrap_or(&bout.last().unwrap().1)) } else { None };
        let df = decode_view(&dslices, au.dep_idr, au.dep_idc, dst, poc, dsps, dpps, &drefs, inter_view)?;
        if au.dep_idc != 0 {
            drefs.push((poc, clone_frame(&df.0), df.1.clone().unwrap_or_else(empty_mf)));
        }
        dout.push((disp, df.0));
    }

    let ok_b = validate("base", &mut bout, &bjm);
    let ok_d = validate("dep", &mut dout, &djm);
    if ok_b && ok_d {
        eprintln!("✓ BOTH views: {} base + {} dep frames (I/P/B, inter-view) bit-exact vs JM post-deblock", bout.len(), dout.len());
    } else {
        std::process::exit(1);
    }
    Ok(())
}

/// Decode one view's frame (returns frame + its L0 motion field if inter).
/// `inter_view` = the base frame for a dependent anchor (inter-view L0);
/// otherwise refs are temporal. B uses L0=nearest-past, L1=nearest-future.
#[allow(clippy::too_many_arguments)]
fn decode_view(
    slices: &[&[u8]],
    idr: bool,
    idc: u8,
    st: u32,
    poc: i32,
    sps: &Sps,
    pps: &Pps,
    refs: &[Ref],
    inter_view: Option<&Frame>,
) -> anyhow::Result<(Frame, Option<MotionField>)> {
    if st == 2 {
        let mut f = decode_intra_frame(slices, idc, idr, sps, pps)?;
        f.deblock_intra(pps.chroma_qp_index_offset);
        Ok((f, None))
    } else if st == 0 {
        // P: inter-view anchor uses [base]; else temporal nearest-past.
        let reff: &Frame = if let Some(iv) = inter_view {
            iv
        } else {
            &refs.iter().filter(|(p, ..)| *p < poc).max_by_key(|(p, ..)| *p).expect("P ref").1
        };
        let (mut f, mf) = decode_p_frame(slices, idc, idr, sps, pps, &[reff])?;
        deblock_inter(&mut f, &mf, pps.chroma_qp_index_offset);
        Ok((f, Some(mf)))
    } else {
        // B: temporal L0=nearest past, L1=nearest future; col = L1's motion.
        let l0 = refs.iter().filter(|(p, ..)| *p < poc).max_by_key(|(p, ..)| *p).expect("B L0");
        let l1 = refs.iter().filter(|(p, ..)| *p > poc).min_by_key(|(p, ..)| *p).expect("B L1");
        let (mut f, bmf) = decode_b_frame(slices, idc, idr, sps, pps, &[(&l0.1, l0.0)], &[(&l1.1, l1.0)], &l1.2, (32, 32))?;
        deblock_b(&mut f, &bmf, pps.chroma_qp_index_offset);
        Ok((f, None))
    }
}

fn validate(tag: &str, out: &mut [(usize, Frame)], jm: &[u8]) -> bool {
    let fsz = W * H + 2 * (W / 2) * (H / 2);
    out.sort_by_key(|(d, _)| *d);
    for (disp, f) in out.iter() {
        let off = disp * fsz;
        if off + fsz > jm.len() {
            continue;
        }
        if !cmp_frame(f, jm, off) {
            eprintln!("✗ {tag} display frame {disp} mismatch");
            return false;
        }
    }
    true
}

fn cmp_frame(f: &Frame, jm: &[u8], off: usize) -> bool {
    let (cw, ch) = (W / 2, H / 2);
    for yy in 0..H {
        for xx in 0..W {
            if f.y[yy * f.fw + xx] != jm[off + yy * W + xx] {
                return false;
            }
        }
    }
    for (plane, base) in [(&f.cb, W * H), (&f.cr, W * H + cw * ch)] {
        for yy in 0..ch {
            for xx in 0..cw {
                if plane[yy * f.cw + xx] != jm[off + base + yy * cw + xx] {
                    return false;
                }
            }
        }
    }
    true
}

fn empty_mf() -> MotionField {
    MotionField { mv: vec![], refidx: vec![], nz: vec![], bw4: 0, bh4: 0 }
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
