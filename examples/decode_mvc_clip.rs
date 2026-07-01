// Full temporal MVC clip decode via the library `mvc::clip::decode_annex_b`
// — BOTH views, every access unit — diffed against JM's per-view ground
// truth. The base view is a single-ref P chain (with mid-GOP I-frames); the
// dependent view is an anchor (inter-view from base) followed by non-anchor
// 2-ref P frames whose L0 = [inter-view base, temporal previous dependent]
// per the slice's ref_pic_list_modification.
//
//   decode_mvc_clip -- full.h264 cfull_ViewId0000.yuv cfull_ViewId0001.yuv

use ripsaw::mvc::clip::decode_annex_b;
use ripsaw::mvc::recon::Frame;

// The test clips are cropped to 1920×1080; JM writes cropped per-view I420.
const W: usize = 1920;
const H: usize = 1080;

fn cmp(tag: &str, f: &Frame, jm: &[u8], off: usize, k: usize) -> bool {
    let (cw, ch) = (W / 2, H / 2);
    for yy in 0..H {
        for xx in 0..W {
            if f.y[yy * f.fw + xx] != jm[off + yy * W + xx] {
                eprintln!("✗ {tag} frame {k} Y mismatch ({xx},{yy}) [MB ({},{})]: {} vs JM {}", xx / 16, yy / 16, f.y[yy * f.fw + xx], jm[off + yy * W + xx]);
                return false;
            }
        }
    }
    for (plane, base) in [(&f.cb, W * H), (&f.cr, W * H + cw * ch)] {
        for yy in 0..ch {
            for xx in 0..cw {
                if plane[yy * f.cw + xx] != jm[off + base + yy * cw + xx] {
                    eprintln!("✗ {tag} frame {k} C mismatch ({xx},{yy})");
                    return false;
                }
            }
        }
    }
    true
}

fn main() -> anyhow::Result<()> {
    let h264 = std::env::args().nth(1).unwrap();
    let base_truth = std::env::args().nth(2).unwrap();
    let dep_truth = std::env::args().nth(3).unwrap();
    let data = std::fs::read(&h264)?;
    let bjm = std::fs::read(&base_truth)?;
    let djm = std::fs::read(&dep_truth)?;
    let fsz = W * H + 2 * (W / 2) * (H / 2);

    let mut k = 0usize;
    let info = decode_annex_b(&data, |bf, df, _w, _h| {
        let off = k * fsz;
        if !(cmp("base", bf, &bjm, off, k) & cmp("dep", df, &djm, off, k)) {
            anyhow::bail!("mismatch at frame {k}");
        }
        k += 1;
        Ok(())
    })?;

    eprintln!("✓ MVC clip: both views, {}/{} frames decoded bit-exact vs JM (temporal + inter-view)", k, info.frames);
    Ok(())
}
