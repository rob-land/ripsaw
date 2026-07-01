// Temporal multi-frame MVC decode with a minimal DPB. The base view is a
// single-reference P chain (I P P P …, each frame predicts from the previous),
// so no ref_idx / multi-ref list is needed yet — just keep the last decoded
// (deblocked) base frame as the reference. Decodes N access units of the base
// view and diffs each frame against JM's per-view ground truth. (Dependent
// temporal frames — 2 refs, inter-view + temporal — come next.)
//
//   cargo run --release --example decode_mvc_clip -- au012.h264 mvc2_ViewId0000.yuv

use ripsaw::mvc::annexb::NalSplitter;
use ripsaw::mvc::bitstream::BitReader;
use ripsaw::mvc::nal::parse_nal_unit_header;
use ripsaw::mvc::pps::{parse_pic_parameter_set, Pps};
use ripsaw::mvc::rbsp::extract_rbsp;
use ripsaw::mvc::recon::{decode_intra_frame, Frame};
use ripsaw::mvc::recon_inter::{deblock_inter, decode_p_frame};
use ripsaw::mvc::sps::{parse_seq_parameter_set_data, Sps};

#[derive(Default)]
struct Au {
    base: Vec<Vec<u8>>,   // base view slice rbsps (NAL 1/5)
    base_idr: bool,
    base_ref_idc: u8,
}

fn main() -> anyhow::Result<()> {
    let h264 = std::env::args().nth(1).unwrap();
    let base_truth = std::env::args().nth(2).unwrap();
    let data = std::fs::read(&h264)?;
    let (mut sps, mut pps): (Option<Sps>, Option<Pps>) = (None, None);

    // Group base-view slices into access units (split on AUD, NAL 9).
    let mut aus: Vec<Au> = Vec::new();
    for nal in NalSplitter::new(&data) {
        if nal.is_empty() {
            continue;
        }
        let Ok((hdr, consumed)) = parse_nal_unit_header(nal) else { continue };
        let rbsp = extract_rbsp(&nal[consumed..]);
        match hdr.nal_unit_type {
            9 => aus.push(Au::default()),
            7 => sps = Some(parse_seq_parameter_set_data(&mut BitReader::new(&rbsp))?),
            8 if pps.is_none() => {
                pps = Some(parse_pic_parameter_set(&mut BitReader::new(&rbsp), sps.as_ref().unwrap().chroma_format_idc)?);
            }
            5 | 1 => {
                let au = aus.last_mut().unwrap();
                au.base_idr = hdr.nal_unit_type == 5;
                au.base_ref_idc = hdr.nal_ref_idc;
                au.base.push(rbsp.to_vec());
            }
            _ => {}
        }
    }
    aus.retain(|a| !a.base.is_empty());

    let (sps, pps) = (sps.as_ref().unwrap(), pps.as_ref().unwrap());
    let fw = sps.pic_width_in_mbs as usize * 16;
    let cw = fw / 2;
    let jm = std::fs::read(&base_truth)?;
    let fsz = fw * 1080 + 2 * (cw * 540);

    let mut prev: Option<Frame> = None;
    let mut all_ok = true;
    for (k, au) in aus.iter().enumerate() {
        let slices: Vec<&[u8]> = au.base.iter().map(|s| s.as_slice()).collect();
        let mut frame = if au.base_idr {
            decode_intra_frame(&slices, au.base_ref_idc, sps, pps)?
        } else {
            let reff = prev.as_ref().expect("P frame before any IDR");
            let (mut f, mf) = decode_p_frame(&slices, au.base_ref_idc, false, sps, pps, reff)?;
            deblock_inter(&mut f, &mf, pps.chroma_qp_index_offset);
            f
        };
        if au.base_idr {
            frame.deblock_intra(pps.chroma_qp_index_offset);
        }

        // Diff vs JM frame k (decode order == POC order for an all-P chain).
        let off = k * fsz;
        let mut ok = true;
        'chk: for yy in 0..1080 {
            for xx in 0..fw {
                if frame.y[yy * fw + xx] != jm[off + yy * fw + xx] {
                    eprintln!("✗ base frame {k} ({}) Y mismatch ({xx},{yy}) [MB ({},{})]: {} vs JM {}", if au.base_idr { "IDR" } else { "P" }, xx / 16, yy / 16, frame.y[yy * fw + xx], jm[off + yy * fw + xx]);
                    ok = false;
                    break 'chk;
                }
            }
        }
        if ok {
            eprintln!("✓ base frame {k} ({}) Y matches JM", if au.base_idr { "IDR" } else { "P" });
        }
        all_ok &= ok;
        prev = Some(frame);
    }
    if all_ok {
        eprintln!("✓ base view temporal chain ({} frames) decoded bit-exact vs JM", aus.len());
    } else {
        std::process::exit(1);
    }
    Ok(())
}
