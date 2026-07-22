// Motion compensation — luma quarter-pel + chroma eighth-pel interpolation
// (ITU-T H.264 § 8.4.2.2). The throughput-critical core of inter prediction
// (docs/libmvc-inter.md), shared by base inter decode and the dependent
// (3D) view. Pure functions over a reference plane + a fractional motion
// vector; the decode layer supplies the MV and reference, the layer above
// adds the residual.
//
// Reference samples outside the plane clamp to the border (§ 8.4.2.2.1).

#[inline]
fn clip1(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// A reference luma/chroma plane with border-clamped sampling.
pub struct Plane<'a> {
    pub data: &'a [u8],
    pub w: usize,
    pub h: usize,
}
impl Plane<'_> {
    #[inline]
    fn at(&self, x: i32, y: i32) -> i32 {
        let x = x.clamp(0, self.w as i32 - 1) as usize;
        let y = y.clamp(0, self.h as i32 - 1) as usize;
        self.data[y * self.w + x] as i32
    }
    /// Un-clamped direct sample — valid only when the caller has verified the
    /// coordinate (with its filter taps) is inside the plane (the interior fast
    /// path). No per-sample clamp branch, so the hot loops vectorise.
    #[inline]
    fn raw(&self, x: i32, y: i32) -> i32 {
        self.data[y as usize * self.w + x as usize] as i32
    }
    /// Un-rounded horizontal 6-tap (the `b1` intermediate, § 8.4.2.2.1).
    #[inline]
    fn h6(&self, x: i32, y: i32) -> i32 {
        self.at(x - 2, y) - 5 * self.at(x - 1, y) + 20 * self.at(x, y)
            + 20 * self.at(x + 1, y)
            - 5 * self.at(x + 2, y)
            + self.at(x + 3, y)
    }
    /// Un-rounded vertical 6-tap.
    #[inline]
    fn v6(&self, x: i32, y: i32) -> i32 {
        self.at(x, y - 2) - 5 * self.at(x, y - 1) + 20 * self.at(x, y)
            + 20 * self.at(x, y + 1)
            - 5 * self.at(x, y + 2)
            + self.at(x, y + 3)
    }
    /// Rounded half-pel horizontal (`b`).
    #[inline]
    fn half_h(&self, x: i32, y: i32) -> i32 {
        clip1((self.h6(x, y) + 16) >> 5) as i32
    }
    /// Rounded half-pel vertical (`h`).
    #[inline]
    fn half_v(&self, x: i32, y: i32) -> i32 {
        clip1((self.v6(x, y) + 16) >> 5) as i32
    }
    /// Centre half-pel (`j`): 6-tap of the un-rounded horizontal halves.
    #[inline]
    fn center(&self, x: i32, y: i32) -> i32 {
        let j1 = self.h6(x, y - 2) - 5 * self.h6(x, y - 1) + 20 * self.h6(x, y)
            + 20 * self.h6(x, y + 1)
            - 5 * self.h6(x, y + 2)
            + self.h6(x, y + 3);
        clip1((j1 + 512) >> 10) as i32
    }
}

/// Predict a `bw`×`bh` luma block at destination block origin (`bx`,`by`)
/// using a quarter-pel motion vector (`mvx`,`mvy`, units of ¼ sample).
/// Returns the predicted samples, row-major.
pub fn mc_luma(refp: &Plane, bx: i32, by: i32, mvx: i32, mvy: i32, bw: usize, bh: usize) -> Vec<u8> {
    let (fx, fy) = (mvx & 3, mvy & 3);
    let (ox, oy) = (bx + (mvx >> 2), by + (mvy >> 2));
    let mut out = vec![0u8; bw * bh];
    // Interior blocks (the common case) index the reference directly — no
    // per-sample border clamp — via `raw`. The 6-tap reads columns/rows
    // ox-2 .. ox+bw+2, so require a 2-sample margin on the low side and 3 on
    // the high side. Border blocks keep the clamped `at`.
    let (w, h) = (refp.w as i32, refp.h as i32);
    let interior = ox >= 2 && oy >= 2 && ox + bw as i32 + 3 <= w && oy + bh as i32 + 3 <= h;
    // A 16-wide SIMD read needs a wider right margin than the scalar interior
    // (it always reads a full vector, ox-2..ox+18); when that fits and the case
    // isn't a centre 'j', take the portable-SIMD path (bit-exact vs scalar).
    if !force_scalar() && interior && ox + 19 <= w && portable::handles(fx, fy) {
        portable::luma_block(refp.data, refp.w, ox, oy, fx, fy, bw, bh, &mut out);
        return out;
    }
    if interior {
        luma_block(&|x, y| refp.raw(x, y), ox, oy, fx, fy, bw, bh, &mut out);
    } else {
        luma_block(&|x, y| refp.at(x, y), ox, oy, fx, fy, bw, bh, &mut out);
    }
    out
}

/// Fill a `bw`×`bh` luma block for a fixed fractional position (`fx`,`fy`) ∈
/// {0..3}² (§ 8.4.2.2.1, Figure 8-4 / Table 8-12). `s(x, y)` samples the
/// reference; monomorphised into an un-clamped direct read (interior) or a
/// border-clamped read. The fractional case is chosen ONCE, so each arm is a
/// single tight per-pixel loop that vectorises.
#[inline]
fn luma_block<S: Fn(i32, i32) -> i32>(s: &S, ox: i32, oy: i32, fx: i32, fy: i32, bw: usize, bh: usize, out: &mut [u8]) {
    let h6 = |x: i32, y: i32| s(x - 2, y) - 5 * s(x - 1, y) + 20 * s(x, y) + 20 * s(x + 1, y) - 5 * s(x + 2, y) + s(x + 3, y);
    let v6 = |x: i32, y: i32| s(x, y - 2) - 5 * s(x, y - 1) + 20 * s(x, y) + 20 * s(x, y + 1) - 5 * s(x, y + 2) + s(x, y + 3);
    let hh = |x: i32, y: i32| clip1((h6(x, y) + 16) >> 5) as i32; // horizontal half (b/s)
    let hv = |x: i32, y: i32| clip1((v6(x, y) + 16) >> 5) as i32; // vertical half (h/m)
    let center = |x: i32, y: i32| {
        let j1 = h6(x, y - 2) - 5 * h6(x, y - 1) + 20 * h6(x, y) + 20 * h6(x, y + 1) - 5 * h6(x, y + 2) + h6(x, y + 3);
        clip1((j1 + 512) >> 10) as i32
    };
    let avg = |a: i32, b: i32| ((a + b + 1) >> 1) as u8;
    match (fx, fy) {
        (0, 0) => run_luma(ox, oy, bw, bh, out, |x, y| s(x, y) as u8),
        (1, 0) => run_luma(ox, oy, bw, bh, out, |x, y| avg(s(x, y), hh(x, y))),
        (2, 0) => run_luma(ox, oy, bw, bh, out, |x, y| hh(x, y) as u8),
        (3, 0) => run_luma(ox, oy, bw, bh, out, |x, y| avg(s(x + 1, y), hh(x, y))),
        (0, 1) => run_luma(ox, oy, bw, bh, out, |x, y| avg(s(x, y), hv(x, y))),
        (0, 2) => run_luma(ox, oy, bw, bh, out, |x, y| hv(x, y) as u8),
        (0, 3) => run_luma(ox, oy, bw, bh, out, |x, y| avg(s(x, y + 1), hv(x, y))),
        (2, 2) => run_luma(ox, oy, bw, bh, out, |x, y| center(x, y) as u8),
        (1, 1) => run_luma(ox, oy, bw, bh, out, |x, y| avg(hh(x, y), hv(x, y))),
        (3, 1) => run_luma(ox, oy, bw, bh, out, |x, y| avg(hh(x, y), hv(x + 1, y))),
        (1, 3) => run_luma(ox, oy, bw, bh, out, |x, y| avg(hh(x, y + 1), hv(x, y))),
        (3, 3) => run_luma(ox, oy, bw, bh, out, |x, y| avg(hh(x, y + 1), hv(x + 1, y))),
        (2, 1) => run_luma(ox, oy, bw, bh, out, |x, y| avg(hh(x, y), center(x, y))),
        (2, 3) => run_luma(ox, oy, bw, bh, out, |x, y| avg(hh(x, y + 1), center(x, y))),
        (1, 2) => run_luma(ox, oy, bw, bh, out, |x, y| avg(hv(x, y), center(x, y))),
        (3, 2) => run_luma(ox, oy, bw, bh, out, |x, y| avg(hv(x + 1, y), center(x, y))),
        _ => unreachable!(),
    }
}

/// One tight per-pixel loop for a chosen fractional case: `px(x, y)` is
/// monomorphised + inlined and has no branch, so it vectorises.
#[inline]
fn run_luma<P: Fn(i32, i32) -> u8>(ox: i32, oy: i32, bw: usize, bh: usize, out: &mut [u8], px: P) {
    for j in 0..bh {
        let y = oy + j as i32;
        let row = &mut out[j * bw..j * bw + bw];
        for (i, o) in row.iter_mut().enumerate() {
            *o = px(ox + i as i32, y);
        }
    }
}

/// Predict a `bw`×`bh` chroma block (§ 8.4.2.2.2): bilinear over eighth-pel
/// MVs. `mvx`/`mvy` are in chroma eighth-sample units (= luma quarter-pel MV
/// for 4:2:0, where the chroma MV has 1/8 precision).
pub fn mc_chroma(refp: &Plane, bx: i32, by: i32, mvx: i32, mvy: i32, bw: usize, bh: usize) -> Vec<u8> {
    let (fx, fy) = (mvx & 7, mvy & 7);
    let (ox, oy) = (bx + (mvx >> 3), by + (mvy >> 3));
    let mut out = vec![0u8; bw * bh];
    // Bilinear reads (x,y)..(x+1,y+1); interior needs a 1-sample high margin.
    let (w, h) = (refp.w as i32, refp.h as i32);
    let interior = ox >= 0 && oy >= 0 && ox + bw as i32 + 1 <= w && oy + bh as i32 + 1 <= h;
    if interior {
        chroma_block(&|x, y| refp.raw(x, y), ox, oy, fx, fy, bw, bh, &mut out);
    } else {
        chroma_block(&|x, y| refp.at(x, y), ox, oy, fx, fy, bw, bh, &mut out);
    }
    out
}

/// Fill a `bw`×`bh` chroma block: bilinear over eighth-pel positions with the
/// four weights hoisted out of the loop. `s` is the interior (direct) or border
/// (clamped) sampler.
#[inline]
fn chroma_block<S: Fn(i32, i32) -> i32>(s: &S, ox: i32, oy: i32, fx: i32, fy: i32, bw: usize, bh: usize, out: &mut [u8]) {
    let (w00, w10, w01, w11) = ((8 - fx) * (8 - fy), fx * (8 - fy), (8 - fx) * fy, fx * fy);
    for j in 0..bh {
        let y = oy + j as i32;
        let row = &mut out[j * bw..j * bw + bw];
        for i in 0..bw {
            let x = ox + i as i32;
            row[i] = ((w00 * s(x, y) + w10 * s(x + 1, y) + w01 * s(x, y + 1) + w11 * s(x + 1, y + 1) + 32) >> 6) as u8;
        }
    }
}

// ---- portable-SIMD luma interpolation (via the `wide` crate) ----
//
// Bit-exact SIMD of the scalar `luma_block` for the 11 non-centre fractional
// cases (the ones built only from the horizontal/vertical 6-tap + averaging).
// Written against portable vector types (`wide::i16x16`) that lower to AVX2 on
// x86-64 and NEON on aarch64 from one source — no `unsafe`, no per-arch code.
// The 6-tap output fits in i16, so a row of 16 is filtered in one 256-bit
// register; `clip` (max/min to 0..255) reproduces `clip1` and `avg` reproduces
// `(a+b+1)>>1` exactly. The 5 centre ('j') cases need i32 intermediates and stay
// on the scalar path, as do border/edge blocks.
mod portable {
    use wide::{i16x16, u8x16};

    /// True for the fractional cases this module handles (everything except the
    /// centre 'j' positions, which need i32 precision).
    #[inline]
    pub fn handles(fx: i32, fy: i32) -> bool {
        !matches!((fx, fy), (2, 2) | (2, 1) | (2, 3) | (1, 2) | (3, 2))
    }

    #[inline]
    fn ld(data: &[u8], w: usize, x: i32, y: i32) -> i16x16 {
        let off = y as usize * w + x as usize;
        let bytes: [u8; 16] = data[off..off + 16].try_into().unwrap();
        i16x16::from(u8x16::new(bytes))
    }
    #[inline]
    fn h6(data: &[u8], w: usize, x: i32, y: i32) -> i16x16 {
        let a = ld(data, w, x - 2, y) + ld(data, w, x + 3, y);
        let b = ld(data, w, x, y) + ld(data, w, x + 1, y);
        let c = ld(data, w, x - 1, y) + ld(data, w, x + 2, y);
        a + b * 20 - c * 5
    }
    #[inline]
    fn v6(data: &[u8], w: usize, x: i32, y: i32) -> i16x16 {
        let a = ld(data, w, x, y - 2) + ld(data, w, x, y + 3);
        let b = ld(data, w, x, y) + ld(data, w, x, y + 1);
        let c = ld(data, w, x, y - 1) + ld(data, w, x, y + 2);
        a + b * 20 - c * 5
    }
    #[inline]
    fn half(v: i16x16) -> i16x16 {
        (v + 16) >> 5u32
    }
    #[inline]
    fn clip(v: i16x16) -> i16x16 {
        v.max(i16x16::new([0; 16])).min(i16x16::new([255; 16]))
    }
    #[inline]
    fn avg(a: i16x16, b: i16x16) -> i16x16 {
        (a + b + 1) >> 1u32
    }
    #[inline]
    fn store(v: i16x16, row: &mut [u8], bw: usize) {
        let a = clip(v).to_array();
        for k in 0..bw {
            row[k] = a[k] as u8;
        }
    }

    pub fn luma_block(data: &[u8], w: usize, ox: i32, oy: i32, fx: i32, fy: i32, bw: usize, bh: usize, out: &mut [u8]) {
        for j in 0..bh {
            let y = oy + j as i32;
            let r = match (fx, fy) {
                (0, 0) => ld(data, w, ox, y),
                (2, 0) => half(h6(data, w, ox, y)),
                (0, 2) => half(v6(data, w, ox, y)),
                (1, 0) => avg(ld(data, w, ox, y), clip(half(h6(data, w, ox, y)))),
                (3, 0) => avg(ld(data, w, ox + 1, y), clip(half(h6(data, w, ox, y)))),
                (0, 1) => avg(ld(data, w, ox, y), clip(half(v6(data, w, ox, y)))),
                (0, 3) => avg(ld(data, w, ox, y + 1), clip(half(v6(data, w, ox, y)))),
                (1, 1) => avg(clip(half(h6(data, w, ox, y))), clip(half(v6(data, w, ox, y)))),
                (3, 1) => avg(clip(half(h6(data, w, ox, y))), clip(half(v6(data, w, ox + 1, y)))),
                (1, 3) => avg(clip(half(h6(data, w, ox, y + 1))), clip(half(v6(data, w, ox, y)))),
                (3, 3) => avg(clip(half(h6(data, w, ox, y + 1))), clip(half(v6(data, w, ox + 1, y)))),
                _ => unreachable!(),
            };
            store(r, &mut out[j * bw..j * bw + bw], bw);
        }
    }
}

/// Force the scalar luma path (skip portable SIMD) — set `RIPSAW_MC=scalar`.
/// Kept as a debugging/validation escape hatch; the SIMD path is bit-exact.
fn force_scalar() -> bool {
    use std::sync::OnceLock;
    static M: OnceLock<bool> = OnceLock::new();
    *M.get_or_init(|| std::env::var("RIPSAW_MC").as_deref() == Ok("scalar"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp_plane(w: usize, h: usize) -> Vec<u8> {
        (0..w * h).map(|i| ((i * 7) % 256) as u8).collect()
    }

    #[test]
    fn integer_mv_is_a_copy() {
        let (w, h) = (16, 16);
        let d = ramp_plane(w, h);
        let p = Plane { data: &d, w, h };
        // MV (8, -4) quarter-pel = (2, -1) integer, frac (0,0).
        let blk = mc_luma(&p, 4, 4, 8, -4, 4, 4);
        for j in 0..4 {
            for i in 0..4 {
                assert_eq!(blk[j * 4 + i], p.at(4 + 2 + i as i32, 4 - 1 + j as i32) as u8);
            }
        }
    }

    #[test]
    fn half_pel_horizontal_is_6tap() {
        let (w, h) = (16, 16);
        let d = ramp_plane(w, h);
        let p = Plane { data: &d, w, h };
        // frac (2,0): pure horizontal half-pel.
        let blk = mc_luma(&p, 5, 5, 2, 0, 2, 2);
        for j in 0..2i32 {
            for i in 0..2i32 {
                let x = 5 + i;
                let y = 5 + j;
                let expect = clip1((p.h6(x, y) + 16) >> 5);
                assert_eq!(blk[(j * 2 + i) as usize], expect);
            }
        }
    }

    #[test]
    fn quarter_pel_averages_integer_and_half() {
        let (w, h) = (16, 16);
        let d = ramp_plane(w, h);
        let p = Plane { data: &d, w, h };
        // frac (1,0): average of G and b.
        let blk = mc_luma(&p, 5, 5, 1, 0, 1, 1);
        let g = p.at(5, 5);
        let b = clip1((p.h6(5, 5) + 16) >> 5) as i32;
        assert_eq!(blk[0], ((g + b + 1) >> 1) as u8);
    }

    #[test]
    fn chroma_integer_is_copy() {
        let (w, h) = (8, 8);
        let d = ramp_plane(w, h);
        let p = Plane { data: &d, w, h };
        // frac 0: integer position copy.
        let blk = mc_chroma(&p, 1, 1, 8, 8, 2, 2); // (8,8)/8 = (1,1) integer
        assert_eq!(blk[0], p.at(2, 2) as u8);
    }

    #[test]
    fn border_clamps() {
        let (w, h) = (4, 4);
        let d = ramp_plane(w, h);
        let p = Plane { data: &d, w, h };
        // A MV pointing off the top-left clamps to (0,0).
        assert_eq!(p.at(-10, -10), p.at(0, 0));
        let blk = mc_luma(&p, 0, 0, -40, -40, 2, 2); // way off-frame
        assert_eq!(blk[0], p.at(0, 0) as u8);
    }
}
