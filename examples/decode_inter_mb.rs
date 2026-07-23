// First inter PIXELS: reconstruct the P-slice's skip prefix (MV 0 → copy the
// reference) and the first coded MB (P_16x8: MC at its MVs + residual), and
// diff against JM's P-frame reconstruction. Uses JM's decoded IDR (frame 0 of
// inter_post.yuv) as the reference, isolating the inter MC + MV + residual
// integration from the reference-frame decode.
//
//   cargo run --release --example decode_inter_mb -- inter.h264 inter_post.yuv inter_predeblock.bin

use ripsaw::mvc::annexb::NalSplitter;
use ripsaw::mvc::bitstream::BitReader;
use ripsaw::mvc::cabac::CabacEngine;
use ripsaw::mvc::mb_header::{decode_cbp_ctx, decode_dquant_ctx, MbInfo};
use ripsaw::mvc::mb_inter::{decode_inter_mb_type, decode_mb_skip_flag, decode_mvd_component, InterContexts};
use ripsaw::mvc::mb_residual::{decode_mb_residual, CbfNeighbours, CbpBits, ResidualContexts};
use ripsaw::mvc::mc::{mc_chroma, mc_luma, Plane};
use ripsaw::mvc::nal::parse_nal_unit_header;
use ripsaw::mvc::pps::{parse_pic_parameter_set, Pps};
use ripsaw::mvc::rbsp::extract_rbsp;
use ripsaw::mvc::slice_header::parse_slice_header;
use ripsaw::mvc::sps::{parse_seq_parameter_set_data, Sps};

fn main() -> anyhow::Result<()> {
    let h264 = std::env::args().nth(1).unwrap();
    let ref_yuv = std::env::args().nth(2).unwrap(); // inter_post.yuv (frame 0 = ref)
    let jm_p = std::env::args().nth(3).unwrap(); // inter_predeblock.bin (P-frame)
    let data = std::fs::read(&h264)?;

    let mut sps: Option<Sps> = None;
    let mut pps: Option<Pps> = None;
    let mut idr_done = false;

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
            5 => idr_done = true,
            1 if idr_done => {
                let (sps, pps) = (sps.as_ref().unwrap(), pps.as_ref().unwrap());
                let width = sps.pic_width_in_mbs as usize;
                let (fw, fh) = (width * 16, sps.pic_height_in_map_units as usize * 16);
                let (cw, ch) = (fw / 2, fh / 2);

                // Reference = JM's IDR (frame 0 of inter_post.yuv).
                let r = std::fs::read(&ref_yuv)?;
                let (ysz, csz) = (fw * fh, cw * ch);
                let ref_y = r[..ysz].to_vec();
                let ref_cb = r[ysz..ysz + csz].to_vec();
                let ref_cr = r[ysz + csz..ysz + 2 * csz].to_vec();
                let rpy = Plane { data: &ref_y, w: fw, h: fh };
                let rpcb = Plane { data: &ref_cb, w: cw, h: ch };
                let rpcr = Plane { data: &ref_cr, w: cw, h: ch };

                let mut sh_reader = BitReader::new(&rbsp);
                let sh = parse_slice_header(&mut sh_reader, false, hdr.nal_ref_idc, sps, pps)?;
                let slice_qp = 26 + pps.pic_init_qp_minus26 + sh.slice_qp_delta;
                let idc = sh.cabac_init_idc.unwrap_or(0);
                let cabac_start = (sh_reader.position_bits() + 7) / 8;
                let mut e = CabacEngine::new(&rbsp[cabac_start..]);
                let mut ctx = InterContexts::new(idc, slice_qp);

                // Output planes (only the region we reconstruct is filled).
                let mut y = vec![0u8; ysz];
                let mut cb = vec![0u8; csz];
                let mut cr = vec![0u8; csz];

                let put_luma = |y: &mut [u8], px: usize, py: usize, bw: usize, bh: usize, pred: &[u8], res: &[[i32; 16]; 16], rx: usize, ry: usize| {
                    for j in 0..bh {
                        for i in 0..bw {
                            let v = (pred[j * bw + i] as i32 + res[ry + j][rx + i]).clamp(0, 255) as u8;
                            y[(py + j) * fw + px + i] = v;
                        }
                    }
                };
                let put_chroma = |c: &mut [u8], px: usize, py: usize, bw: usize, bh: usize, pred: &[u8], res: &[[i32; 8]; 8], ry: usize| {
                    for j in 0..bh {
                        for i in 0..bw {
                            let v = (pred[j * bw + i] as i32 + res[ry + j][i]).clamp(0, 255) as u8;
                            c[(py + j) * cw + px + i] = v;
                        }
                    }
                };

                let mut skip_grid: Vec<bool> = Vec::new();
                let mut addr = 0usize;
                let mut last_coded = 0usize;
                let zero = [[0i32; 16]; 16];
                let zerc = [[0i32; 8]; 8];
                loop {
                    let mbx = addr % width;
                    let mby = addr / width;
                    let (mpx, mpy, cpx, cpy) = (mbx * 16, mby * 16, mbx * 8, mby * 8);
                    let left_ns = if mbx != 0 { skip_grid.get(addr - 1).map(|&s| (!s) as u32).unwrap_or(0) } else { 0 };
                    let up_ns = if addr >= width { skip_grid.get(addr - width).map(|&s| (!s) as u32).unwrap_or(0) } else { 0 };
                    let (is_skip, _) = decode_mb_skip_flag(&mut e, &mut ctx, left_ns, up_ns);
                    skip_grid.push(is_skip);

                    if is_skip {
                        // P_Skip with all-zero neighbours -> MV 0 -> copy ref.
                        put_luma(&mut y, mpx, mpy, 16, 16, &mc_luma(&rpy, mpx as i32, mpy as i32, 0, 0, 16, 16), &zero, 0, 0);
                        put_chroma(&mut cb, cpx, cpy, 8, 8, &mc_chroma(&rpcb, cpx as i32, cpy as i32, 0, 0, 8, 8), &zerc, 0);
                        put_chroma(&mut cr, cpx, cpy, 8, 8, &mc_chroma(&rpcr, cpx as i32, cpy as i32, 0, 0, 8, 8), &zerc, 0);
                        let eos = e.decode_terminate();
                        if eos == 1 {
                            break;
                        }
                        addr += 1;
                        continue;
                    }

                    // First coded MB (P_16x8 here; mvp = 0 since neighbours are MV-0 skips).
                    let mb_type = decode_inter_mb_type(&mut e, &mut ctx);
                    assert_eq!(mb_type, 2, "expected P_16x8 for this stream's first coded MB");
                    let p0 = (decode_mvd_component(&mut e, &mut ctx, 0, 0) as i32, decode_mvd_component(&mut e, &mut ctx, 1, 0) as i32);
                    let p1 = (decode_mvd_component(&mut e, &mut ctx, 0, 0) as i32, decode_mvd_component(&mut e, &mut ctx, 1, 0) as i32);
                    let cbp = decode_cbp_ctx(&mut e, &mut ctx.cbp, Some(0), None);
                    let mut transform8x8 = false;
                    if cbp & 0x0f != 0 && pps.transform_8x8_mode_flag {
                        transform8x8 = e.decode_decision(&mut ctx.transform[0]) == 1;
                    }
                    let mut last_dquant = 0;
                    if cbp != 0 {
                        decode_dquant_ctx(&mut e, &mut ctx.delta_qp, &mut last_dquant);
                    }
                    let info = MbInfo { i_nxn: false, transform8x8, c_ipred: 0, cbp: cbp as u8, i16_pred: 0 };
                    let mut rctx = ResidualContexts::new(slice_qp, true, 0);
                    let mut rneigh = CbfNeighbours { cur: CbpBits::default(), left: None, up: Some(CbpBits::default()) };
                    let mut sink = Vec::new();
                    let res = decode_mb_residual(&mut e, &mut rctx, &info, &mut rneigh, slice_qp + last_dquant, pps.chroma_qp_index_offset, true, &ripsaw::mvc::scaling::ScalingLists::flat(), &mut sink);

                    // P_16x8: partition 0 = top 16x8, partition 1 = bottom 16x8.
                    for (part, (mvx, mvy)) in [p0, p1].iter().enumerate() {
                        let oy = mpy + part * 8;
                        put_luma(&mut y, mpx, oy, 16, 8, &mc_luma(&rpy, mpx as i32, oy as i32, *mvx, *mvy, 16, 8), &res.luma, 0, part * 8);
                        // 4:2:0 chroma: 8x4 per 16x8 partition, chroma MV = luma MV.
                        let coy = cpy + part * 4;
                        put_chroma(&mut cb, cpx, coy, 8, 4, &mc_chroma(&rpcb, cpx as i32, coy as i32, *mvx, *mvy, 8, 4), &res.cb, part * 4);
                        put_chroma(&mut cr, cpx, coy, 8, 4, &mc_chroma(&rpcr, cpx as i32, coy as i32, *mvx, *mvy, 8, 4), &res.cr, part * 4);
                    }
                    last_coded = addr;
                    let _ = e.decode_terminate();
                    break;
                }

                // Diff vs JM's P-frame (pre-deblock = final, deblock disabled).
                let jm = std::fs::read(&jm_p)?;
                let rows_full = (last_coded / width) * 16; // fully reconstructed luma rows (skip region)
                let mut ok = true;
                for yy in 0..rows_full {
                    for xx in 0..fw {
                        if y[yy * fw + xx] != jm[yy * fw + xx] {
                            eprintln!("✗ skip region luma mismatch at ({xx},{yy}): {} vs {}", y[yy * fw + xx], jm[yy * fw + xx]);
                            ok = false;
                            break;
                        }
                    }
                    if !ok {
                        break;
                    }
                }
                // The first coded MB's 16x16 luma block.
                let (mbx, mby) = (last_coded % width, last_coded / width);
                let mut mb_ok = true;
                for j in 0..16 {
                    for i in 0..16 {
                        let (px, py) = (mbx * 16 + i, mby * 16 + j);
                        if y[py * fw + px] != jm[py * fw + px] {
                            eprintln!("✗ coded MB {last_coded} luma mismatch at rel ({i},{j}): {} vs {}", y[py * fw + px], jm[py * fw + px]);
                            mb_ok = false;
                            break;
                        }
                    }
                    if !mb_ok {
                        break;
                    }
                }
                if ok {
                    eprintln!("✓ skip region ({} MBs, {rows_full} luma rows) matches JM — MC zero-MV copy", last_coded);
                }
                if mb_ok {
                    eprintln!("✓ first coded MB {last_coded} (P_16x8, MC + residual) luma matches JM");
                }
                if ok && mb_ok {
                    eprintln!("✓ first inter pixels reconstructed bit-exact vs JM");
                } else {
                    std::process::exit(1);
                }
                break;
            }
            _ => {}
        }
    }
    Ok(())
}
