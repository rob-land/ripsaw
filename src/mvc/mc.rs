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
    for j in 0..bh as i32 {
        for i in 0..bw as i32 {
            let (x, y) = (ox + i, oy + j);
            let v = luma_sample(refp, x, y, fx, fy);
            out[j as usize * bw + i as usize] = v;
        }
    }
    out
}

/// One luma sample at integer base (`x`,`y`) and fractional position
/// (`fx`,`fy`) ∈ {0..3}² (§ 8.4.2.2.1, Figure 8-4 / Table 8-12).
fn luma_sample(p: &Plane, x: i32, y: i32, fx: i32, fy: i32) -> u8 {
    let g = || p.at(x, y);
    let hh = |dx: i32| p.half_h(x, y + dx); // horizontal half in row y+dx (b / s)
    let hv = |dy: i32| p.half_v(x + dy, y); // vertical half in col x+dy (h / m)
    let avg = |a: i32, b: i32| ((a + b + 1) >> 1) as u8;
    match (fx, fy) {
        (0, 0) => g() as u8,
        (1, 0) => avg(g(), hh(0)),
        (2, 0) => hh(0) as u8,
        (3, 0) => avg(p.at(x + 1, y), hh(0)),
        (0, 1) => avg(g(), hv(0)),
        (0, 2) => hv(0) as u8,
        (0, 3) => avg(p.at(x, y + 1), hv(0)),
        (2, 2) => p.center(x, y) as u8,
        (1, 1) => avg(hh(0), hv(0)),
        (3, 1) => avg(hh(0), hv(1)),
        (1, 3) => avg(hh(1), hv(0)),
        (3, 3) => avg(hh(1), hv(1)),
        (2, 1) => avg(hh(0), p.center(x, y)),
        (2, 3) => avg(hh(1), p.center(x, y)),
        (1, 2) => avg(hv(0), p.center(x, y)),
        (3, 2) => avg(hv(1), p.center(x, y)),
        _ => unreachable!(),
    }
}

/// Predict a `bw`×`bh` chroma block (§ 8.4.2.2.2): bilinear over eighth-pel
/// MVs. `mvx`/`mvy` are in chroma eighth-sample units (= luma quarter-pel MV
/// for 4:2:0, where the chroma MV has 1/8 precision).
pub fn mc_chroma(refp: &Plane, bx: i32, by: i32, mvx: i32, mvy: i32, bw: usize, bh: usize) -> Vec<u8> {
    let (fx, fy) = (mvx & 7, mvy & 7);
    let (ox, oy) = (bx + (mvx >> 3), by + (mvy >> 3));
    let mut out = vec![0u8; bw * bh];
    for j in 0..bh as i32 {
        for i in 0..bw as i32 {
            let (x, y) = (ox + i, oy + j);
            let a = refp.at(x, y);
            let b = refp.at(x + 1, y);
            let c = refp.at(x, y + 1);
            let d = refp.at(x + 1, y + 1);
            let v = ((8 - fx) * (8 - fy) * a
                + fx * (8 - fy) * b
                + (8 - fx) * fy * c
                + fx * fy * d
                + 32)
                >> 6;
            out[j as usize * bw + i as usize] = v as u8;
        }
    }
    out
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
