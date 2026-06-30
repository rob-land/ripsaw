// Probe: how close is libmvc's intra decoder to real 1080p Blu-ray content?
// Decode the FIRST base-view IDR slice of a real MVC stream and diff the region
// it covers against JM's per-view ground truth. The first slice has
// first_mb_in_slice = 0, so it decodes standalone (no cross-slice neighbour
// issue) — this isolates "does the intra decode handle real content" from the
// multi-slice work. Reports how far it gets.
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

    for nal in NalSplitter::new(&data) {
        if nal.is_empty() {
            continue;
        }
        let Ok((hdr, consumed)) = parse_nal_unit_header(nal) else { continue };
        let rbsp = extract_rbsp(&nal[consumed..]);
        match hdr.nal_unit_type {
            7 => {
                let s = parse_seq_parameter_set_data(&mut BitReader::new(&rbsp))?;
                eprintln!("SPS: {}x{} (mbs {}x{}), chroma {}, profile {}", s.pic_width_in_mbs * 16, s.pic_height_in_map_units * 16, s.pic_width_in_mbs, s.pic_height_in_map_units, s.chroma_format_idc, s.profile_idc);
                sps = Some(s);
            }
            8 => {
                let chroma = sps.as_ref().map(|s| s.chroma_format_idc).unwrap_or(1);
                pps = Some(parse_pic_parameter_set(&mut BitReader::new(&rbsp), chroma)?);
            }
            5 => {
                // First base-view IDR slice.
                let (sps, pps) = (sps.as_ref().unwrap(), pps.as_ref().unwrap());
                {
                    use ripsaw::mvc::slice_header::parse_slice_header;
                    let mut r = BitReader::new(&rbsp);
                    match parse_slice_header(&mut r, true, hdr.nal_ref_idc, sps, pps) {
                        Ok(sh) => {
                            let slice_qp = 26 + pps.pic_init_qp_minus26 + sh.slice_qp_delta;
                            eprintln!("slice hdr: first_mb {}, type {}, qp_delta {}, slice_qp {slice_qp} (pic_init_qp_minus26 {}), disable_deblock {}, header ends at bit {}", sh.first_mb_in_slice, sh.slice_type, sh.slice_qp_delta, pps.pic_init_qp_minus26, sh.disable_deblocking_filter_idc, r.position_bits());
                        }
                        Err(e) => eprintln!("slice header parse error: {e}"),
                    }
                }
                eprintln!("decoding first base IDR slice ({} bytes rbsp)…", rbsp.len());
                let frame = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| decode_intra_frame(&rbsp, hdr.nal_ref_idc, sps, pps))) {
                    Ok(Ok(f)) => f,
                    Ok(Err(e)) => {
                        eprintln!("✗ decode error: {e}");
                        std::process::exit(1);
                    }
                    Err(_) => {
                        eprintln!("✗ decode panicked (unsupported real-content feature)");
                        std::process::exit(1);
                    }
                };
                // The slice covers some MB rows from the top. Diff luma row by
                // row vs the ground truth (cropped to 1080) until the first
                // all-zero (undecoded) row, reporting how far it matched.
                let jm = std::fs::read(&truth)?;
                let nz = frame.y.iter().filter(|&&p| p != 0).count();
                eprintln!("frame.y nonzero pixels: {nz} / {}; first nonzero at {:?}", frame.y.len(), frame.y.iter().position(|&p| p != 0).map(|i| (i % frame.fw, i / frame.fw)));
                let fw = frame.fw;
                let mut matched_rows = 0;
                let mut first_bad = None;
                'outer: for y in 0..frame.fh.min(1080) {
                    let row = &frame.y[y * fw..(y + 1) * fw];
                    if row.iter().all(|&p| p == 0) {
                        break; // past the slice's extent
                    }
                    for x in 0..fw {
                        if frame.y[y * fw + x] != jm[y * fw + x] {
                            first_bad = Some((x, y, frame.y[y * fw + x], jm[y * fw + x]));
                            break 'outer;
                        }
                    }
                    matched_rows += 1;
                }
                if let Some((x, y, g, j)) = first_bad {
                    eprintln!("✗ first base slice: {matched_rows} luma rows match JM, then mismatch at ({x},{y}): libmvc {g} vs JM {j}");
                } else {
                    eprintln!("✓ first base slice: all {matched_rows} decoded luma rows match JM ground truth (pre-deblock vs JM final — deblock not yet applied here)");
                }
                break;
            }
            _ => {}
        }
    }
    Ok(())
}
