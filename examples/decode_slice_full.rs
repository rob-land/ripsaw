// Decode a full intra slice (all MB types — I_4x4 / I_8x8 / I_16x16, with
// chroma residual) macroblock by macroblock and diff every syntax element
// against the JM ldecod trace. Extends decode_slice's header-only loop with
// the residual orchestration (src/mvc/mb_residual.rs) and its coded_block_
// flag neighbour tracking.
//
//   cargo run --release --example decode_slice_full -- test.h264 trace_dec.txt

use ripsaw::mvc::annexb::NalSplitter;
use ripsaw::mvc::bitstream::BitReader;
use ripsaw::mvc::cabac::CabacEngine;
use ripsaw::mvc::mb_header::{decode_mb_header, MbHeaderContexts, MbInfo, Neighbors};
use ripsaw::mvc::mb_residual::{decode_mb_residual, CbfNeighbours, CbpBits, ResidualContexts};
use ripsaw::mvc::nal::parse_nal_unit_header;
use ripsaw::mvc::pps::{parse_pic_parameter_set, Pps};
use ripsaw::mvc::rbsp::extract_rbsp;
use ripsaw::mvc::slice_header::parse_slice_header;
use ripsaw::mvc::sps::{parse_seq_parameter_set_data, Sps};
use ripsaw::mvc::trace::{macroblock_elements, parse_trace};

type Elem = (String, i64, Option<i64>);

fn main() -> anyhow::Result<()> {
    let h264 = std::env::args().nth(1).ok_or_else(|| anyhow::anyhow!("usage: decode_slice_full <h264> <trace>"))?;
    let trace_path = std::env::args().nth(2).ok_or_else(|| anyhow::anyhow!("missing trace"))?;
    let data = std::fs::read(&h264)?;

    let mut sps: Option<Sps> = None;
    let mut pps: Option<Pps> = None;
    let mut decoded: Vec<Elem> = Vec::new();

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
                let width = sps.pic_width_in_mbs as usize;
                let mut r = BitReader::new(&rbsp);
                let sh = parse_slice_header(&mut r, true, hdr.nal_ref_idc, sps, pps)?;
                let slice_qp = 26 + pps.pic_init_qp_minus26 + sh.slice_qp_delta;
                let cabac_start = (r.position_bits() + 7) / 8;
                eprintln!("SliceQP {slice_qp}, width {width} MBs");

                let mut e = CabacEngine::new(&rbsp[cabac_start..]);
                let mut hctx = MbHeaderContexts::new(slice_qp);
                let mut rctx = ResidualContexts::new(slice_qp, false);
                let mut last_dquant = 0;
                let mut qp = slice_qp;

                let mut grid: Vec<MbInfo> = Vec::new();
                let mut cbp_grid: Vec<CbpBits> = Vec::new();
                let mut addr = 0usize;

                loop {
                    let mbx = addr % width;
                    let left_i = if mbx != 0 { grid.get(addr - 1).copied() } else { None };
                    let up_i = if addr >= width { grid.get(addr - width).copied() } else { None };
                    let mut header: Vec<(String, i64)> = Vec::new();
                    let info = decode_mb_header(&mut e, &mut hctx, &Neighbors { left: left_i, up: up_i }, &mut last_dquant, &mut header);
                    decoded.extend(header.iter().map(|(n, v)| (n.clone(), *v, None)));
                    if let Some((_, d)) = header.iter().find(|(n, _)| n == "mb_qp_delta") {
                        qp = (qp + *d as i32).rem_euclid(52);
                    }

                    let mut neigh = CbfNeighbours {
                        cur: CbpBits::default(),
                        left: if mbx != 0 { cbp_grid.get(addr - 1).copied() } else { None },
                        up: if addr >= width { cbp_grid.get(addr - width).copied() } else { None },
                    };
                    decode_mb_residual(&mut e, &mut rctx, &info, &mut neigh, qp, pps.chroma_qp_index_offset, false, &ripsaw::mvc::scaling::ScalingLists::flat(), &mut decoded);

                    let eos = e.decode_terminate();
                    grid.push(info);
                    cbp_grid.push(neigh.cur);
                    if eos == 1 {
                        // JM does not trace the slice-terminating end_of_slice.
                        break;
                    }
                    decoded.push(("end_of_slice_flag".into(), 0, None));
                    addr += 1;
                }
                eprintln!("decoded {} MBs, {} elements", grid.len(), decoded.len());
                break;
            }
            _ => {}
        }
    }

    let trace = parse_trace(&std::fs::read_to_string(&trace_path)?);
    let mb = macroblock_elements(&trace);
    for (i, d) in decoded.iter().enumerate() {
        let Some(t) = mb.get(i) else {
            eprintln!("✗ ran past trace at element {i}: decoded {d:?}");
            std::process::exit(1);
        };
        if t.name != d.0 || t.value != d.1 || t.value2 != d.2 {
            eprintln!("✗ diverged at element {i}: JM ({:?}, {}, {:?}) vs libmvc {d:?}", t.name, t.value, t.value2);
            std::process::exit(1);
        }
    }
    eprintln!("✓ {} elements match JM trace exactly", decoded.len());
    Ok(())
}
