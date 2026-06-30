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
use ripsaw::mvc::mb_header::{decode_cbp_ctx, decode_dquant_ctx, MbInfo};
use ripsaw::mvc::mb_inter::{decode_inter_mb_type, decode_mb_skip_flag, decode_mvd_component, InterContexts};
use ripsaw::mvc::mb_residual::{decode_mb_residual, CbfNeighbours, CbpBits, ResidualContexts};
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
    let mut decoded: Vec<(String, i64, Option<i64>)> = Vec::new();

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
                    decoded.push(("mb_skip_flag".into(), value1, None));
                    skip_grid.push(is_skip);
                    if !is_skip {
                        coded_at = Some(addr);
                        // First coded MB. Its neighbours are all skipped/absent
                        // (skip cbp = 0, transform = 0), so every neighbour-
                        // derived ctxIdxInc is 0.
                        let mb_type = decode_inter_mb_type(&mut e, &mut ctx);
                        decoded.push(("mb_type".into(), mb_type, None));
                        let mvd_pairs = match mb_type {
                            1 => 1,
                            2 | 3 => 2,
                            _ => 0,
                        };
                        for _ in 0..mvd_pairs {
                            let x = decode_mvd_component(&mut e, &mut ctx, 0, 0);
                            decoded.push(("mvd_l0".into(), x, None));
                            let y = decode_mvd_component(&mut e, &mut ctx, 1, 0);
                            decoded.push(("mvd_l0".into(), y, None));
                        }
                        // coded_block_pattern (neighbour cbp: up skip = 0, left absent).
                        let cbp = decode_cbp_ctx(&mut e, &mut ctx.cbp, Some(0), None);
                        decoded.push(("coded_block_pattern".into(), cbp, None));
                        let mut transform8x8 = false;
                        if cbp & 0x0f != 0 && pps.transform_8x8_mode_flag {
                            let t = e.decode_decision(&mut ctx.transform[0]);
                            transform8x8 = t == 1;
                            decoded.push(("transform_size_8x8_flag".into(), t as i64, None));
                        }
                        let mut last_dquant = 0;
                        if cbp != 0 {
                            let dq = decode_dquant_ctx(&mut e, &mut ctx.delta_qp, &mut last_dquant);
                            decoded.push(("mb_qp_delta".into(), dq as i64, None));
                        }
                        // Inter residual (P-model contexts). cbf neighbours:
                        // left absent, up = skipped MB (s_cbp = 0).
                        let info = MbInfo { i_nxn: false, transform8x8, c_ipred: 0, cbp: cbp as u8, i16_pred: 0 };
                        let mut rctx = ResidualContexts::new(slice_qp, true);
                        let mut rneigh = CbfNeighbours { cur: CbpBits::default(), left: None, up: Some(CbpBits::default()) };
                        decode_mb_residual(&mut e, &mut rctx, &info, &mut rneigh, slice_qp + last_dquant, pps.chroma_qp_index_offset, true, &mut decoded);
                        let eos = e.decode_terminate();
                        decoded.push(("end_of_slice_flag".into(), eos as i64, None));
                        break;
                    }
                    // P_Skip: no further MB syntax; just end_of_slice_flag.
                    let eos = e.decode_terminate();
                    decoded.push(("end_of_slice_flag".into(), eos as i64, None));
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
        if t.name != d.0 || t.value != d.1 || t.value2 != d.2 {
            eprintln!("✗ diverged at P-element {i}: JM ({:?}, {}, {:?}) vs libmvc {d:?}", t.name, t.value, t.value2);
            std::process::exit(1);
        }
    }
    eprintln!("✓ {} P-slice elements (skip-flag prefix) match JM trace exactly", decoded.len());
    Ok(())
}
