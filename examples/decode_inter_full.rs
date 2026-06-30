// End-to-end, JM-pixel-free decode of inter.h264 (IDR + P-slice), now driven
// entirely by the library: recon::decode_intra_frame + Frame::deblock_intra for
// the IDR, then recon_inter::decode_p_frame + deblock_inter for the P-slice
// using libmvc's OWN deblocked IDR as the MC reference. Both frames are diffed
// against JM ground truth:
//   - IDR     vs inter_post.yuv frame 0
//   - P-slice vs inter_predeblock.bin (pre-deblock) AND inter_post.yuv frame 1.
// No JM pixels enter the decode — only the final comparison. Also the
// regression guard for the recon_inter library refactor.
//
//   cargo run --release --example decode_inter_full -- inter.h264 inter_post.yuv inter_predeblock.bin

use ripsaw::mvc::annexb::NalSplitter;
use ripsaw::mvc::bitstream::BitReader;
use ripsaw::mvc::nal::parse_nal_unit_header;
use ripsaw::mvc::pps::{parse_pic_parameter_set, Pps};
use ripsaw::mvc::rbsp::extract_rbsp;
use ripsaw::mvc::recon::{decode_intra_frame, Frame};
use ripsaw::mvc::recon_inter::{deblock_inter, decode_p_frame};
use ripsaw::mvc::sps::{parse_seq_parameter_set_data, Sps};

fn cmp(label: &str, got: &[u8], jm: &[u8], off: usize) -> bool {
    for (i, &g) in got.iter().enumerate() {
        if g != jm[off + i] {
            eprintln!("✗ {label} mismatch at byte {i}: {g} vs JM {}", jm[off + i]);
            return false;
        }
    }
    true
}

fn main() -> anyhow::Result<()> {
    let h264 = std::env::args().nth(1).unwrap();
    let jm_yuv = std::env::args().nth(2).unwrap();
    let jm_p = std::env::args().nth(3).unwrap();
    let data = std::fs::read(&h264)?;

    let mut sps: Option<Sps> = None;
    let mut pps: Option<Pps> = None;
    let mut reference: Option<Frame> = None;

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
                frame.deblock_intra(pps.chroma_qp_index_offset);
                let jm = std::fs::read(&jm_yuv)?;
                let (ysz, csz) = (frame.fw * frame.fh, frame.cw * frame.ch);
                let ok = cmp("IDR Y", &frame.y, &jm, 0) && cmp("IDR U", &frame.cb, &jm, ysz) && cmp("IDR V", &frame.cr, &jm, ysz + csz);
                if ok {
                    eprintln!("✓ IDR decoded by libmvc matches JM frame 0 ({}×{})", frame.fw, frame.fh);
                } else {
                    std::process::exit(1);
                }
                reference = Some(frame);
            }
            1 => {
                let (sps, pps) = (sps.as_ref().unwrap(), pps.as_ref().unwrap());
                let reff = reference.as_ref().expect("P-slice before IDR");
                let (mut pframe, mf) = decode_p_frame(&rbsp, hdr.nal_ref_idc, sps, pps, reff)?;
                let (ysz, csz) = (pframe.fw * pframe.fh, pframe.cw * pframe.ch);

                let pre = std::fs::read(&jm_p)?;
                let pre_ok = cmp("P pre Y", &pframe.y, &pre, 0) && cmp("P pre U", &pframe.cb, &pre, ysz) && cmp("P pre V", &pframe.cr, &pre, ysz + csz);
                if pre_ok {
                    eprintln!("✓ P-slice pre-deblock (ref = libmvc's own IDR) matches JM ({}×{})", pframe.fw, pframe.fh);
                } else {
                    std::process::exit(1);
                }

                deblock_inter(&mut pframe, &mf, pps.chroma_qp_index_offset);
                let jm = std::fs::read(&jm_yuv)?;
                let foff = ysz + 2 * csz; // frame 1
                let post_ok = cmp("P post Y", &pframe.y, &jm, foff) && cmp("P post U", &pframe.cb, &jm, foff + ysz) && cmp("P post V", &pframe.cr, &jm, foff + ysz + csz);
                if post_ok {
                    eprintln!("✓ P-slice post-deblock matches JM final P-frame");
                    eprintln!("✓ inter.h264 decoded END-TO-END by libmvc — both frames bit-exact incl. inter deblock, zero JM pixels");
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
