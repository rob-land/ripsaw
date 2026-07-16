// Full base-view IBP clip decode (I/P/B, multi-slice) validated post-deblock
// vs JM's per-view display-order output. Exercises decode_b_frame + deblock_b
// on a real hierarchical-GOP MVC stream's base view.
//
//   decode_bclip -- clip.h264 out_ViewId0000.yuv
//
// Status: all 48 base-view frames (I/P/B, multi-slice) decode bit-exact vs JM
// post-deblock. Diagnostic env knobs: RP_COUNT (per-frame diff count), RP_FIND
// (best-match JM frame), RP_PRE=<dump> (compare pre-deblock recon vs JM's
// DUMP_RECON, decode order).

use ripsaw::mvc::annexb::NalSplitter;
use ripsaw::mvc::bitstream::BitReader;
use ripsaw::mvc::nal::parse_nal_unit_header;
use ripsaw::mvc::pps::{parse_pic_parameter_set, Pps};
use ripsaw::mvc::rbsp::extract_rbsp;
use ripsaw::mvc::recon::{decode_intra_frame, Frame};
use ripsaw::mvc::recon_inter::{deblock_b, deblock_inter, decode_b_frame, decode_p_frame, MotionField};
use ripsaw::mvc::slice_header::parse_slice_header;
use ripsaw::mvc::sps::{parse_seq_parameter_set_data, Sps};

const W: usize = 1920;
const H: usize = 1080;

#[derive(Default)]
struct Au {
    idr: bool,
    idc: u8,
    slices: Vec<Vec<u8>>,
}

fn main() -> anyhow::Result<()> {
    let data = std::fs::read(std::env::args().nth(1).unwrap())?;
    let jm = std::fs::read(std::env::args().nth(2).unwrap())?;
    let (mut sps, mut pps): (Option<Sps>, Option<Pps>) = (None, None);
    let mut aus: Vec<Au> = Vec::new();
    for nal in NalSplitter::new(&data) {
        if nal.is_empty() {
            continue;
        }
        let Ok((h, c)) = parse_nal_unit_header(nal) else { continue };
        let rbsp = extract_rbsp(&nal[c..]);
        match h.nal_unit_type {
            9 => aus.push(Au::default()),
            7 => sps = Some(parse_seq_parameter_set_data(&mut BitReader::new(&rbsp))?),
            8 => pps = Some(parse_pic_parameter_set(&mut BitReader::new(&rbsp), sps.as_ref().unwrap().chroma_format_idc)?),
            5 | 1 => {
                let au = aus.last_mut().unwrap();
                au.idr = h.nal_unit_type == 5;
                au.idc = h.nal_ref_idc;
                au.slices.push(rbsp.to_vec());
            }
            _ => {}
        }
    }
    aus.retain(|a| !a.slices.is_empty());
    let (sps, pps) = (sps.as_ref().unwrap(), pps.as_ref().unwrap());

    // Per-GOP reference set: (poc, frame, L0 motion field). Cleared at IDR.
    let mut refs: Vec<(i32, Frame, MotionField)> = Vec::new();
    // Decoded frames tagged with a global display index (gop_start + poc/2).
    let mut out: Vec<(usize, Frame)> = Vec::new();
    let mut gop_start = 0usize; // display index of the current GOP's first frame
    let mut prev_gop_frames = 0usize;
    // POC type 0 state (§ 8.2.1.1), updated by reference pictures only.
    let max_lsb = 1i32 << (sps.log2_max_pic_order_cnt_lsb_minus4 + 4);
    let (mut prev_msb, mut prev_lsb) = (0i32, 0i32);
    let mut dcidx = 0usize; // decode-order index (matches preall.bin)

    // Pre-deblock recon check against JM's DUMP_RECON dump (padded 1920x1088,
    // decode order). Isolates recon from deblock. Enable with RP_PRE=<path>.
    let preall = std::env::var("RP_PRE").ok().map(|p| std::fs::read(p).unwrap());
    let check_pre = |f: &Frame, idx: usize, st: u32| {
        let Some(pre) = &preall else { return };
        let fsz = f.fw * f.fh + 2 * (f.cw * f.ch);
        let off = idx * fsz;
        if off + fsz > pre.len() { return; }
        let mut n = 0u64;
        let mut mx = 0i32;
        let mut first = None;
        use std::collections::BTreeSet;
        let mut mbs = BTreeSet::new();
        for yy in 0..f.fh {
            for xx in 0..f.fw {
                let i = yy * f.fw + xx;
                let d = (f.y[i] as i32 - pre[off + i] as i32).abs();
                if d != 0 { if first.is_none() { first = Some((xx, yy)); } n += 1; mx = mx.max(d); mbs.insert((xx / 16, yy / 16)); }
            }
        }
        let kind = ["P", "B", "I"][(st % 5) as usize];
        eprintln!("  [pre-deblock] decode {idx} ({kind}): {n} Y diffs, max {mx}, first {first:?}, {} MBs {:?}", mbs.len(), mbs.iter().take(8).collect::<Vec<_>>());
    };

    for au in &aus {
        let slices: Vec<&[u8]> = au.slices.iter().map(|s| s.as_slice()).collect();
        let sh = parse_slice_header(&mut BitReader::new(&au.slices[0]), au.idr, au.idc, sps, pps)?;
        let lsb = sh.pic_order_cnt_lsb.unwrap_or(0) as i32;
        let st = sh.slice_type % 5;
        if au.idr {
            gop_start += prev_gop_frames;
            prev_gop_frames = 0;
            refs.clear();
            prev_msb = 0;
            prev_lsb = 0;
        }
        // Full POC (§ 8.2.1.1): resolve the LSB wrap against the last ref pic.
        let msb = if au.idr {
            0
        } else if lsb < prev_lsb && prev_lsb - lsb >= max_lsb / 2 {
            prev_msb + max_lsb
        } else if lsb > prev_lsb && lsb - prev_lsb > max_lsb / 2 {
            prev_msb - max_lsb
        } else {
            prev_msb
        };
        let poc = msb + lsb;
        if au.idc != 0 {
            // Reference picture: advance the POC prediction state.
            prev_msb = msb;
            prev_lsb = lsb;
        }
        prev_gop_frames = prev_gop_frames.max(poc as usize / 2 + 1);
        let disp = gop_start + poc as usize / 2;

        if st == 2 {
            let f0 = decode_intra_frame(&slices, au.idc, au.idr, sps, pps)?;
            check_pre(&f0, dcidx, st);
            let mut f = clone_frame(&f0);
            f.deblock_intra(pps.chroma_qp_index_offset);
            refs.push((poc, clone_frame(&f), MotionField { mv: vec![], refidx: vec![], refpoc: vec![], nz: vec![], bw4: 0, bh4: 0 }));
            out.push((disp, f));
        } else if st == 0 {
            // P: single ref = nearest past by POC.
            let r = refs.iter().filter(|(p, ..)| *p < poc).max_by_key(|(p, ..)| *p).expect("P past ref");
            let rpoc = r.0;
            let (mut f, mut mf) = decode_p_frame(&slices, au.idc, false, sps, pps, &[&r.1])?;
            mf.resolve_refpoc(&[rpoc]);
            check_pre(&f, dcidx, st);
            deblock_inter(&mut f, &mf, pps.chroma_qp_index_offset);
            refs.push((poc, clone_frame(&f), mf));
            out.push((disp, f));
        } else {
            // B: L0 = nearest past, L1 = nearest future; col = L1's motion field.
            let l0 = refs.iter().filter(|(p, ..)| *p < poc).max_by_key(|(p, ..)| *p).expect("B L0");
            let l1 = refs.iter().filter(|(p, ..)| *p > poc).min_by_key(|(p, ..)| *p).expect("B L1");
            let (mut f, bmf) = decode_b_frame(&slices, au.idc, false, sps, pps, &[(&l0.1, l0.0)], &[(&l1.1, l1.0)], poc, &l1.2, (32, 32))?;
            check_pre(&f, dcidx, st);
            deblock_b(&mut f, &bmf, pps.chroma_qp_index_offset);
            out.push((disp, f));
        }
        dcidx += 1;
    }

    // Compare each decoded frame against JM at its display index.
    let fsz = W * H + 2 * (W / 2) * (H / 2);
    let mut okall = true;
    out.sort_by_key(|(d, _)| *d);

    if std::env::var("RP_FIND").is_ok() {
        let njm = jm.len() / fsz;
        for (disp, f) in out.iter().take(6) {
            let mut best = (usize::MAX, u64::MAX);
            for k in 0..njm {
                let off = k * fsz;
                let mut n = 0u64;
                for yy in 0..H { for xx in 0..W { if f.y[yy * f.fw + xx] != jm[off + yy * W + xx] { n += 1; } } }
                if n < best.1 { best = (k, n); }
            }
            eprintln!("my disp {disp}: best-match JM frame {} ({} Y diffs)", best.0, best.1);
        }
        return Ok(());
    }
    for (disp, f) in &out {
        let off = disp * fsz;
        if off + fsz > jm.len() {
            eprintln!("… display {disp} beyond JM output ({} frames) — skipping", jm.len() / fsz);
            continue;
        }
        let ok = cmp_frame(f, &jm, off);
        if !ok {
            eprintln!("✗ display frame {disp} mismatch");
            okall = false;
            break;
        }
    }
    if okall {
        eprintln!("✓ base view: {} frames (I/P/B) decoded bit-exact vs JM post-deblock", out.len());
    } else {
        std::process::exit(1);
    }
    Ok(())
}

fn cmp_frame(f: &Frame, jm: &[u8], off: usize) -> bool {
    let (cw, ch) = (W / 2, H / 2);
    if std::env::var("RP_COUNT").is_ok() {
        let (mut n, mut maxd, mut fx, mut fy) = (0usize, 0i32, 0, 0);
        for yy in 0..H {
            for xx in 0..W {
                let d = (f.y[yy * f.fw + xx] as i32 - jm[off + yy * W + xx] as i32).abs();
                if d != 0 { if n == 0 { fx = xx; fy = yy; } n += 1; maxd = maxd.max(d); }
            }
        }
        if n > 0 {
            use std::collections::BTreeSet;
            let mut mbs = BTreeSet::new();
            for yy in 0..H { for xx in 0..W { if f.y[yy*f.fw+xx] != jm[off+yy*W+xx] { mbs.insert((xx/16, yy/16)); } } }
            eprintln!("  Y: {n} px differ, max|Δ|={maxd}, first ({fx},{fy}); {} MBs: {:?}", mbs.len(), mbs.iter().take(20).collect::<Vec<_>>());
            return false;
        }
    }
    for yy in 0..H {
        for xx in 0..W {
            if f.y[yy * f.fw + xx] != jm[off + yy * W + xx] {
                eprintln!("  Y ({xx},{yy}) MB({},{}): {} vs {}", xx / 16, yy / 16, f.y[yy * f.fw + xx], jm[off + yy * W + xx]);
                return false;
            }
        }
    }
    for (plane, base) in [(&f.cb, W * H), (&f.cr, W * H + cw * ch)] {
        for yy in 0..ch {
            for xx in 0..cw {
                if plane[yy * f.cw + xx] != jm[off + base + yy * cw + xx] {
                    eprintln!("  C ({xx},{yy})");
                    return false;
                }
            }
        }
    }
    true
}

fn clone_frame(f: &Frame) -> Frame {
    Frame {
        y: f.y.clone(),
        cb: f.cb.clone(),
        cr: f.cr.clone(),
        fw: f.fw,
        fh: f.fh,
        cw: f.cw,
        ch: f.ch,
        width_mbs: f.width_mbs,
        mb_info: f.mb_info.clone(),
        qp: f.qp.clone(),
        disable_deblock_idc: f.disable_deblock_idc,
        slice_alpha_c0_offset_div2: f.slice_alpha_c0_offset_div2,
        slice_beta_offset_div2: f.slice_beta_offset_div2,
    }
}
