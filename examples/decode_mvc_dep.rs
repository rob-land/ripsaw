// First inter-view (Annex G) prediction on real 3D Blu-ray data: decode the
// base IDR (view 0) with libmvc, then decode the dependent-view anchor's first
// slice (view 1, a P-slice whose single reference is the base picture) using
// the base frame as the inter-view reference, and diff vs JM's dependent-view
// ground truth. Slice 0 (first_mb_in_slice = 0) needs no multi-slice, so this
// isolates the inter-view MC.
//
//   cargo run --release --example decode_mvc_dep -- au0.h264 mvc_ViewId0000.yuv mvc_ViewId0001.yuv

use ripsaw::mvc::annexb::NalSplitter;
use ripsaw::mvc::bitstream::BitReader;
use ripsaw::mvc::nal::parse_nal_unit_header;
use ripsaw::mvc::pps::{parse_pic_parameter_set, Pps};
use ripsaw::mvc::rbsp::extract_rbsp;
use ripsaw::mvc::recon::decode_intra_frame;
use ripsaw::mvc::recon_inter::decode_p_frame;
use ripsaw::mvc::sps::{parse_seq_parameter_set_data, parse_subset_sps_rbsp, Sps};

fn main() -> anyhow::Result<()> {
    let h264 = std::env::args().nth(1).unwrap();
    let base_truth = std::env::args().nth(2).unwrap();
    let dep_truth = std::env::args().nth(3).unwrap();
    let data = std::fs::read(&h264)?;
    let (mut base_sps, mut base_pps): (Option<Sps>, Option<Pps>) = (None, None);
    let (mut dep_sps, mut dep_pps): (Option<Sps>, Option<Pps>) = (None, None);
    let mut seen_subset = false;

    let mut base_slices: Vec<Vec<u8>> = Vec::new();
    let mut dep_slices: Vec<(u8, Vec<u8>)> = Vec::new();
    let mut base_ref_idc = 0;
    for nal in NalSplitter::new(&data) {
        if nal.is_empty() {
            continue;
        }
        let Ok((hdr, consumed)) = parse_nal_unit_header(nal) else { continue };
        let rbsp = extract_rbsp(&nal[consumed..]);
        match hdr.nal_unit_type {
            7 => base_sps = Some(parse_seq_parameter_set_data(&mut BitReader::new(&rbsp))?),
            15 => {
                dep_sps = Some(parse_subset_sps_rbsp(&rbsp)?.sps);
                seen_subset = true;
            }
            8 => {
                // PPS before the subset SPS = base's; after = dependent's.
                let chroma = base_sps.as_ref().map(|s| s.chroma_format_idc).unwrap_or(1);
                let p = parse_pic_parameter_set(&mut BitReader::new(&rbsp), chroma)?;
                if seen_subset {
                    dep_pps = Some(p);
                } else {
                    base_pps = Some(p);
                }
            }
            5 => {
                base_ref_idc = hdr.nal_ref_idc;
                base_slices.push(rbsp.to_vec());
            }
            20 => dep_slices.push((hdr.nal_ref_idc, rbsp.to_vec())),
            _ => {}
        }
    }

    let (sps, pps) = (base_sps.as_ref().unwrap(), base_pps.as_ref().unwrap());
    let (dsps, dpps) = (dep_sps.as_ref().unwrap(), dep_pps.as_ref().unwrap());
    // Base view (inter-view reference). disable_deblock=1 for this stream, so
    // deblock_intra is a no-op — the pre-deblock frame is the reference.
    let refs: Vec<&[u8]> = base_slices.iter().map(|s| s.as_slice()).collect();
    let mut base = decode_intra_frame(&refs, base_ref_idc, sps, pps)?;
    base.deblock_intra(pps.chroma_qp_index_offset);
    let (fw, cw) = (base.fw, base.cw);
    // Sanity: base matches its ground truth.
    let bjm = std::fs::read(&base_truth)?;
    let bok = base.y[..fw * 1080].iter().zip(&bjm[..fw * 1080]).all(|(a, b)| a == b);
    eprintln!("base view (inter-view ref) matches JM: {bok}");

    // Dependent anchor: a multi-slice P-frame whose single L0 ref is the base
    // picture (inter-view). The anchor is IDR-marked (non_idr_flag=0) even
    // though slice_type=P — the IDR header layout.
    let idc = dep_slices[0].0;
    let dref: Vec<&[u8]> = dep_slices.iter().map(|(_, s)| s.as_slice()).collect();
    eprintln!("dependent anchor: {} slices", dref.len());
    let (pf, _mf) = decode_p_frame(&dref, idc, true, dsps, dpps, &base)?;

    // Diff the whole dependent frame (luma + chroma, cropped to 1080) vs JM.
    let djm = std::fs::read(&dep_truth)?;
    let (jysz, jcsz) = (fw * 1080, cw * 540);
    let chk = |name: &str, got: &[u8], w: usize, h: usize, off: usize| -> bool {
        for yy in 0..h {
            for xx in 0..w {
                if got[yy * w + xx] != djm[off + yy * w + xx] {
                    eprintln!("✗ dep {name} mismatch at ({xx},{yy}) [MB ({},{})]: libmvc {} vs JM {}", xx / 16, yy / 16, got[yy * w + xx], djm[off + yy * w + xx]);
                    return false;
                }
            }
        }
        eprintln!("  ✓ dep {name}: {h}×{w} match JM");
        true
    };
    let oy = chk("Y", &pf.y, fw, 1080, 0);
    let ou = chk("U", &pf.cb, cw, 540, jysz);
    let ov = chk("V", &pf.cr, cw, 540, jysz + jcsz);
    if oy && ou && ov {
        eprintln!("✓ FULL dependent view (view 1, {} slices) decoded by libmvc via inter-view prediction — bit-exact vs JM", dref.len());
    } else {
        std::process::exit(1);
    }
    Ok(())
}
