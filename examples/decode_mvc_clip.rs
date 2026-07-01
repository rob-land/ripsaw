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

#[derive(Default)]
struct Au {
    base: Vec<Vec<u8>>,
    base_idr: bool,
    base_idc: u8,
    dep: Vec<Vec<u8>>,
    dep_idc: u8,
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

    let mut prev_base: Option<Frame> = None;
    let mut prev_dep: Option<Frame> = None;
    let mut ok = true;
    for (k, au) in aus.iter().enumerate() {
        // ---- base view ----
        let bs: Vec<&[u8]> = au.base.iter().map(|s| s.as_slice()).collect();
        let mut bf = if au.base_idr {
            let mut f = decode_intra_frame(&bs, au.base_idc, bsps, bpps)?;
            f.deblock_intra(bpps.chroma_qp_index_offset);
            f
        } else {
            let (mut f, mf) = decode_p_frame(&bs, au.base_idc, false, bsps, bpps, &[prev_base.as_ref().unwrap()])?;
            deblock_inter(&mut f, &mf, bpps.chroma_qp_index_offset);
            f
        };
        ok &= cmp("base", &bf, &bjm, k);

        // ---- dependent view ----
        let ds: Vec<&[u8]> = au.dep.iter().map(|s| s.as_slice()).collect();
        let (mut df, mf) = if au.base_idr {
            // Anchor: inter-view from the base only (IDR-marked P).
            decode_p_frame(&ds, au.dep_idc, true, dsps, dpps, &[&bf])?
        } else {
            // Temporal: L0 = [previous dependent, current base (inter-view)].
            decode_p_frame(&ds, au.dep_idc, false, dsps, dpps, &[prev_dep.as_ref().unwrap(), &bf])?
        };
        deblock_inter(&mut df, &mf, dpps.chroma_qp_index_offset);
        ok &= cmp("dep", &df, &djm, k);

        eprintln!("frame {k}: base {} dep {}", if au.base_idr { "IDR" } else { "P" }, if au.base_idr { "anchor" } else { "P(2ref)" });
        prev_base = Some(bf);
        prev_dep = Some(df);
    }
    if ok {
        eprintln!("✓ MVC clip: both views, {} frames, decoded bit-exact vs JM (temporal + inter-view)", aus.len());
    } else {
        std::process::exit(1);
    }
    Ok(())
}
