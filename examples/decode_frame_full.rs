// Full intra-frame reconstruction for every MB type (I_4x4 / I_8x8 /
// I_16x16 + chroma residual), driven entirely by the library:
// `recon::decode_intra_frame` does the CABAC decode + prediction + residual +
// clip, and `Frame::deblock_intra` runs the in-loop filter (§ 8.7). This
// example just wires them to JM ground truth — the capstone validation of the
// intra decoder AND the regression guard for the library refactor.
//
//   cargo run --release --example decode_frame_full -- test.h264 test_predeblock.bin
//   POST=<jm_output.yuv> cargo run ... decode_frame_full -- test.h264 test_predeblock.bin

use ripsaw::mvc::annexb::NalSplitter;
use ripsaw::mvc::bitstream::BitReader;
use ripsaw::mvc::nal::parse_nal_unit_header;
use ripsaw::mvc::pps::{parse_pic_parameter_set, Pps};
use ripsaw::mvc::rbsp::extract_rbsp;
use ripsaw::mvc::recon::decode_intra_frame;
use ripsaw::mvc::sps::{parse_seq_parameter_set_data, Sps};

fn main() -> anyhow::Result<()> {
    let h264 = std::env::args().nth(1).unwrap();
    let ref_path = std::env::args().nth(2).unwrap();
    let data = std::fs::read(&h264)?;
    let (mut sps, mut pps): (Option<Sps>, Option<Pps>) = (None, None);

    for nal in NalSplitter::new(&data) {
        if nal.is_empty() {
            continue;
        }
        let Ok((hdr, consumed)) = parse_nal_unit_header(nal) else { continue };
        if hdr.mvc_extension.as_ref().map(|e| e.view_id != 0).unwrap_or(false) {
            continue;
        }
        let rbsp = extract_rbsp(&nal[consumed..]);
        match hdr.nal_unit_type {
            7 => sps = Some(parse_seq_parameter_set_data(&mut BitReader::new(&rbsp))?),
            8 => {
                let chroma = sps.as_ref().map(|s| s.chroma_format_idc).unwrap_or(1);
                pps = Some(parse_pic_parameter_set(&mut BitReader::new(&rbsp), chroma)?);
            }
            5 => {
                let (sps, pps) = (sps.as_ref().unwrap(), pps.as_ref().unwrap());
                let mut frame = decode_intra_frame(&rbsp, hdr.nal_ref_idc, sps, pps)?;
                let (fw, fh, cw, ch) = (frame.fw, frame.fh, frame.cw, frame.ch);
                let (ysz, csz) = (fw * fh, cw * ch);

                // POST=<jm post-deblock yuv> validates the deblocked frame;
                // otherwise diff the pre-deblock recon vs the predeblock dump.
                let (reference, stage) = if let Some(post) = std::env::var_os("POST") {
                    frame.deblock_intra(pps.chroma_qp_index_offset);
                    (std::fs::read(post)?, "post-deblock")
                } else {
                    (std::fs::read(&ref_path)?, "pre-deblock")
                };
                if std::env::var_os("DUMPY").is_some() {
                    std::fs::write("/home/rob/mvc-test/mine_y.bin", &frame.y)?;
                }

                let ok = |name: &str, got: &[u8], h: usize, w: usize, off: usize| -> bool {
                    for yy in 0..h {
                        for xx in 0..w {
                            let (g, exp) = (got[yy * w + xx], reference[off + yy * w + xx]);
                            if g != exp {
                                eprintln!("  ✗ {name} ({stage}): first mismatch ({xx},{yy}): libmvc={g} JM={exp}");
                                return false;
                            }
                        }
                    }
                    eprintln!("  ✓ {name} ({stage}): {h}×{w} match JM");
                    true
                };
                let oy = ok("Y", &frame.y, fh, fw, 0);
                let ou = ok("U", &frame.cb, ch, cw, ysz);
                let ov = ok("V", &frame.cr, ch, cw, ysz + csz);
                if oy && ou && ov {
                    eprintln!("✓ full intra-frame reconstruction MATCHES JM ({stage})");
                } else {
                    std::process::exit(1);
                }
                break;
            }
            _ => {}
        }
    }
    Ok(())
}
