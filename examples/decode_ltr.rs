// Single-view H.264 decode with a proper DPB (short-term + long-term
// references, § 8.2.4 list construction via `reflist` + § 8.2.5 marking)
// against JM ldecod. Exercises long-term reference handling — the JM encoder's
// `SetFirstAsLongTerm` keeps the IDR as a long-term reference that late P frames
// still point at, which a plain sliding-window DPB would have evicted.
//
//   decode_ltr <stream.264> <ldecod_output.yuv>
//
// Resolution is read from the SPS. Frames are single-slice, no AUDs (each VCL
// NAL is one picture); decode order == display order for the IPPP test streams.
// Selects the PPS by pic_parameter_set_id (lencod emits several PPS variants).
//
// Decodes the JM lencod `SetFirstAsLongTerm` stream (~/ltr/ltr.264) bit-exact vs
// ldecod — this exercises the long-term reference DPB path AND the cabac_init_idc
// 1/2 context-init tables (that stream switches cabac_init_idc mid-sequence).
// MMCO-marked streams still bail (see reflist unit tests for the MMCO logic).

use ripsaw::mvc::annexb::NalSplitter;
use ripsaw::mvc::bitstream::BitReader;
use ripsaw::mvc::nal::parse_nal_unit_header;
use ripsaw::mvc::pps::{parse_pic_parameter_set, Pps};
use ripsaw::mvc::rbsp::extract_rbsp;
use ripsaw::mvc::recon::{decode_intra_frame, Frame};
use ripsaw::mvc::recon_inter::{deblock_inter, decode_p_frame};
use ripsaw::mvc::reflist::{init_p_list0, sliding_window_victim, DpbRef};
use ripsaw::mvc::slice_header::parse_slice_header;
use ripsaw::mvc::sps::{parse_seq_parameter_set_data, Sps};

fn main() -> anyhow::Result<()> {
    let data = std::fs::read(std::env::args().nth(1).unwrap())?;
    let jm = std::fs::read(std::env::args().nth(2).unwrap())?;
    let mut sps: Option<Sps> = None;
    // The JM encoder emits several PPS variants (differing weighted_pred_flag
    // etc.) and each slice selects one by pic_parameter_set_id — so track PPS by
    // id, not just the most recent.
    let mut pps_map: std::collections::HashMap<u32, Pps> = std::collections::HashMap::new();

    struct Pic {
        idr: bool,
        idc: u8,
        rbsp: Vec<u8>,
    }
    let mut pics: Vec<Pic> = Vec::new();
    for nal in NalSplitter::new(&data) {
        if nal.is_empty() {
            continue;
        }
        let Ok((h, c)) = parse_nal_unit_header(nal) else { continue };
        let rbsp = extract_rbsp(&nal[c..]);
        match h.nal_unit_type {
            7 => sps = Some(parse_seq_parameter_set_data(&mut BitReader::new(&rbsp))?),
            8 => {
                let p = parse_pic_parameter_set(&mut BitReader::new(&rbsp), sps.as_ref().unwrap().chroma_format_idc)?;
                pps_map.insert(p.pic_parameter_set_id, p);
            }
            1 | 5 => pics.push(Pic { idr: h.nal_unit_type == 5, idc: h.nal_ref_idc, rbsp: rbsp.to_vec() }),
            _ => {}
        }
    }
    let sps = sps.as_ref().unwrap();
    let (w, h) = (sps.width as usize, sps.height as usize);
    let max_frame_num = 1i32 << (sps.log2_max_frame_num_minus4 + 4);
    let max_lsb = 1i32 << (sps.log2_max_pic_order_cnt_lsb_minus4 + 4);
    let num_ref_frames = sps.max_num_ref_frames.max(1) as usize;

    // DPB: reference marking record + the decoded (deblocked) frame, in lockstep.
    let mut dpb: Vec<DpbRef> = Vec::new();
    let mut frames: Vec<Frame> = Vec::new();
    let mut out: Vec<Frame> = Vec::new();
    let (mut prev_msb, mut prev_lsb) = (0i32, 0i32);

    for pic in pics.iter() {
        let slices = [pic.rbsp.as_slice()];
        // The slice's pic_parameter_set_id (first_mb, slice_type, pps_id are all
        // ue and PPS-independent) selects which PPS this slice uses.
        let pps_id = {
            let mut r = BitReader::new(&pic.rbsp);
            let _first_mb = r.read_ue()?;
            let _slice_type = r.read_ue()?;
            r.read_ue()?
        };
        let pps = pps_map.get(&pps_id).ok_or_else(|| anyhow::anyhow!("slice references unknown PPS id {pps_id}"))?;
        let sh = parse_slice_header(&mut BitReader::new(&pic.rbsp), pic.idr, pic.idc, sps, pps)?;
        let frame_num = sh.frame_num as i32;
        let lsb = sh.pic_order_cnt_lsb.unwrap_or(0) as i32;
        if pic.idr {
            dpb.clear();
            frames.clear();
            prev_msb = 0;
            prev_lsb = 0;
        }
        let msb = if pic.idr {
            0
        } else if lsb < prev_lsb && prev_lsb - lsb >= max_lsb / 2 {
            prev_msb + max_lsb
        } else if lsb > prev_lsb && lsb - prev_lsb > max_lsb / 2 {
            prev_msb - max_lsb
        } else {
            prev_msb
        };
        let poc = msb + lsb;
        if pic.idc != 0 {
            prev_msb = msb;
            prev_lsb = lsb;
        }

        let st = sh.slice_type % 5;
        let frame = if st == 2 {
            let mut f = decode_intra_frame(&slices, pic.idc, pic.idr, sps, pps)?;
            f.deblock_intra(pps.chroma_qp_index_offset);
            f
        } else if st == 0 {
            // RefPicList0 (§ 8.2.4.2.1), truncated to num_ref_idx_l0_active. No
            // ref_pic_list_modification in these streams (asserted).
            anyhow::ensure!(
                sh.ref_pic_list_modifications.list0.as_deref().unwrap_or(&[]).is_empty(),
                "ref_pic_list_modification (reordering) not handled by this harness"
            );
            let order = init_p_list0(&dpb, frame_num, max_frame_num);
            let num_active = (sh.num_ref_idx_l0_active_minus1 + 1) as usize;
            anyhow::ensure!(order.len() >= num_active, "DPB has fewer refs ({}) than num_ref_idx_l0_active ({num_active})", order.len());
            let refs: Vec<&Frame> = order.iter().take(num_active).map(|&i| &frames[i]).collect();
            let (mut f, mf) = decode_p_frame(&slices, pic.idc, pic.idr, sps, pps, &refs)?;
            deblock_inter(&mut f, &mf, pps.chroma_qp_index_offset);
            f
        } else {
            anyhow::bail!("B slice unexpected in this LTR test stream");
        };

        // Decoded-reference-picture marking (§ 8.2.5) for reference pictures.
        if pic.idc != 0 {
            let mut current_lt: Option<i32> = None;
            if pic.idr {
                if sh.long_term_reference_flag {
                    current_lt = Some(0);
                }
            } else {
                // MMCO marking (apply_mmco) is covered by reflist unit tests; the
                // LTR test streams use sliding-window marking only, so keep the
                // parallel frames vec trivially in sync and bail on MMCO here.
                anyhow::ensure!(sh.mmco.is_empty(), "MMCO marking not handled by this harness (see reflist unit tests)");
                if let Some(v) = sliding_window_victim(&dpb, frame_num, max_frame_num, num_ref_frames) {
                    dpb.remove(v);
                    frames.remove(v);
                }
            }
            let rec = match current_lt {
                Some(idx) => DpbRef { frame_num, poc, long_term: true, long_term_frame_idx: idx },
                None => DpbRef::short(frame_num, poc),
            };
            dpb.push(rec);
            frames.push(clone_frame(&frame));
        }
        out.push(frame);
    }

    // Compare (decode order == display order for these IPPP streams).
    let (cw, ch) = (w / 2, h / 2);
    let fsz = w * h + 2 * cw * ch;
    for (i, f) in out.iter().enumerate() {
        let off = i * fsz;
        if off + fsz > jm.len() {
            eprintln!("frame {i} beyond JM output; stopping");
            break;
        }
        if !cmp_frame(f, &jm, off, w, h) {
            eprintln!("✗ frame {i} mismatch");
            std::process::exit(1);
        }
    }
    eprintln!("✓ {} frames decoded bit-exact vs JM (long-term references)", out.len());
    Ok(())
}

fn cmp_frame(f: &Frame, jm: &[u8], off: usize, w: usize, h: usize) -> bool {
    let (cw, ch) = (w / 2, h / 2);
    for yy in 0..h {
        for xx in 0..w {
            if f.y[yy * f.fw + xx] != jm[off + yy * w + xx] {
                eprintln!("  Y ({xx},{yy}) MB({},{}): {} vs {}", xx / 16, yy / 16, f.y[yy * f.fw + xx], jm[off + yy * w + xx]);
                return false;
            }
        }
    }
    for (plane, base) in [(&f.cb, w * h), (&f.cr, w * h + cw * ch)] {
        for yy in 0..ch {
            for xx in 0..cw {
                if plane[yy * f.cw + xx] != jm[off + base + yy * cw + xx] {
                    eprintln!("  C ({xx},{yy})");
                    return false;
                }
            }
        }
    }
    true
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
