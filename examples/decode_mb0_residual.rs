// Decode the first macroblock's *residual* (on top of decode_mb0's header)
// and validate the coefficient (level, run) stream against the JM trace
// (docs/libmvc-poc.md § Validation). MB 0 of this base-view IDR is an I_8x8
// macroblock with coded_block_pattern = 1, so exactly one 8×8 luma block is
// coded and no chroma; its significance map yields a single coefficient, so
// this is the minimal end-to-end residual path — significance map + last
// flag + the run-adaptive level decode (here the saturated TU prefix + EG0
// bypass suffix, since the QP-0 level is large) — exercised on real CABAC
// data and diffed coefficient-for-coefficient against ldecod.
//
//   cargo run --release --example decode_mb0_residual -- base1.h264 trace_dec.txt

use ripsaw::mvc::annexb::NalSplitter;
use ripsaw::mvc::bitstream::BitReader;
use ripsaw::mvc::cabac::CabacEngine;
use ripsaw::mvc::mb_header::{decode_mb_header, MbHeaderContexts, Neighbors};
use ripsaw::mvc::nal::parse_nal_unit_header;
use ripsaw::mvc::pps::{parse_pic_parameter_set, Pps};
use ripsaw::mvc::rbsp::extract_rbsp;
use ripsaw::mvc::residual::decode_residual_block;
use ripsaw::mvc::residual_ctx::ResidualCat;
use ripsaw::mvc::slice_header::parse_slice_header;
use ripsaw::mvc::sps::{parse_seq_parameter_set_data, Sps};
use ripsaw::mvc::trace::{macroblock_elements, parse_trace};

/// Convert scan-order coefficients to JM's (level, run) trace pairs: each
/// nonzero emits (level, #preceding zeros) and a final (0, 0) EOB marks the
/// end of the block.
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
    pairs.push((0, 0)); // EOB
    pairs
}

fn main() -> anyhow::Result<()> {
    let h264 = std::env::args().nth(1).ok_or_else(|| anyhow::anyhow!("usage: decode_mb0_residual <base1.h264> <trace_dec.txt>"))?;
    let trace_path = std::env::args().nth(2).ok_or_else(|| anyhow::anyhow!("missing trace_dec.txt"))?;
    let data = std::fs::read(&h264)?;

    let mut sps: Option<Sps> = None;
    let mut pps: Option<Pps> = None;

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
                let mut r = BitReader::new(&rbsp);
                let sh = parse_slice_header(&mut r, true, hdr.nal_ref_idc, sps, pps)?;
                let slice_qp = 26 + pps.pic_init_qp_minus26 + sh.slice_qp_delta;
                let cabac_start = (r.position_bits() + 7) / 8;
                eprintln!("SliceQP {slice_qp}, CABAC data from RBSP byte {cabac_start}");

                let mut e = CabacEngine::new(&rbsp[cabac_start..]);
                let mut ctx = MbHeaderContexts::new(slice_qp);
                let mut last_dquant = 0;
                let mut header: Vec<(String, i64)> = Vec::new();

                let neigh = Neighbors::default();
                let info = decode_mb_header(&mut e, &mut ctx, &neigh, &mut last_dquant, &mut header);
                eprintln!("MB 0: i_nxn={}, transform8x8={}, cbp={}", info.i_nxn, info.transform8x8, info.cbp);

                // I_8x8 with cbp bit 0 set -> decode the one coded 8×8 luma
                // block. No coded_block_flag in 8×8 mode (inferred from CBP).
                assert!(info.transform8x8, "expected 8×8 transform MB");
                assert_eq!(info.cbp & 1, 1, "expected luma block 0 coded");

                let cat = ResidualCat::Luma8x8;
                let d = cat.desc();
                let mut coeff_ctx = cat.coeff_contexts(slice_qp);
                let coeffs = decode_residual_block(
                    &mut e,
                    &mut coeff_ctx,
                    d.max_num_coeff,
                    d.pos2ctx_map,
                    d.pos2ctx_last,
                    d.gt1_cap,
                );
                let pairs = level_run_pairs(&coeffs);

                eprintln!("\ndecoded luma 8×8 block 0 (level, run) pairs:");
                for (lvl, run) in &pairs {
                    eprintln!("  level {lvl:>6}  run {run}");
                }

                // The trace's residual elements for MB 0: every non-header
                // element after mb_qp_delta up to end_of_slice_flag.
                let trace = parse_trace(&std::fs::read_to_string(&trace_path)?);
                let mb = macroblock_elements(&trace);
                let qp_idx = mb.iter().position(|e| e.name == "mb_qp_delta").unwrap();
                let eos_idx = mb.iter().position(|e| e.name == "end_of_slice_flag").unwrap();
                let ref_pairs: Vec<(i64, i64)> = mb[qp_idx + 1..eos_idx]
                    .iter()
                    .map(|e| (e.value, e.value2.expect("residual line has run")))
                    .collect();

                eprintln!("\nJM trace residual (level, run) pairs:");
                for (lvl, run) in &ref_pairs {
                    eprintln!("  level {lvl:>6}  run {run}");
                }

                eprintln!();
                if pairs == ref_pairs {
                    eprintln!("✓ MB 0 luma 8×8 block 0 residual MATCHES JM trace ({} coeffs)", pairs.len());
                } else {
                    eprintln!("✗ residual mismatch: libmvc {pairs:?} vs JM {ref_pairs:?}");
                    std::process::exit(1);
                }
                break;
            }
            _ => {}
        }
    }
    Ok(())
}
