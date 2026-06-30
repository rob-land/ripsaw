// B-slice syntax decode, diffed element-by-element against the JM trace — the
// first step of the B arc (mirrors decode_pslice for P). Validates the B
// CABAC contexts (mb_type_contexts[2], b8_type_contexts[1]) and the B MB parse:
// mb_skip_flag (B), mb_type (B tree → interpret_b_mb_type), sub_mb_type (B),
// the list-major mvd read order (all L0 partitions, then all L1), cbp /
// transform_size_8x8_flag / mb_qp_delta, and the inter residual. Spatial
// direct / bi-pred reconstruction (pixels) comes next; this is syntax only.
//
//   cargo run --release --example decode_bslice -- bframe.h264 trace_dec.txt

use ripsaw::mvc::annexb::NalSplitter;
use ripsaw::mvc::bitstream::BitReader;
use ripsaw::mvc::cabac::CabacEngine;
use ripsaw::mvc::mb_header::{decode_cbp_ctx, decode_dquant_ctx, MbInfo};
use ripsaw::mvc::mb_inter::{
    decode_b_mb_type, decode_b_sub_mb_type, decode_mb_skip_flag_b, decode_mvd_component, interpret_b_mb_type, mvd_ctx_inc,
    InterContexts,
};
use ripsaw::mvc::mb_residual::{decode_mb_residual, CbfNeighbours, CbpBits, ResidualContexts};
use ripsaw::mvc::nal::parse_nal_unit_header;
use ripsaw::mvc::pps::{parse_pic_parameter_set, Pps};
use ripsaw::mvc::rbsp::extract_rbsp;
use ripsaw::mvc::slice_header::parse_slice_header;
use ripsaw::mvc::sps::{parse_seq_parameter_set_data, Sps};
use ripsaw::mvc::trace::{macroblock_elements, parse_trace};

/// sub_mb_type → (pdir, sub-partitions as (dx4,dy4,w4,h4) within the b8's 2×2).
/// pdir: 0=L0, 1=L1, 2=Bi, 3=Direct (no mvd).
fn sub_geom(s: i64) -> (u8, Vec<(usize, usize, usize, usize)>) {
    let pdir = match s {
        0 => 3,
        1 | 4 | 5 | 10 => 0,
        2 | 6 | 7 | 11 => 1,
        _ => 2,
    };
    let parts = match s {
        0 | 1 | 2 | 3 => vec![(0, 0, 2, 2)],                       // 8×8
        4 | 6 | 8 => vec![(0, 0, 2, 1), (0, 1, 2, 1)],             // 8×4
        5 | 7 | 9 => vec![(0, 0, 1, 2), (1, 0, 1, 2)],             // 4×8
        _ => vec![(0, 0, 1, 1), (1, 0, 1, 1), (0, 1, 1, 1), (1, 1, 1, 1)], // 4×4
    };
    (pdir, parts)
}

fn main() -> anyhow::Result<()> {
    let h264 = std::env::args().nth(1).unwrap();
    let trace_path = std::env::args().nth(2).unwrap();
    let data = std::fs::read(&h264)?;

    let mut sps: Option<Sps> = None;
    let mut pps: Option<Pps> = None;
    let mut decoded: Vec<(String, i64, Option<i64>)> = Vec::new();
    let mut slice_idx = 0; // 0=IDR,1=P,2=B(first)

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
            5 => slice_idx = 1,
            1 => {
                if slice_idx < 2 {
                    slice_idx += 1; // skip the P-slice; first B is slice_idx 2
                    continue;
                }
                let (sps, pps) = (sps.as_ref().unwrap(), pps.as_ref().unwrap());
                let width = sps.pic_width_in_mbs as usize;
                let (bw4, bh4) = (width * 4, sps.pic_height_in_map_units as usize * 4);
                let mut r = BitReader::new(&rbsp);
                let sh = parse_slice_header(&mut r, false, hdr.nal_ref_idc, sps, pps)?;
                let slice_qp = 26 + pps.pic_init_qp_minus26 + sh.slice_qp_delta;
                let idc = sh.cabac_init_idc.unwrap_or(0);
                eprintln!("B-slice: slice_type {}, qp {slice_qp}, idc {idc}, direct_spatial {}", sh.slice_type, sh.direct_spatial_mv_pred_flag);
                let cabac_start = (r.position_bits() + 7) / 8;
                let mut e = CabacEngine::new(&rbsp[cabac_start..]);
                let mut ctx = InterContexts::new(idc, slice_qp);
                let mut rctx = ResidualContexts::new(slice_qp, true);

                let mut skip_grid: Vec<bool> = Vec::new();
                let mut mbtype_grid: Vec<i64> = Vec::new(); // 0 for skip/direct
                let mut cbp_grid: Vec<CbpBits> = Vec::new();
                let mut cbpv: Vec<u8> = Vec::new();
                let mut t8grid: Vec<bool> = Vec::new();
                // Per-4×4 mvd per list (for the mvd ctxIdxInc).
                let mut mvd_l = [vec![[0i32; 2]; bw4 * bh4], vec![[0i32; 2]; bw4 * bh4]];
                let mut last_dquant = 0;
                let mut qp = slice_qp;
                let mut addr = 0usize;

                loop {
                    assert!(addr < bw4 / 4 * (bh4 / 4), "ran past frame at addr {addr} — desync");
                    let (mbx, mby) = (addr % width, addr / width);
                    let (mbx4, mby4) = (mbx * 4, mby * 4);
                    let left_ns = if mbx != 0 { (!skip_grid[addr - 1]) as u32 } else { 0 };
                    let up_ns = if addr >= width { (!skip_grid[addr - width]) as u32 } else { 0 };
                    let (is_skip, v1) = decode_mb_skip_flag_b(&mut e, &mut ctx, left_ns, up_ns);
                    decoded.push(("mb_skip_flag".into(), v1, None));
                    skip_grid.push(is_skip);

                    if is_skip {
                        mbtype_grid.push(0);
                        cbp_grid.push(CbpBits::default());
                        cbpv.push(0);
                        t8grid.push(false);
                        // JM traces end_of_slice_flag only when 0 (continue).
                        let eos = e.decode_terminate();
                        if eos == 1 {
                            break;
                        }
                        decoded.push(("end_of_slice_flag".into(), eos as i64, None));
                        addr += 1;
                        continue;
                    }

                    // B mb_type (ctxIdxInc from neighbour mb_type != 0).
                    let a = if mbx != 0 { (mbtype_grid[addr - 1] != 0) as u32 } else { 0 };
                    let b = if addr >= width { (mbtype_grid[addr - width] != 0) as u32 } else { 0 };
                    let mb_type = decode_b_mb_type(&mut e, &mut ctx, a, b);
                    if std::env::var("DBG").is_ok() {
                        eprintln!("addr {addr}: mb_type {mb_type}");
                    }
                    decoded.push(("mb_type".into(), mb_type, None));
                    mbtype_grid.push(mb_type);
                    assert!(mb_type <= 22, "intra-in-B not handled (mb_type {mb_type})");

                    // Build the per-list mvd-partition list (gx,gy,w4,h4,pdir).
                    let mut mvd_parts: Vec<(usize, usize, usize, usize, u8)> = Vec::new();
                    let mut all_subs_ge_8x8 = true;
                    if mb_type == 22 {
                        let mut subs = [0i64; 4];
                        for (b8, s) in subs.iter_mut().enumerate() {
                            *s = decode_b_sub_mb_type(&mut e, &mut ctx);
                            decoded.push(("sub_mb_type".into(), *s, None));
                            if *s > 3 {
                                all_subs_ge_8x8 = false;
                            }
                            let (bx8, by8) = (b8 & 1, b8 >> 1);
                            let (pdir, parts) = sub_geom(*s);
                            for (dx, dy, w4, h4) in parts {
                                mvd_parts.push((mbx4 + bx8 * 2 + dx, mby4 + by8 * 2 + dy, w4, h4, pdir));
                            }
                        }
                    } else if mb_type != 0 {
                        // mb_type 0 = B_Direct_16x16: direct, no mvd coded.
                        let (nparts, pw4, ph4, pdir) = interpret_b_mb_type(mb_type);
                        for p in 0..nparts {
                            let (dx, dy) = if pw4 == 4 && ph4 == 2 {
                                (0, p * 2) // 16×8
                            } else if pw4 == 2 && ph4 == 4 {
                                (p * 2, 0) // 8×16
                            } else {
                                (0, 0) // 16×16
                            };
                            mvd_parts.push((mbx4 + dx, mby4 + dy, pw4, ph4, pdir[p]));
                        }
                    }

                    // Read mvd: list 0 all partitions, then list 1.
                    for list in 0..2usize {
                        for &(gx, gy, w4, h4, pdir) in &mvd_parts {
                            if pdir as usize != list && pdir != 2 {
                                continue;
                            }
                            // JM labels the two components of a full-MB (16×16)
                            // partition "mvd{k}_l{list}"; multi-partition modes
                            // use plain "mvd_l{list}".
                            let indexed = w4 == 4 && h4 == 4;
                            let mut comp = [0i32; 2];
                            for (k, c) in comp.iter_mut().enumerate() {
                                let lv = if gx > 0 { mvd_l[list][gy * bw4 + gx - 1][k].abs() } else { 0 };
                                let uv = if gy > 0 { mvd_l[list][(gy - 1) * bw4 + gx][k].abs() } else { 0 };
                                let inc = mvd_ctx_inc(lv + uv);
                                let val = decode_mvd_component(&mut e, &mut ctx, k, inc) as i32;
                                let lab = if indexed { format!("mvd{k}_l{list}") } else { format!("mvd_l{list}") };
                                decoded.push((lab, val as i64, None));
                                *c = val;
                            }
                            for j in 0..h4 {
                                for i in 0..w4 {
                                    mvd_l[list][(gy + j) * bw4 + gx + i] = comp;
                                }
                            }
                        }
                    }

                    // cbp / transform / qp / residual.
                    let up = if addr >= width { Some(cbpv[addr - width]) } else { None };
                    let left = if mbx != 0 { Some(cbpv[addr - 1]) } else { None };
                    let cbp = decode_cbp_ctx(&mut e, &mut ctx.cbp, up, left);
                    decoded.push(("coded_block_pattern".into(), cbp, None));
                    let mut transform8x8 = false;
                    if cbp & 0x0f != 0 && pps.transform_8x8_mode_flag && all_subs_ge_8x8 {
                        let lt = if mbx != 0 { t8grid[addr - 1] as usize } else { 0 };
                        let ut = if addr >= width { t8grid[addr - width] as usize } else { 0 };
                        let t = e.decode_decision(&mut ctx.transform[lt + ut]);
                        transform8x8 = t == 1;
                        decoded.push(("transform_size_8x8_flag".into(), t as i64, None));
                    }
                    let delta = if cbp != 0 {
                        let d = decode_dquant_ctx(&mut e, &mut ctx.delta_qp, &mut last_dquant);
                        decoded.push(("mb_qp_delta".into(), d as i64, None));
                        d
                    } else {
                        last_dquant = 0;
                        0
                    };
                    qp = (qp + delta).rem_euclid(52);

                    let info = MbInfo { i_nxn: false, transform8x8, c_ipred: 0, cbp: cbp as u8, i16_pred: 0 };
                    let mut rneigh = CbfNeighbours {
                        cur: CbpBits::default(),
                        left: if mbx != 0 { Some(cbp_grid[addr - 1]) } else { None },
                        up: if addr >= width { Some(cbp_grid[addr - width]) } else { None },
                    };
                    decode_mb_residual(&mut e, &mut rctx, &info, &mut rneigh, qp, pps.chroma_qp_index_offset, true, &mut decoded);
                    cbp_grid.push(rneigh.cur);
                    cbpv.push(cbp as u8);
                    t8grid.push(transform8x8);

                    let eos = e.decode_terminate();
                    if eos == 1 {
                        break;
                    }
                    decoded.push(("end_of_slice_flag".into(), eos as i64, None));
                    addr += 1;
                }
                eprintln!("decoded {} MBs ({} skipped)", skip_grid.len(), skip_grid.iter().filter(|&&s| s).count());
                break;
            }
            _ => {}
        }
    }

    // Locate the first B-slice in the trace: the 3rd slice with mb_skip_flag
    // (P is the 1st, B-slices follow). Each slice = 48 MBs = 48 mb_skip_flags.
    let trace = parse_trace(&std::fs::read_to_string(&trace_path)?);
    let mb = macroblock_elements(&trace);
    let skip_positions: Vec<usize> = mb.iter().enumerate().filter(|(_, e)| e.name == "mb_skip_flag").map(|(i, _)| i).collect();
    let b_start = skip_positions[48]; // P MBs 0..47, then B-slice MB 0
    let b1_len = skip_positions[96] - b_start; // up to B-slice 2's MB 0
    eprintln!("libmvc emitted {} elements; JM B-slice1 has {b1_len}", decoded.len());
    let reference = &mb[b_start..];

    for (i, d) in decoded.iter().enumerate() {
        let t = reference[i];
        if t.name != d.0 || t.value != d.1 || t.value2 != d.2 {
            eprintln!("✗ diverged at B-element {i}: JM ({:?}, {}, {:?}) vs libmvc {d:?}", t.name, t.value, t.value2);
            std::process::exit(1);
        }
    }
    eprintln!("✓ all {} B-slice syntax elements match JM trace exactly", decoded.len());
    Ok(())
}
