// Decode an entire base-view I-slice macroblock by macroblock — header +
// (8×8) luma residual, with full neighbour tracking — and diff *every*
// syntax element against the JM ldecod trace (docs/libmvc-poc.md §
// Validation). Where MB 0 alone proved one coefficient, this walks the
// whole slice: 1320 MBs of header context derivation (mb_type / transform /
// intra-pred / chroma-pred / CBP / dquant neighbour contexts) plus the
// run-adaptive residual decode wherever a block is coded, all kept in lock-
// step with the CABAC engine across the slice.
//
// 8×8 luma blocks carry no coded_block_flag (inferred from the CBP), so the
// luma residual needs no neighbour-cbf machinery. The loop stops cleanly at
// the first construct not yet built — chroma residual, an I_4x4 or I_16x16
// macroblock with coefficients — and reports how far it validated, which is
// exactly the next piece to implement.
//
//   cargo run --release --example decode_slice -- base1.h264 trace_dec.txt

use ripsaw::mvc::annexb::NalSplitter;
use ripsaw::mvc::bitstream::BitReader;
use ripsaw::mvc::cabac::CabacEngine;
use ripsaw::mvc::mb_header::{decode_mb_header, MbHeaderContexts, MbInfo, Neighbors};
use ripsaw::mvc::nal::parse_nal_unit_header;
use ripsaw::mvc::pps::{parse_pic_parameter_set, Pps};
use ripsaw::mvc::rbsp::extract_rbsp;
use ripsaw::mvc::residual::decode_residual_block;
use ripsaw::mvc::residual_ctx::ResidualCat;
use ripsaw::mvc::slice_header::parse_slice_header;
use ripsaw::mvc::sps::{parse_seq_parameter_set_data, Sps};
use ripsaw::mvc::trace::{macroblock_elements, parse_trace};

/// A decoded element with the run for residual lines, so it can be diffed
/// against the trace including the run value.
type Elem = (String, i64, Option<i64>);

fn level_run_pairs(coeffs: &[i32]) -> Vec<(i64, i64)> {
    let mut pairs = Vec::new();
    let mut run = 0i64;
    for &c in coeffs {
        if c == 0 {
            run += 1;
        } else {
            pairs.push((c as i64, run));
            run = 0;
        }
    }
    pairs.push((0, 0));
    pairs
}

/// Decode one coded 8×8 luma block and append its (level, run) trace
/// elements: the first call is labelled "Luma8x8 DC sng", the rest
/// "Luma8x8 sng" (matching JM read_comp_cabac.c).
fn decode_luma8x8(e: &mut CabacEngine, slice_qp: i32, out: &mut Vec<Elem>) {
    let cat = ResidualCat::Luma8x8;
    let d = cat.desc();
    let mut ctx = cat.coeff_contexts(slice_qp, false, 0);
    let coeffs = decode_residual_block(e, &mut ctx, d.max_num_coeff, d.pos2ctx_map, d.pos2ctx_last, d.gt1_cap);
    for (idx, (lvl, run)) in level_run_pairs(&coeffs).into_iter().enumerate() {
        let name = if idx == 0 { "Luma8x8 DC sng" } else { "Luma8x8 sng" };
        out.push((name.into(), lvl, Some(run)));
    }
}

fn main() -> anyhow::Result<()> {
    let h264 = std::env::args().nth(1).ok_or_else(|| anyhow::anyhow!("usage: decode_slice <base1.h264> <trace_dec.txt>"))?;
    let trace_path = std::env::args().nth(2).ok_or_else(|| anyhow::anyhow!("missing trace_dec.txt"))?;
    let data = std::fs::read(&h264)?;

    let mut sps: Option<Sps> = None;
    let mut pps: Option<Pps> = None;
    let mut decoded: Vec<Elem> = Vec::new();
    let mut stop_reason = String::from("end of slice");

    for nal in NalSplitter::new(&data) {
        if nal.is_empty() {
            continue;
        }
        let (hdr, consumed) = match parse_nal_unit_header(nal) {
            Ok(v) => v,
            Err(_) => continue,
        };
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
                eprintln!("SliceQP {slice_qp}, width {width} MBs, CABAC from RBSP byte {cabac_start}");

                if std::env::var("DBG").is_ok() {
                    let lo = 24.min(rbsp.len());
                    let hi = 44.min(rbsp.len());
                    eprintln!("RBSP len {}, bytes[{lo}..{hi}]: {:02x?}", rbsp.len(), &rbsp[lo..hi]);
                }
                let mut e = CabacEngine::new(&rbsp[cabac_start..]);
                let mut ctx = MbHeaderContexts::new(slice_qp);
                let mut last_dquant = 0;

                // MbInfo grid for neighbour lookup, indexed by MB address.
                let mut grid: Vec<MbInfo> = Vec::new();
                let mut addr = 0usize; // first_mb_in_slice = 0 for this slice

                'slice: loop {
                    let left = if addr % width != 0 { grid.get(addr - 1).copied() } else { None };
                    let up = if addr >= width { grid.get(addr - width).copied() } else { None };
                    let neigh = Neighbors { left, up };

                    let mut header: Vec<(String, i64)> = Vec::new();
                    let info = decode_mb_header(&mut e, &mut ctx, &neigh, &mut last_dquant, pps.transform_8x8_mode_flag, &mut header);
                    decoded.extend(header.into_iter().map(|(n, v)| (n, v, None)));

                    // Residual. Only the I_8x8 luma path is built; bail out
                    // (cleanly) on anything else that carries coefficients.
                    if info.cbp != 0 {
                        if !info.i_nxn {
                            stop_reason = format!("MB {addr}: I_16x16 residual not yet implemented (cbp={})", info.cbp);
                            break 'slice;
                        }
                        if !info.transform8x8 {
                            stop_reason = format!("MB {addr}: I_4x4 residual not yet implemented (cbp={})", info.cbp);
                            break 'slice;
                        }
                        // Luma: 4 8×8 blocks, low nibble of cbp.
                        for b8 in 0..4 {
                            if info.cbp & (1 << b8) != 0 {
                                decode_luma8x8(&mut e, slice_qp, &mut decoded);
                            }
                        }
                        // Chroma: high nibble. Needs the cbf neighbour path.
                        if info.cbp & 0x30 != 0 {
                            stop_reason = format!("MB {addr}: chroma residual not yet implemented (cbp={})", info.cbp);
                            break 'slice;
                        }
                    }

                    let eos = e.decode_terminate();
                    decoded.push(("end_of_slice_flag".into(), eos as i64, None));
                    grid.push(info);

                    if eos == 1 {
                        break 'slice;
                    }
                    addr += 1;
                }

                eprintln!("decoded {} MBs, {} elements; stop: {stop_reason}", grid.len(), decoded.len());
                break;
            }
            _ => {}
        }
    }

    // Diff against the trace's macroblock elements (name + value + run).
    let trace = parse_trace(&std::fs::read_to_string(&trace_path)?);
    let mb = macroblock_elements(&trace);
    let mut matched = 0usize;
    for (i, d) in decoded.iter().enumerate() {
        let Some(t) = mb.get(i) else {
            eprintln!("✗ ran past trace at element {i}: decoded {d:?}");
            std::process::exit(1);
        };
        if t.name != d.0 || t.value != d.1 || t.value2 != d.2 {
            eprintln!(
                "✗ diverged at element {i} (MB-relative): JM ({:?}, {}, {:?}) vs libmvc {d:?}",
                t.name, t.value, t.value2
            );
            std::process::exit(1);
        }
        matched += 1;
    }
    eprintln!("✓ {matched} elements match JM trace exactly");
    Ok(())
}
