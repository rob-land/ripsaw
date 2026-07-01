// Decode the full base-view IDR of a real MVC stream (6 slices at 1080p) with
// libmvc's multi-slice intra decoder, and diff the whole frame against JM's
// per-view ground truth. Validates real 1920x1080 Blu-ray intra content: the
// custom scaling matrix, the I_8x8 modes, and cross-slice neighbour
// unavailability.
//
//   cargo run --release --example decode_mvc_base -- au0.h264 mvc_ViewId0000.yuv

use ripsaw::mvc::annexb::NalSplitter;
use ripsaw::mvc::bitstream::BitReader;
use ripsaw::mvc::nal::parse_nal_unit_header;
use ripsaw::mvc::pps::{parse_pic_parameter_set, Pps};
use ripsaw::mvc::rbsp::extract_rbsp;
use ripsaw::mvc::recon::decode_intra_frame;
use ripsaw::mvc::sps::{parse_seq_parameter_set_data, Sps};

fn main() -> anyhow::Result<()> {
    let h264 = std::env::args().nth(1).unwrap();
    let truth = std::env::args().nth(2).unwrap();
    let data = std::fs::read(&h264)?;
    let (mut sps, mut pps): (Option<Sps>, Option<Pps>) = (None, None);

    // Collect the base view's IDR slices (NAL 5); stop at the dependent view.
    let mut base_slices: Vec<Vec<u8>> = Vec::new();
    let mut nal_ref_idc = 0;
    for nal in NalSplitter::new(&data) {
        if nal.is_empty() {
            continue;
        }
        let Ok((hdr, consumed)) = parse_nal_unit_header(nal) else { continue };
        let rbsp = extract_rbsp(&nal[consumed..]);
        match hdr.nal_unit_type {
            7 => sps = Some(parse_seq_parameter_set_data(&mut BitReader::new(&rbsp))?),
            8 if sps.is_some() && pps.is_none() => {
                let chroma = sps.as_ref().unwrap().chroma_format_idc;
                pps = Some(parse_pic_parameter_set(&mut BitReader::new(&rbsp), chroma)?);
            }
            5 => {
                nal_ref_idc = hdr.nal_ref_idc;
                base_slices.push(rbsp.to_vec());
            }
            15 | 20 => break, // dependent view — base access unit done
            _ => {}
        }
    }

    let (sps, pps) = (sps.as_ref().unwrap(), pps.as_ref().unwrap());
    eprintln!("base view: {} slices, {}x{}", base_slices.len(), sps.pic_width_in_mbs * 16, sps.pic_height_in_map_units * 16);
    let refs: Vec<&[u8]> = base_slices.iter().map(|s| s.as_slice()).collect();
    let frame = decode_intra_frame(&refs, nal_ref_idc, true, sps, pps)?;

    // Diff the whole luma+chroma frame (cropped to the coded 1080) vs JM.
    let jm = std::fs::read(&truth)?;
    let (fw, cw, ch) = (frame.fw, frame.cw, frame.ch);
    let ok = |name: &str, got: &[u8], w: usize, h: usize, off: usize| -> bool {
        for yy in 0..h {
            for xx in 0..w {
                if got[yy * w + xx] != jm[off + yy * w + xx] {
                    eprintln!("✗ {name} mismatch at ({xx},{yy}) [MB ({},{})]: libmvc {} vs JM {}", xx / 16, yy / 16, got[yy * w + xx], jm[off + yy * w + xx]);
                    return false;
                }
            }
        }
        eprintln!("  ✓ {name}: {h}×{w} match JM");
        true
    };
    let ( jysz, jcsz) = (fw * 1080, cw * 540);
    let oy = ok("Y", &frame.y, fw, 1080, 0);
    let ou = ok("U", &frame.cb, cw, 540, jysz);
    let ov = ok("V", &frame.cr, cw, 540, jysz + jcsz);
    let _ = ch;
    if oy && ou && ov {
        eprintln!("✓ full base IDR ({}×1080, {} slices) decoded by libmvc matches JM ground truth", fw, refs.len());
    } else {
        std::process::exit(1);
    }
    Ok(())
}
