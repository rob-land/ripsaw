// Decode the first macroblock's header from a real base-view IDR slice and
// compare it, element by element, against the JM ldecod trace
// (docs/libmvc-poc.md § Validation). The MB header precedes any residual
// data, so it can be validated without residual decoding — the first
// concrete checkpoint that the CABAC engine + context init + macroblock-
// header context derivation are correct on real data.
//
// Usage (after extract_mvcc_mkv + ldecod-trace produce s.h264 + trace_dec.txt):
//   cargo run --release --example decode_mb0 -- s.h264 trace_dec.txt

use ripsaw::mvc::annexb::NalSplitter;
use ripsaw::mvc::bitstream::BitReader;
use ripsaw::mvc::cabac::CabacEngine;
use ripsaw::mvc::mb_header::{decode_mb_header, MbHeaderContexts, Neighbors};
use ripsaw::mvc::nal::parse_nal_unit_header;
use ripsaw::mvc::pps::{parse_pic_parameter_set, Pps};
use ripsaw::mvc::rbsp::extract_rbsp;
use ripsaw::mvc::slice_header::parse_slice_header;
use ripsaw::mvc::sps::{parse_seq_parameter_set_data, Sps};
use ripsaw::mvc::trace::{first_divergence, macroblock_elements, parse_trace, Comparison};

fn main() -> anyhow::Result<()> {
    let h264 = std::env::args().nth(1).ok_or_else(|| anyhow::anyhow!("usage: decode_mb0 <s.h264> <trace_dec.txt>"))?;
    let trace_path = std::env::args().nth(2).ok_or_else(|| anyhow::anyhow!("missing trace_dec.txt"))?;
    let data = std::fs::read(&h264)?;

    let mut sps: Option<Sps> = None;
    let mut pps: Option<Pps> = None;
    let mut decoded: Vec<(String, i64)> = Vec::new();

    for nal in NalSplitter::new(&data) {
        if nal.is_empty() {
            continue;
        }
        let (h, consumed) = match parse_nal_unit_header(nal) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if h.mvc_extension.as_ref().map(|e| e.view_id != 0).unwrap_or(false) {
            continue;
        }
        let rbsp = extract_rbsp(&nal[consumed..]);
        match h.nal_unit_type {
            7 => sps = Some(parse_seq_parameter_set_data(&mut BitReader::new(&rbsp))?),
            8 => {
                let chroma = sps.as_ref().map(|s| s.chroma_format_idc).unwrap_or(1);
                pps = Some(parse_pic_parameter_set(&mut BitReader::new(&rbsp), chroma)?);
            }
            5 => {
                let (sps, pps) = (sps.as_ref().unwrap(), pps.as_ref().unwrap());
                let mut r = BitReader::new(&rbsp);
                let sh = parse_slice_header(&mut r, true, h.nal_ref_idc, sps, pps)?;
                let slice_qp = 26 + pps.pic_init_qp_minus26 + sh.slice_qp_delta;

                // CABAC slice data begins at the next byte boundary after
                // the header (cabac_alignment_one_bit).
                let cabac_start = (r.position_bits() + 7) / 8;
                eprintln!("SliceQP {slice_qp}, CABAC data from RBSP byte {cabac_start}");

                let mut e = CabacEngine::new(&rbsp[cabac_start..]);
                let mut ctx = MbHeaderContexts::new(slice_qp);
                let mut last_dquant = 0;

                // MB 0: top-left, no neighbours.
                let neigh = Neighbors::default();
                decode_mb_header(&mut e, &mut ctx, &neigh, &mut last_dquant, &mut decoded);
                // end_of_slice_flag follows the MB's data; for MB 0 it is 0.
                decoded.push(("end_of_slice_flag".into(), e.decode_terminate() as i64));
                break;
            }
            _ => {}
        }
    }

    eprintln!("\ndecoded MB 0 header elements:");
    for (n, v) in &decoded {
        eprintln!("  {n:<28} {v}");
    }

    // Compare against the trace's first macroblock.
    let trace = parse_trace(&std::fs::read_to_string(&trace_path)?);
    let mb = macroblock_elements(&trace);
    let reference: Vec<_> = mb.into_iter().take(decoded.len()).collect();

    eprintln!();
    match first_divergence(&reference, &decoded) {
        Comparison::Match { count } => {
            eprintln!("✓ MB 0 header MATCHES JM trace ({count} elements)");
        }
        Comparison::Diverged { position, reference, decoded } => {
            eprintln!(
                "✗ diverged at element {position}: JM = {:?} {}, libmvc = {:?} {}",
                reference.0, reference.1, decoded.0, decoded.1
            );
            std::process::exit(1);
        }
        Comparison::LengthMismatch { common, reference_len, decoded_len } => {
            eprintln!("✗ length mismatch: {common} common, JM {reference_len}, libmvc {decoded_len}");
            std::process::exit(1);
        }
    }
    Ok(())
}
