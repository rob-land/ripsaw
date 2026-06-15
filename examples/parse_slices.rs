// Front-end driver: parse every SPS / Subset SPS / PPS / slice header in
// an H.264 (or MVC) Annex B elementary stream and print a summary. This
// exercises the whole libmvc parsing chain — nal -> rbsp -> sps/pps ->
// slice_header — against a real bitstream.
//
// Usage:
//   cargo run --release --example extract_mvcc_mkv -- in.mkv /tmp/s.h264
//   cargo run --release --example parse_slices -- /tmp/s.h264

use std::collections::HashMap;
use std::fs;

use ripsaw::mvc::annexb::NalSplitter;
use ripsaw::mvc::nal::{
    parse_nal_unit_header, NAL_SPS, NAL_SUBSET_SPS,
};
use ripsaw::mvc::pps::{parse_pps_rbsp, Pps};
use ripsaw::mvc::rbsp::extract_rbsp;
use ripsaw::mvc::slice_header::parse_slice_header;
use ripsaw::mvc::sps::{parse_sps_rbsp, parse_subset_sps_rbsp, Sps};

const NAL_SLICE_NON_IDR: u8 = 1;
const NAL_SLICE_IDR: u8 = 5;
const NAL_SLICE_EXT: u8 = 20; // MVC dependent-view coded slice extension
const NAL_PPS: u8 = 8;

fn main() -> anyhow::Result<()> {
    let path = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("usage: parse_slices <input.h264>"))?;
    let data = fs::read(&path)?;
    eprintln!("read {path} ({} bytes)", data.len());

    let mut sps_by_id: HashMap<u32, Sps> = HashMap::new();
    let mut pps_by_id: HashMap<u32, Pps> = HashMap::new();
    let (mut n_slices, mut n_mvc_slices, mut printed) = (0u64, 0u64, 0u64);

    for nal in NalSplitter::new(&data) {
        if nal.is_empty() {
            continue;
        }
        let (header, consumed) = match parse_nal_unit_header(nal) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let rbsp = extract_rbsp(&nal[consumed..]);

        match header.nal_unit_type {
            NAL_SPS => {
                if let Ok(sps) = parse_sps_rbsp(&rbsp) {
                    eprintln!(
                        "SPS  id={} profile={} level={} {}x{}",
                        sps.seq_parameter_set_id, sps.profile_idc, sps.level_idc,
                        sps.width, sps.height
                    );
                    sps_by_id.insert(sps.seq_parameter_set_id, sps);
                }
            }
            NAL_SUBSET_SPS => {
                if let Ok(subset) = parse_subset_sps_rbsp(&rbsp) {
                    eprintln!(
                        "SubSPS id={} profile={} {}x{} views={}",
                        subset.sps.seq_parameter_set_id, subset.sps.profile_idc,
                        subset.sps.width, subset.sps.height, subset.mvc.num_views_minus1 + 1
                    );
                    sps_by_id.insert(subset.sps.seq_parameter_set_id, subset.sps);
                }
            }
            NAL_PPS => {
                // Peek the PPS's referenced SPS for chroma_format_idc; a
                // standalone PPS parse only needs it for scaling matrices.
                let chroma = sps_by_id.values().next().map(|s| s.chroma_format_idc).unwrap_or(1);
                if let Ok(pps) = parse_pps_rbsp(&rbsp, chroma) {
                    eprintln!(
                        "PPS  id={} sps={} cabac={} deblock_ctl={}",
                        pps.pic_parameter_set_id, pps.seq_parameter_set_id,
                        pps.entropy_coding_mode_flag, pps.deblocking_filter_control_present_flag
                    );
                    pps_by_id.insert(pps.pic_parameter_set_id, pps);
                }
            }
            NAL_SLICE_NON_IDR | NAL_SLICE_IDR | NAL_SLICE_EXT => {
                let is_mvc = header.nal_unit_type == NAL_SLICE_EXT;
                if is_mvc {
                    n_mvc_slices += 1;
                } else {
                    n_slices += 1;
                }
                // Resolve SPS/PPS. We need the pps_id, which is the 3rd
                // ue field — easiest is to let parse_slice_header read it,
                // but it needs the SPS up front. Real streams here use a
                // single SPS+PPS, so use the first of each.
                let (Some(sps), Some(pps)) =
                    (sps_by_id.values().next(), pps_by_id.values().next())
                else {
                    continue;
                };
                let idr_pic_flag = if is_mvc {
                    header.mvc_extension.as_ref().map(|e| !e.non_idr_flag).unwrap_or(false)
                } else {
                    header.nal_unit_type == NAL_SLICE_IDR
                };
                let mut r = ripsaw::mvc::bitstream::BitReader::new(&rbsp);
                match parse_slice_header(&mut r, idr_pic_flag, header.nal_ref_idc, sps, pps) {
                    Ok(sh) if printed < 8 => {
                        printed += 1;
                        eprintln!(
                            "{} slice: type={:?} first_mb={} frame_num={} poc_lsb={:?} qp_delta={} view_id={:?}",
                            if is_mvc { "MVC" } else { "base" },
                            sh.slice_kind, sh.first_mb_in_slice, sh.frame_num,
                            sh.pic_order_cnt_lsb, sh.slice_qp_delta,
                            header.mvc_extension.as_ref().map(|e| e.view_id),
                        );
                    }
                    Ok(_) => {}
                    Err(e) => eprintln!("slice header parse error: {e:?}"),
                }
            }
            _ => {}
        }
    }

    eprintln!(
        "done: {n_slices} base slices, {n_mvc_slices} MVC slices, \
         {} SPS, {} PPS",
        sps_by_id.len(), pps_by_id.len()
    );
    Ok(())
}
