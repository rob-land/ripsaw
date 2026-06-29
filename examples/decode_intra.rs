// libmvc PoC validation harness (docs/libmvc-poc.md § Validation).
//
// Drives the front-end parser chain over a real base-view IDR frame and
// locates the CABAC slice data the macroblock decoder will consume, and
// produces the ffmpeg reference frame to diff against. The macroblock
// decode itself is the remaining integration work; this harness is the
// scaffold it plugs into.
//
// Usage:
//   cargo run --release --example extract_mvcc_mkv -- clip.mkv s.h264
//   cargo run --release --example decode_intra -- s.h264 clip.mkv
//
// Arg 1: Annex B elementary stream (from extract_mvcc_mkv).
// Arg 2: the source MKV (for the ffmpeg reference; optional).

use std::path::PathBuf;
use std::process::Command;

use ripsaw::mvc::annexb::NalSplitter;
use ripsaw::mvc::bitstream::BitReader;
use ripsaw::mvc::nal::parse_nal_unit_header;
use ripsaw::mvc::pps::{parse_pic_parameter_set, Pps};
use ripsaw::mvc::rbsp::extract_rbsp;
use ripsaw::mvc::slice_header::parse_slice_header;
use ripsaw::mvc::sps::{parse_seq_parameter_set_data, Sps};

const NAL_SPS: u8 = 7;
const NAL_PPS: u8 = 8;
const NAL_IDR: u8 = 5;

fn main() -> anyhow::Result<()> {
    let h264 = std::env::args().nth(1).ok_or_else(|| anyhow::anyhow!("usage: decode_intra <s.h264> [clip.mkv]"))?;
    let mkv = std::env::args().nth(2).map(PathBuf::from);
    let data = std::fs::read(&h264)?;
    eprintln!("read {h264} ({} bytes)", data.len());

    let mut sps: Option<Sps> = None;
    let mut pps: Option<Pps> = None;

    // Walk NAL units: pick up the base-layer SPS/PPS, then decode the
    // header of the first base-view IDR slice (NAL type 5).
    for nal in NalSplitter::new(&data) {
        if nal.is_empty() {
            continue;
        }
        let (h, consumed) = match parse_nal_unit_header(nal) {
            Ok(v) => v,
            Err(_) => continue,
        };
        // MVC dependent-view NALs carry the extension; skip anything not
        // base-layer for this base-frame harness.
        if h.mvc_extension.as_ref().map(|e| e.view_id != 0).unwrap_or(false) {
            continue;
        }
        let rbsp = extract_rbsp(&nal[consumed..]);
        match h.nal_unit_type {
            NAL_SPS => {
                let mut r = BitReader::new(&rbsp);
                let s = parse_seq_parameter_set_data(&mut r)?;
                eprintln!(
                    "SPS: profile {} level {} {}x{} chroma_idc {} POC type {}",
                    s.profile_idc, s.level_idc, s.width, s.height, s.chroma_format_idc,
                    s.pic_order_cnt_type
                );
                sps = Some(s);
            }
            NAL_PPS => {
                let chroma = sps.as_ref().map(|s| s.chroma_format_idc).unwrap_or(1);
                let mut r = BitReader::new(&rbsp);
                let p = parse_pic_parameter_set(&mut r, chroma)?;
                eprintln!(
                    "PPS: cabac={} transform_8x8={} deblock_ctl={} num_ref_l0={}",
                    p.entropy_coding_mode_flag, p.transform_8x8_mode_flag,
                    p.deblocking_filter_control_present_flag,
                    p.num_ref_idx_l0_default_active_minus1 + 1
                );
                pps = Some(p);
            }
            NAL_IDR => {
                let (Some(sps), Some(pps)) = (sps.as_ref(), pps.as_ref()) else {
                    anyhow::bail!("IDR slice before SPS/PPS");
                };
                let mut r = BitReader::new(&rbsp);
                let sh = parse_slice_header(&mut r, true, h.nal_ref_idc, sps, pps)?;
                eprintln!(
                    "\nfirst base IDR slice:\n  type={:?} first_mb={} qp={} (slice_qp_delta={})\n  \
                     pps cabac={} -> CABAC slice data starts at bit {} of the RBSP",
                    sh.slice_kind,
                    sh.first_mb_in_slice,
                    26 + pps.pic_init_qp_minus26 + sh.slice_qp_delta,
                    sh.slice_qp_delta,
                    pps.entropy_coding_mode_flag,
                    r.position_bits(),
                );
                let mbs = sps.pic_width_in_mbs * sps.pic_height_in_map_units;
                eprintln!(
                    "  frame is {} macroblocks ({}x{} MB grid), coded {}x{}",
                    mbs, sps.pic_width_in_mbs, sps.pic_height_in_map_units,
                    sps.pic_width_in_mbs * 16, sps.pic_height_in_map_units * 16
                );
                eprintln!(
                    "\n[harness ready] front-end parsed the base IDR; the macroblock\n  \
                     decoder consumes the CABAC slice data from here. Reference:"
                );
                break;
            }
            _ => {}
        }
    }

    // ffmpeg reference frame, if the MKV was provided.
    if let Some(mkv) = mkv {
        let out = PathBuf::from(&h264).with_extension("ref.yuv");
        let status = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
            .arg(&mkv)
            .args(["-map", "0:v:0", "-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "yuv420p"])
            .arg(&out)
            .status()?;
        if status.success() {
            let sz = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
            eprintln!("  ffmpeg reference frame -> {} ({} bytes)", out.display(), sz);
        } else {
            eprintln!("  ffmpeg reference decode failed");
        }
    }
    Ok(())
}
