// First step of the inter arc: decode a P-slice's mb_skip_flag stream and
// diff it against the JM trace. Validates the inter CABAC context init
// (cabac_init_idc model) + the skip-flag neighbour context. Stops at the
// first coded (non-skip) MB — full inter-MB decode (mb_type, mvd, MC) comes
// next.
//
//   cargo run --release --example decode_pslice -- inter.h264 trace_dec.txt

use ripsaw::mvc::annexb::NalSplitter;
use ripsaw::mvc::bitstream::BitReader;
use ripsaw::mvc::cabac::CabacEngine;
use ripsaw::mvc::mb_inter::{decode_mb_skip_flag, InterContexts};
use ripsaw::mvc::nal::parse_nal_unit_header;
use ripsaw::mvc::pps::{parse_pic_parameter_set, Pps};
use ripsaw::mvc::rbsp::extract_rbsp;
use ripsaw::mvc::slice_header::parse_slice_header;
use ripsaw::mvc::sps::{parse_seq_parameter_set_data, Sps};
use ripsaw::mvc::trace::{macroblock_elements, parse_trace};

fn main() -> anyhow::Result<()> {
    let h264 = std::env::args().nth(1).unwrap();
    let trace_path = std::env::args().nth(2).unwrap();
    let data = std::fs::read(&h264)?;

    let mut sps: Option<Sps> = None;
    let mut pps: Option<Pps> = None;
    let mut idr_done = false;
    let mut decoded: Vec<(String, i64)> = Vec::new();

    for nal in NalSplitter::new(&data) {
        if nal.is_empty() {
            continue;
        }
        let Ok((hdr, consumed)) = parse_nal_unit_header(nal) else { continue };
        let rbsp = extract_rbsp(&nal[consumed..]);
        match hdr.nal_unit_type {
            7 => sps = Some(parse_seq_parameter_set_data(&mut BitReader::new(&rbsp))?),
            8 => {
                let chroma = sps.as_ref().map(|s| s.chroma_format_idc).unwrap_or(1);
                pps = Some(parse_pic_parameter_set(&mut BitReader::new(&rbsp), chroma)?);
            }
            5 => idr_done = true, // skip the IDR; we validate the P-slice
            1 if idr_done => {
                let (sps, pps) = (sps.as_ref().unwrap(), pps.as_ref().unwrap());
                let width = sps.pic_width_in_mbs as usize;
                let mut r = BitReader::new(&rbsp);
                let sh = parse_slice_header(&mut r, false, hdr.nal_ref_idc, sps, pps)?;
                let slice_qp = 26 + pps.pic_init_qp_minus26 + sh.slice_qp_delta;
                let idc = sh.cabac_init_idc.unwrap_or(0);
                eprintln!("P-slice: slice_type {}, qp {slice_qp}, cabac_init_idc {idc}", sh.slice_type);

                let cabac_start = (r.position_bits() + 7) / 8;
                let mut e = CabacEngine::new(&rbsp[cabac_start..]);
                let mut ctx = InterContexts::new(idc, slice_qp);

                // skip_flag per MB (for the neighbour context).
                let mut skip_grid: Vec<bool> = Vec::new();
                let mut addr = 0usize;
                let mut coded_at = None;
                loop {
                    let mbx = addr % width;
                    let left_ns = if mbx != 0 { skip_grid.get(addr - 1).map(|&s| (!s) as u32).unwrap_or(0) } else { 0 };
                    let up_ns = if addr >= width { skip_grid.get(addr - width).map(|&s| (!s) as u32).unwrap_or(0) } else { 0 };
                    let (is_skip, value1) = decode_mb_skip_flag(&mut e, &mut ctx, left_ns, up_ns);
                    decoded.push(("mb_skip_flag".into(), value1));
                    skip_grid.push(is_skip);
                    if !is_skip {
                        coded_at = Some(addr);
                        break;
                    }
                    // P_Skip: no further MB syntax; just end_of_slice_flag.
                    let eos = e.decode_terminate();
                    decoded.push(("end_of_slice_flag".into(), eos as i64));
                    if eos == 1 {
                        break;
                    }
                    addr += 1;
                }
                eprintln!("decoded {} skipped MBs; first coded MB at {:?}", skip_grid.iter().filter(|&&s| s).count(), coded_at);
                break;
            }
            _ => {}
        }
    }

    // The P-slice's macroblock elements follow the IDR's in the trace.
    let trace = parse_trace(&std::fs::read_to_string(&trace_path)?);
    let mb = macroblock_elements(&trace);
    let first_skip = mb.iter().position(|e| e.name == "mb_skip_flag").expect("a P-slice in the trace");
    let reference = &mb[first_skip..];

    for (i, d) in decoded.iter().enumerate() {
        let t = reference[i];
        if t.name != d.0 || t.value != d.1 {
            eprintln!("✗ diverged at P-element {i}: JM ({:?}, {}) vs libmvc {d:?}", t.name, t.value);
            std::process::exit(1);
        }
    }
    eprintln!("✓ {} P-slice elements (skip-flag prefix) match JM trace exactly", decoded.len());
    Ok(())
}
