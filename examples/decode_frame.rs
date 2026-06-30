// Reconstruct the base-view I-slice's luma plane macroblock by macroblock —
// CABAC decode + intra prediction + residual + clip — and diff the pixels
// against JM's pre-deblock reconstruction (docs/libmvc-poc.md § Validation).
// This ties together every primitive: the syntax decode, derive_intra_mode,
// predict_8x8 + the reference-sample filter, the 8×8 inverse transform with
// the stream's scaling list, and the neighbour-sample gathering.
//
// Reference: ldecod built with the DUMP_RECON hook writes the pre-deblock
// luma plane; this frame is one slice (11 of 68 MB rows), so we compare only
// the decoded region. Deblocking is not applied on either side.
//
//   cargo run --release --example decode_frame -- base1.h264 recon_y.bin

use ripsaw::mvc::annexb::NalSplitter;
use ripsaw::mvc::bitstream::BitReader;
use ripsaw::mvc::cabac::CabacEngine;
use ripsaw::mvc::intra::{derive_intra_mode, predict_8x8, Intra4x4Mode, Neighbors8x8};
use ripsaw::mvc::mb_header::{decode_mb_header, MbHeaderContexts, MbInfo, Neighbors};
use ripsaw::mvc::nal::parse_nal_unit_header;
use ripsaw::mvc::pps::{parse_pic_parameter_set, Pps};
use ripsaw::mvc::rbsp::extract_rbsp;
use ripsaw::mvc::residual::decode_residual_block;
use ripsaw::mvc::residual_ctx::ResidualCat;
use ripsaw::mvc::scaling::ScalingLists;
use ripsaw::mvc::slice_header::parse_slice_header;
use ripsaw::mvc::sps::{parse_seq_parameter_set_data, Sps};
use ripsaw::mvc::transform::{inverse_scan_8x8, reconstruct_residual_8x8};

struct Frame {
    y: Vec<u8>,
    w: usize,
}
impl Frame {
    fn at(&self, x: usize, y: usize) -> i32 {
        self.y[y * self.w + x] as i32
    }
}

fn main() -> anyhow::Result<()> {
    let h264 = std::env::args().nth(1).ok_or_else(|| anyhow::anyhow!("usage: decode_frame <base1.h264> <recon_y.bin>"))?;
    let ref_path = std::env::args().nth(2).ok_or_else(|| anyhow::anyhow!("missing recon_y.bin"))?;
    let data = std::fs::read(&h264)?;

    let mut sps: Option<Sps> = None;
    let mut pps: Option<Pps> = None;

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
                let frame_w = width * 16;
                let frame_h = sps.pic_height_in_map_units as usize * 16;
                let weight = pps
                    .scaling
                    .as_ref()
                    .or(sps.scaling.as_ref())
                    .cloned()
                    .unwrap_or_else(ScalingLists::flat);
                let weight8 = *weight.intra_8x8_luma();

                let mut r = BitReader::new(&rbsp);
                let sh = parse_slice_header(&mut r, true, hdr.nal_ref_idc, sps, pps)?;
                let slice_qp = 26 + pps.pic_init_qp_minus26 + sh.slice_qp_delta;
                let cabac_start = (r.position_bits() + 7) / 8;
                let mut e = CabacEngine::new(&rbsp[cabac_start..]);
                let mut ctx = MbHeaderContexts::new(slice_qp);
                let mut last_dquant = 0;

                let mut frame = Frame { y: vec![0u8; frame_w * frame_h], w: frame_w };
                let bw = width * 2; // 8×8 blocks across
                let mut modes = vec![255u8; bw * frame_h / 8]; // per-8×8-block mode grid
                let mut grid: Vec<MbInfo> = Vec::new();
                let mut qp = slice_qp;
                let mut addr = 0usize;

                'slice: loop {
                    let mbx = addr % width;
                    let mby = addr / width;
                    let left = if mbx != 0 { grid.get(addr - 1).copied() } else { None };
                    let up = if addr >= width { grid.get(addr - width).copied() } else { None };
                    let mut header: Vec<(String, i64)> = Vec::new();
                    let info = decode_mb_header(&mut e, &mut ctx, &Neighbors { left, up }, &mut last_dquant, &mut header);

                    if info.cbp != 0 && (!info.i_nxn || !info.transform8x8 || info.cbp & 0x30 != 0) {
                        eprintln!("MB {addr}: unsupported residual (i_nxn={}, t8x8={}, cbp={}); stopping", info.i_nxn, info.transform8x8, info.cbp);
                        break 'slice;
                    }
                    // Track QP from mb_qp_delta (emitted only when cbp != 0).
                    if let Some((_, d)) = header.iter().find(|(n, _)| n == "mb_qp_delta") {
                        qp = (qp + *d as i32).rem_euclid(52);
                    }
                    // The four luma 8×8 intra modes (raw: −1 or rem).
                    let raw: Vec<i64> = header.iter().filter(|(n, _)| n == "intra4x4_pred_mode").map(|(_, v)| *v).collect();

                    for b in 0..4usize {
                        let gbx = mbx * 2 + (b & 1);
                        let gby = mby * 2 + (b >> 1);
                        let cur_seq = seq(gbx, gby, width);
                        let mode_a = (gbx > 0).then(|| modes[gby * bw + gbx - 1]).filter(|&m| m != 255);
                        let mode_b = (gby > 0).then(|| modes[(gby - 1) * bw + gbx]).filter(|&m| m != 255);
                        let mode = derive_intra_mode(mode_a, mode_b, raw[b]);
                        modes[gby * bw + gbx] = mode;

                        let neigh = gather(&frame, gbx, gby, bw, width, cur_seq);
                        let pred = predict_8x8(Intra4x4Mode::from_index(mode as u32).unwrap(), &neigh);

                        let resid = if info.cbp & (1 << b) != 0 {
                            let cat = ResidualCat::Luma8x8;
                            let d = cat.desc();
                            let mut cctx = cat.coeff_contexts(slice_qp);
                            let coeffs = decode_residual_block(&mut e, &mut cctx, d.max_num_coeff, d.pos2ctx_map, d.pos2ctx_last, d.gt1_cap);
                            let mut scan = [0i32; 64];
                            scan.copy_from_slice(&coeffs[..64]);
                            reconstruct_residual_8x8(&inverse_scan_8x8(&scan), qp, &weight8)
                        } else {
                            [[0i32; 8]; 8]
                        };

                        for yy in 0..8 {
                            for xx in 0..8 {
                                let v = (pred[yy][xx] + resid[yy][xx]).clamp(0, 255) as u8;
                                let (px, py) = (gbx * 8 + xx, gby * 8 + yy);
                                frame.y[py * frame.w + px] = v;
                            }
                        }
                    }

                    let eos = e.decode_terminate();
                    grid.push(info);
                    if eos == 1 {
                        break 'slice;
                    }
                    addr += 1;
                }

                // Diff the decoded region against the JM reference. The last
                // fully decoded MB row is (grid.len()-1)/width.
                let reference = std::fs::read(&ref_path)?;
                let rows = ((grid.len() - 1) / width + 1) * 16; // decoded luma lines
                eprintln!("decoded {} MBs ({} luma rows); diffing {rows}×{frame_w}", grid.len(), rows);
                let mut mism = 0usize;
                let mut first: Option<(usize, usize, u8, u8)> = None;
                for y in 0..rows {
                    for x in 0..frame_w {
                        let got = frame.at(x, y) as u8;
                        let exp = reference[y * frame_w + x];
                        if got != exp {
                            mism += 1;
                            if first.is_none() {
                                first = Some((x, y, got, exp));
                            }
                        }
                    }
                }
                if mism == 0 {
                    eprintln!("✓ luma reconstruction MATCHES JM exactly over {rows}×{frame_w} ({} samples)", rows * frame_w);
                } else {
                    let (x, y, g, e) = first.unwrap();
                    eprintln!("✗ {mism} mismatches; first at ({x},{y}): libmvc={g} JM={e}");
                    std::process::exit(1);
                }
                break;
            }
            _ => {}
        }
    }
    Ok(())
}

/// Decode-order sequence number of the 8×8 block at (gbx, gby).
fn seq(gbx: usize, gby: usize, width: usize) -> usize {
    let (mbx, mby) = (gbx / 2, gby / 2);
    let block_in_mb = (gby & 1) * 2 + (gbx & 1);
    (mby * width + mbx) * 4 + block_in_mb
}

/// Gather the Intra_8x8 reference samples for block (gbx, gby) from the
/// reconstructed frame, with availability (above-right replicates top[7]).
fn gather(frame: &Frame, gbx: usize, gby: usize, bw: usize, width: usize, cur_seq: usize) -> Neighbors8x8 {
    let (px, py) = (gbx * 8, gby * 8);
    let left_avail = gbx > 0;
    let top_avail = gby > 0;
    let corner_avail = left_avail && top_avail;
    let ar_avail = top_avail && gbx + 1 < bw && seq(gbx + 1, gby - 1, width) < cur_seq;

    let mut top = [0i32; 16];
    let mut left = [0i32; 8];
    let mut corner = 0;
    if top_avail {
        for x in 0..8 {
            top[x] = frame.at(px + x, py - 1);
        }
        for x in 8..16 {
            top[x] = if ar_avail { frame.at(px + x, py - 1) } else { top[7] };
        }
    }
    if left_avail {
        for (y, slot) in left.iter_mut().enumerate() {
            *slot = frame.at(px - 1, py + y);
        }
    }
    if corner_avail {
        corner = frame.at(px - 1, py - 1);
    }
    Neighbors8x8 { top, left, corner, top_avail, left_avail, corner_avail }
}
