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
use ripsaw::mvc::nal::parse_nal_unit_header;
use ripsaw::mvc::pps::{parse_pic_parameter_set, Pps};
use ripsaw::mvc::rbsp::extract_rbsp;
use ripsaw::mvc::recon::{decode_intra_frame, Frame};
use ripsaw::mvc::recon_inter::{deblock_inter, decode_b_frame, decode_p_frame, MotionField};
use ripsaw::mvc::slice_header::parse_slice_header;
use ripsaw::mvc::sps::{parse_seq_parameter_set_data, Sps};

/// Implicit weighted bi-pred weights (§ 8.4.2.3.2) from the POC distances;
/// w0/w1 sum to 64. Falls back to (32,32) (simple average) when out of range.
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
                let mut f = decode_intra_frame(&[&rbsp[..]], hdr.nal_ref_idc, true, sps, pps)?;
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
                    let (mut pf, mf) = decode_p_frame(&[&rbsp[..]], hdr.nal_ref_idc, false, sps, pps, &[reff])?;
                    deblock_inter(&mut pf, &mf, pps.chroma_qp_index_offset);
                    refs.push((poc, pf));
                    col = Some(mf);
                    slice_no += 1;
                } else {
                    // First B-slice (POC 2): reconstruct it via the library.
                    eprintln!("B-slice POC {poc}: reconstructing");
                    let l0e = refs.iter().filter(|(p, _)| *p < poc).max_by_key(|(p, _)| *p).expect("past ref");
                    let l1e = refs.iter().filter(|(p, _)| *p > poc).min_by_key(|(p, _)| *p).expect("future ref");
                    let wt = implicit_weights(l0e.0, l1e.0, poc);
                    let (frame, _bmf) = decode_b_frame(&[&rbsp[..]], hdr.nal_ref_idc, false, sps, pps, &[(&l0e.1, l0e.0)], &[(&l1e.1, l1e.0)], poc, col.as_ref().unwrap(), wt)?;
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
