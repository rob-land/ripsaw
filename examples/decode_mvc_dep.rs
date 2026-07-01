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

    // Dependent anchor slice 0: a P-slice, single ref = the base picture.
    let (idc, ref rbsp0) = dep_slices[0];
    {
        use ripsaw::mvc::slice_header::parse_slice_header;
        let mut r = BitReader::new(rbsp0);
        // The dependent ANCHOR is IDR-marked (non_idr_flag=0) even though it's
        // a P-slice — the IDR header layout (idr_pic_id + simple ref marking).
        let sh = parse_slice_header(&mut r, true, idc, dsps, dpps)?;
        eprintln!(
            "dep slice0 hdr: type {} first_mb {} qp_delta {} slice_qp {} num_ref_l0 {:?} cabac_idc {:?} header_bits {}",
            sh.slice_type, sh.first_mb_in_slice, sh.slice_qp_delta, 26 + dpps.pic_init_qp_minus26 + sh.slice_qp_delta,
            sh.num_ref_idx_l0_active_minus1, sh.cabac_init_idc, r.position_bits()
        );
    }
    let (pf, _mf) = decode_p_frame(rbsp0, idc, true, dsps, dpps, &base)?;
    eprintln!("decode_p_frame nonzero luma pixels: {}", pf.y.iter().filter(|&&p| p != 0).count());

    // Slice 0 covers first_mb_in_slice=0 .. next slice; validate the rows it
    // filled (non-zero) against the dependent-view ground truth.
    let djm = std::fs::read(&dep_truth)?;
    let mut matched = 0;
    let mut bad = None;
    'outer: for y in 0..1080 {
        if pf.y[y * fw..(y + 1) * fw].iter().all(|&p| p == 0) {
            break;
        }
        for x in 0..fw {
            if pf.y[y * fw + x] != djm[y * fw + x] {
                bad = Some((x, y, pf.y[y * fw + x], djm[y * fw + x]));
                break 'outer;
            }
        }
        matched += 1;
    }
    let _ = cw;
    match bad {
        Some((x, y, g, j)) => eprintln!("✗ dependent slice 0: {matched} luma rows match JM, then ({x},{y}) [MB ({},{})]: libmvc {g} vs JM {j}", x / 16, y / 16),
        None => eprintln!("✓ dependent anchor slice 0: all {matched} luma rows match JM — INTER-VIEW prediction bit-exact"),
    }
    Ok(())
}
