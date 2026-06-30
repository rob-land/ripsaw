// Micro-benchmark: how fast does the (partial) libmvc intra path decode +
// reconstruct, in MB/s, and what does that extrapolate to for realtime
// playback? Loops the base-view I-slice decode N times in-process (engine +
// contexts + frame buffer re-init each iteration) so process startup and
// file I/O don't dominate. Luma-only reconstruction (the validated path).
//
//   cargo run --release --example bench_decode -- base1.h264 [iters]

use std::time::Instant;

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

fn main() -> anyhow::Result<()> {
    let h264 = std::env::args().nth(1).ok_or_else(|| anyhow::anyhow!("usage: bench_decode <base1.h264> [iters]"))?;
    let iters: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(2000);
    let data = std::fs::read(&h264)?;

    let mut sps: Option<Sps> = None;
    let mut pps: Option<Pps> = None;
    let mut slice_rbsp: Option<Vec<u8>> = None;
    let mut nal_ref_idc = 0;
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
                slice_rbsp = Some(rbsp);
                nal_ref_idc = hdr.nal_ref_idc;
                break;
            }
            _ => {}
        }
    }

    let (sps, pps, rbsp) = (sps.unwrap(), pps.unwrap(), slice_rbsp.unwrap());
    let width = sps.pic_width_in_mbs as usize;
    let frame_w = width * 16;
    let frame_h = sps.pic_height_in_map_units as usize * 16;
    let weight = pps.scaling.as_ref().or(sps.scaling.as_ref()).cloned().unwrap_or_else(ScalingLists::flat);
    let weight8 = *weight.intra_8x8_luma();

    let mut total_mbs = 0u64;
    let t0 = Instant::now();
    for _ in 0..iters {
        total_mbs += decode_one_slice(&rbsp, &sps, &pps, nal_ref_idc, width, frame_w, frame_h, &weight8);
    }
    let dt = t0.elapsed().as_secs_f64();

    let mb_per_s = total_mbs as f64 / dt;
    // 1080p = 1920×1088 = 120×68 = 8160 MBs/frame.
    let mbs_per_1080p = 8160.0;
    let fps_1080p = mb_per_s / mbs_per_1080p;
    eprintln!("decoded {total_mbs} MBs in {dt:.3} s over {iters} iters");
    eprintln!("  throughput: {:.2} M MB/s", mb_per_s / 1e6);
    eprintln!("  extrapolated 1080p (one view): {fps_1080p:.0} fps");
    eprintln!("  extrapolated 1080p 3D (two views): {:.0} fps", fps_1080p / 2.0);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn decode_one_slice(rbsp: &[u8], sps: &Sps, pps: &Pps, nal_ref_idc: u8, width: usize, frame_w: usize, frame_h: usize, weight8: &[[i32; 8]; 8]) -> u64 {
    let mut r = BitReader::new(rbsp);
    let sh = parse_slice_header(&mut r, true, nal_ref_idc, sps, pps).unwrap();
    let slice_qp = 26 + pps.pic_init_qp_minus26 + sh.slice_qp_delta;
    let cabac_start = (r.position_bits() + 7) / 8;
    let mut e = CabacEngine::new(&rbsp[cabac_start..]);
    let mut ctx = MbHeaderContexts::new(slice_qp);
    let mut last_dquant = 0;
    let mut y = vec![0u8; frame_w * frame_h];
    let bw = width * 2;
    let mut modes = vec![255u8; bw * frame_h / 8];
    let mut grid: Vec<MbInfo> = Vec::new();
    let mut qp = slice_qp;
    let mut addr = 0usize;

    loop {
        let mbx = addr % width;
        let mby = addr / width;
        let left = if mbx != 0 { grid.get(addr - 1).copied() } else { None };
        let up = if addr >= width { grid.get(addr - width).copied() } else { None };
        let mut header: Vec<(String, i64)> = Vec::new();
        let info = decode_mb_header(&mut e, &mut ctx, &Neighbors { left, up }, &mut last_dquant, &mut header);
        if info.cbp != 0 && (!info.i_nxn || !info.transform8x8 || info.cbp & 0x30 != 0) {
            break;
        }
        if let Some((_, d)) = header.iter().find(|(n, _)| n == "mb_qp_delta") {
            qp = (qp + *d as i32).rem_euclid(52);
        }
        let raw: Vec<i64> = header.iter().filter(|(n, _)| n == "intra4x4_pred_mode").map(|(_, v)| *v).collect();
        for b in 0..4usize {
            let gbx = mbx * 2 + (b & 1);
            let gby = mby * 2 + (b >> 1);
            let mode_a = (gbx > 0).then(|| modes[gby * bw + gbx - 1]).filter(|&m| m != 255);
            let mode_b = (gby > 0).then(|| modes[(gby - 1) * bw + gbx]).filter(|&m| m != 255);
            let mode = derive_intra_mode(mode_a, mode_b, raw[b]);
            modes[gby * bw + gbx] = mode;
            let neigh = gather(&y, gbx, gby, frame_w, bw, width, seq(gbx, gby, width));
            let pred = predict_8x8(Intra4x4Mode::from_index(mode as u32).unwrap(), &neigh);
            let resid = if info.cbp & (1 << b) != 0 {
                let cat = ResidualCat::Luma8x8;
                let d = cat.desc();
                let mut cctx = cat.coeff_contexts(slice_qp, false);
                let coeffs = decode_residual_block(&mut e, &mut cctx, d.max_num_coeff, d.pos2ctx_map, d.pos2ctx_last, d.gt1_cap);
                let mut scan = [0i32; 64];
                scan.copy_from_slice(&coeffs[..64]);
                reconstruct_residual_8x8(&inverse_scan_8x8(&scan), qp, weight8)
            } else {
                [[0i32; 8]; 8]
            };
            for yy in 0..8 {
                for xx in 0..8 {
                    let v = (pred[yy][xx] + resid[yy][xx]).clamp(0, 255) as u8;
                    y[(gby * 8 + yy) * frame_w + gbx * 8 + xx] = v;
                }
            }
        }
        let eos = e.decode_terminate();
        grid.push(info);
        if eos == 1 {
            break;
        }
        addr += 1;
    }
    grid.len() as u64
}

fn seq(gbx: usize, gby: usize, width: usize) -> usize {
    let (mbx, mby) = (gbx / 2, gby / 2);
    (mby * width + mbx) * 4 + (gby & 1) * 2 + (gbx & 1)
}

fn gather(y: &[u8], gbx: usize, gby: usize, frame_w: usize, bw: usize, width: usize, cur_seq: usize) -> Neighbors8x8 {
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
            top[x] = y[(py - 1) * frame_w + px + x] as i32;
        }
        for x in 8..16 {
            top[x] = if ar_avail { y[(py - 1) * frame_w + px + x] as i32 } else { top[7] };
        }
    }
    if left_avail {
        for (yy, slot) in left.iter_mut().enumerate() {
            *slot = y[(py + yy) * frame_w + px - 1] as i32;
        }
    }
    if corner_avail {
        corner = y[(py - 1) * frame_w + px - 1] as i32;
    }
    Neighbors8x8 { top, left, corner, top_avail, left_avail, corner_avail }
}
