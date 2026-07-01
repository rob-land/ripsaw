// Intra prediction (ITU-T H.264 § 8.3). Third building block of the
// libmvc decode core (docs/libmvc-poc.md): forms the spatial prediction a
// macroblock's residual is added to.
//
// This module covers Intra_4x4 luma (the 9 modes, § 8.3.1.2). Intra_16x16
// and chroma prediction follow. Predictions are computed in i32 and left
// unclipped; the caller adds the residual and clips to [0, 255].
//
// Neighbour samples are addressed spec-style: a "top" line `t(-1..=7)`
// where `t(-1)` is the top-left corner and `t(0..=7)` are the row above
// (including the 4 above-right samples), and a "left" column `l(-1..=3)`
// where `l(-1)` is the same corner. Availability is the caller's concern;
// for unavailable neighbours it passes the substituted values the spec's
// reference-sample process (§ 8.3.1.2.1) would have produced.

/// The reconstructed neighbour samples around a 4×4 luma block.
#[derive(Debug, Clone, Copy)]
pub struct Neighbors4x4 {
    /// `p[x][-1]` for x = 0..=7 (row above + above-right).
    pub top: [i32; 8],
    /// `p[-1][y]` for y = 0..=3 (column to the left).
    pub left: [i32; 4],
    /// `p[-1][-1]` (top-left corner).
    pub corner: i32,
    pub top_avail: bool,
    pub left_avail: bool,
}

impl Neighbors4x4 {
    #[inline]
    fn t(&self, i: i32) -> i32 {
        if i < 0 {
            self.corner
        } else {
            self.top[i as usize]
        }
    }
    #[inline]
    fn l(&self, j: i32) -> i32 {
        if j < 0 {
            self.corner
        } else {
            self.left[j as usize]
        }
    }
}

/// The nine Intra_4x4 prediction modes (§ 8.3.1.2), in their numeric
/// order. Directional modes require the relevant neighbours to be
/// available (the caller guarantees this via mode availability, § 8.3.1.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intra4x4Mode {
    Vertical,        // 0
    Horizontal,      // 1
    Dc,              // 2
    DiagDownLeft,    // 3
    DiagDownRight,   // 4
    VerticalRight,   // 5
    HorizontalDown,  // 6
    VerticalLeft,    // 7
    HorizontalUp,    // 8
}

impl Intra4x4Mode {
    pub fn from_index(i: u32) -> Option<Self> {
        use Intra4x4Mode::*;
        Some(match i {
            0 => Vertical,
            1 => Horizontal,
            2 => Dc,
            3 => DiagDownLeft,
            4 => DiagDownRight,
            5 => VerticalRight,
            6 => HorizontalDown,
            7 => VerticalLeft,
            8 => HorizontalUp,
            _ => return None,
        })
    }
}

#[inline]
fn avg2(a: i32, b: i32) -> i32 {
    (a + b + 1) >> 1
}
#[inline]
fn avg3(a: i32, b: i32, c: i32) -> i32 {
    (a + 2 * b + c + 2) >> 2
}

/// Predict a 4×4 luma block. Output `pred[y][x]` (row-major), unclipped.
pub fn predict_4x4(mode: Intra4x4Mode, n: &Neighbors4x4) -> [[i32; 4]; 4] {
    use Intra4x4Mode::*;
    let mut p = [[0i32; 4]; 4];
    match mode {
        Vertical => {
            for y in 0..4 {
                for x in 0..4 {
                    p[y][x] = n.t(x as i32);
                }
            }
        }
        Horizontal => {
            for y in 0..4 {
                for x in 0..4 {
                    p[y][x] = n.l(y as i32);
                }
            }
        }
        Dc => {
            let dc = match (n.top_avail, n.left_avail) {
                (true, true) => {
                    ((0..4).map(|i| n.top[i]).sum::<i32>()
                        + (0..4).map(|j| n.left[j]).sum::<i32>()
                        + 4)
                        >> 3
                }
                (true, false) => ((0..4).map(|i| n.top[i]).sum::<i32>() + 2) >> 2,
                (false, true) => ((0..4).map(|j| n.left[j]).sum::<i32>() + 2) >> 2,
                (false, false) => 128, // 1 << (BitDepth - 1), 8-bit
            };
            p = [[dc; 4]; 4];
        }
        DiagDownLeft => {
            for y in 0..4i32 {
                for x in 0..4i32 {
                    p[y as usize][x as usize] = if x == 3 && y == 3 {
                        avg3(n.t(6), n.t(7), n.t(7))
                    } else {
                        avg3(n.t(x + y), n.t(x + y + 1), n.t(x + y + 2))
                    };
                }
            }
        }
        DiagDownRight => {
            for y in 0..4i32 {
                for x in 0..4i32 {
                    p[y as usize][x as usize] = match x.cmp(&y) {
                        std::cmp::Ordering::Greater => {
                            avg3(n.t(x - y - 2), n.t(x - y - 1), n.t(x - y))
                        }
                        std::cmp::Ordering::Less => {
                            avg3(n.l(y - x - 2), n.l(y - x - 1), n.l(y - x))
                        }
                        std::cmp::Ordering::Equal => avg3(n.t(0), n.t(-1), n.l(0)),
                    };
                }
            }
        }
        VerticalRight => {
            for y in 0..4i32 {
                for x in 0..4i32 {
                    let z = 2 * x - y;
                    p[y as usize][x as usize] = if z >= 0 && z % 2 == 0 {
                        avg2(n.t(x - (y >> 1) - 1), n.t(x - (y >> 1)))
                    } else if z >= 0 {
                        avg3(n.t(x - (y >> 1) - 2), n.t(x - (y >> 1) - 1), n.t(x - (y >> 1)))
                    } else if z == -1 {
                        avg3(n.l(0), n.t(-1), n.t(0))
                    } else {
                        avg3(n.l(y - 1), n.l(y - 2), n.l(y - 3))
                    };
                }
            }
        }
        HorizontalDown => {
            for y in 0..4i32 {
                for x in 0..4i32 {
                    let z = 2 * y - x;
                    p[y as usize][x as usize] = if z >= 0 && z % 2 == 0 {
                        avg2(n.l(y - (x >> 1) - 1), n.l(y - (x >> 1)))
                    } else if z >= 0 {
                        avg3(n.l(y - (x >> 1) - 2), n.l(y - (x >> 1) - 1), n.l(y - (x >> 1)))
                    } else if z == -1 {
                        avg3(n.l(0), n.t(-1), n.t(0))
                    } else {
                        avg3(n.t(x - 1), n.t(x - 2), n.t(x - 3))
                    };
                }
            }
        }
        VerticalLeft => {
            for y in 0..4i32 {
                for x in 0..4i32 {
                    p[y as usize][x as usize] = if y % 2 == 0 {
                        avg2(n.t(x + (y >> 1)), n.t(x + (y >> 1) + 1))
                    } else {
                        avg3(n.t(x + (y >> 1)), n.t(x + (y >> 1) + 1), n.t(x + (y >> 1) + 2))
                    };
                }
            }
        }
        HorizontalUp => {
            for y in 0..4i32 {
                for x in 0..4i32 {
                    let z = x + 2 * y;
                    p[y as usize][x as usize] = if z < 5 && z % 2 == 0 {
                        avg2(n.l(y + (x >> 1)), n.l(y + (x >> 1) + 1))
                    } else if z < 5 {
                        avg3(n.l(y + (x >> 1)), n.l(y + (x >> 1) + 1), n.l(y + (x >> 1) + 2))
                    } else if z == 5 {
                        avg3(n.l(2), n.l(3), n.l(3))
                    } else {
                        n.l(3)
                    };
                }
            }
        }
    }
    p
}

/// Derive a block's actual Intra_4x4 / Intra_8x8 prediction mode from its
/// neighbours and the decoded syntax (§ 8.3.1.1 / § 8.3.2.1, identical
/// algorithm). `mode_a` / `mode_b` are the left / above neighbour block's
/// modes, or `None` when that neighbour is unavailable or not coded as an
/// Intra_NxN block — in which case `dcPredModePredictedFlag` is set and the
/// predicted mode is DC (2). `raw` is the decoded value:
/// −1 for `prev_intra4x4_pred_mode_flag = 1` (use the predicted mode), or
/// 0..=7 for `rem_intra4x4_pred_mode`.
pub fn derive_intra_mode(mode_a: Option<u8>, mode_b: Option<u8>, raw: i64) -> u8 {
    let pred = match (mode_a, mode_b) {
        (Some(a), Some(b)) => a.min(b),
        _ => 2, // dcPredModePredictedFlag -> DC
    };
    if raw < 0 {
        pred
    } else {
        let rem = raw as u8;
        if rem < pred {
            rem
        } else {
            rem + 1
        }
    }
}

/// Reference samples for an Intra_8x8 luma block (§ 8.3.2.2). `top` holds
/// p[0..15][-1] (the row above plus above-right; the caller replicates
/// p[7][-1] into indices 8..16 when the above-right block is unavailable),
/// `left` holds p[-1][0..7], `corner` is p[-1][-1]. Intra_8x8 uses the same
/// nine modes as 4×4, applied to *low-pass-filtered* reference samples.
pub struct Neighbors8x8 {
    pub top: [i32; 16],
    pub left: [i32; 8],
    pub corner: i32,
    pub top_avail: bool,
    pub left_avail: bool,
    pub corner_avail: bool,
}

impl Neighbors8x8 {
    #[inline]
    fn t(&self, i: i32) -> i32 {
        if i < 0 {
            self.corner
        } else {
            self.top[i as usize]
        }
    }
    #[inline]
    fn l(&self, j: i32) -> i32 {
        if j < 0 {
            self.corner
        } else {
            self.left[j as usize]
        }
    }

    /// Apply the Intra_8x8 reference-sample low-pass filter (§ 8.3.2.2.1,
    /// JM `LowPassForIntra8x8Pred`): a [1 2 1]/4 filter over the available
    /// reference samples, replicating at the two far ends.
    fn filtered(&self) -> Neighbors8x8 {
        let (up, lf, ul) = (self.top_avail, self.left_avail, self.corner_avail);
        let mut top = self.top;
        let mut left = self.left;
        let mut corner = self.corner;
        if ul {
            corner = if up && lf {
                (self.left[0] + 2 * self.corner + self.top[0] + 2) >> 2
            } else if up {
                (3 * self.corner + self.top[0] + 2) >> 2
            } else if lf {
                (3 * self.corner + self.left[0] + 2) >> 2
            } else {
                self.corner
            };
        }
        if up {
            top[0] = if ul {
                (self.corner + 2 * self.top[0] + self.top[1] + 2) >> 2
            } else {
                (3 * self.top[0] + self.top[1] + 2) >> 2
            };
            for i in 1..15 {
                top[i] = (self.top[i - 1] + 2 * self.top[i] + self.top[i + 1] + 2) >> 2;
            }
            top[15] = (self.top[14] + 3 * self.top[15] + 2) >> 2;
        }
        if lf {
            left[0] = if ul {
                (self.corner + 2 * self.left[0] + self.left[1] + 2) >> 2
            } else {
                (3 * self.left[0] + self.left[1] + 2) >> 2
            };
            for j in 1..7 {
                left[j] = (self.left[j - 1] + 2 * self.left[j] + self.left[j + 1] + 2) >> 2;
            }
            left[7] = (self.left[6] + 3 * self.left[7] + 2) >> 2;
        }
        Neighbors8x8 { top, left, corner, top_avail: up, left_avail: lf, corner_avail: ul }
    }
}

/// Predict an 8×8 luma block (§ 8.3.2.2), reusing the nine Intra_4x4 mode
/// directions. Reference samples are low-pass filtered first (the defining
/// difference from 4×4). Output `pred[y][x]` (row-major), unclipped.
pub fn predict_8x8(mode: Intra4x4Mode, raw: &Neighbors8x8) -> [[i32; 8]; 8] {
    use Intra4x4Mode::*;
    let n = raw.filtered();
    let mut p = [[0i32; 8]; 8];
    match mode {
        Vertical => {
            for y in 0..8 {
                for x in 0..8 {
                    p[y][x] = n.t(x as i32);
                }
            }
        }
        Horizontal => {
            for y in 0..8 {
                for x in 0..8 {
                    p[y][x] = n.l(y as i32);
                }
            }
        }
        Dc => {
            let dc = match (n.top_avail, n.left_avail) {
                (true, true) => {
                    ((0..8).map(|i| n.top[i]).sum::<i32>() + (0..8).map(|j| n.left[j]).sum::<i32>() + 8) >> 4
                }
                (true, false) => ((0..8).map(|i| n.top[i]).sum::<i32>() + 4) >> 3,
                (false, true) => ((0..8).map(|j| n.left[j]).sum::<i32>() + 4) >> 3,
                (false, false) => 128,
            };
            for row in &mut p {
                row.fill(dc);
            }
        }
        DiagDownLeft => {
            for y in 0..8i32 {
                for x in 0..8i32 {
                    p[y as usize][x as usize] = if x == 7 && y == 7 {
                        avg3(n.t(14), n.t(15), n.t(15))
                    } else {
                        avg3(n.t(x + y), n.t(x + y + 1), n.t(x + y + 2))
                    };
                }
            }
        }
        DiagDownRight => {
            for y in 0..8i32 {
                for x in 0..8i32 {
                    p[y as usize][x as usize] = match x.cmp(&y) {
                        std::cmp::Ordering::Greater => avg3(n.t(x - y - 2), n.t(x - y - 1), n.t(x - y)),
                        std::cmp::Ordering::Less => avg3(n.l(y - x - 2), n.l(y - x - 1), n.l(y - x)),
                        std::cmp::Ordering::Equal => avg3(n.t(0), n.t(-1), n.l(0)),
                    };
                }
            }
        }
        VerticalRight => {
            for y in 0..8i32 {
                for x in 0..8i32 {
                    let z = 2 * x - y;
                    p[y as usize][x as usize] = if z >= 0 && z % 2 == 0 {
                        avg2(n.t(x - (y >> 1) - 1), n.t(x - (y >> 1)))
                    } else if z >= 0 {
                        avg3(n.t(x - (y >> 1) - 2), n.t(x - (y >> 1) - 1), n.t(x - (y >> 1)))
                    } else if z == -1 {
                        avg3(n.l(0), n.t(-1), n.t(0))
                    } else {
                        // zVR < -1 (§ 8.3.2.2.5): left samples at y - 2x - k
                        // (NOT the 4×4 y-k — 8×8's x reaches this region).
                        avg3(n.l(y - 2 * x - 1), n.l(y - 2 * x - 2), n.l(y - 2 * x - 3))
                    };
                }
            }
        }
        HorizontalDown => {
            for y in 0..8i32 {
                for x in 0..8i32 {
                    let z = 2 * y - x;
                    p[y as usize][x as usize] = if z >= 0 && z % 2 == 0 {
                        avg2(n.l(y - (x >> 1) - 1), n.l(y - (x >> 1)))
                    } else if z >= 0 {
                        avg3(n.l(y - (x >> 1) - 2), n.l(y - (x >> 1) - 1), n.l(y - (x >> 1)))
                    } else if z == -1 {
                        avg3(n.l(0), n.t(-1), n.t(0))
                    } else {
                        // zHD < -1 (§ 8.3.2.2.6): top samples at x - 2y - k.
                        avg3(n.t(x - 2 * y - 1), n.t(x - 2 * y - 2), n.t(x - 2 * y - 3))
                    };
                }
            }
        }
        VerticalLeft => {
            for y in 0..8i32 {
                for x in 0..8i32 {
                    p[y as usize][x as usize] = if y % 2 == 0 {
                        avg2(n.t(x + (y >> 1)), n.t(x + (y >> 1) + 1))
                    } else {
                        avg3(n.t(x + (y >> 1)), n.t(x + (y >> 1) + 1), n.t(x + (y >> 1) + 2))
                    };
                }
            }
        }
        HorizontalUp => {
            for y in 0..8i32 {
                for x in 0..8i32 {
                    let z = x + 2 * y;
                    p[y as usize][x as usize] = if z < 13 && z % 2 == 0 {
                        avg2(n.l(y + (x >> 1)), n.l(y + (x >> 1) + 1))
                    } else if z < 13 {
                        avg3(n.l(y + (x >> 1)), n.l(y + (x >> 1) + 1), n.l(y + (x >> 1) + 2))
                    } else if z == 13 {
                        avg3(n.l(6), n.l(7), n.l(7))
                    } else {
                        n.l(7)
                    };
                }
            }
        }
    }
    p
}

#[inline]
fn clip1(v: i32) -> i32 {
    v.clamp(0, 255)
}

/// The four whole-macroblock prediction modes shared by Intra_16x16 luma
/// (§ 8.3.3) and chroma (§ 8.3.4): Vertical, Horizontal, DC, Plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaneMode {
    Vertical,   // 0
    Horizontal, // 1
    Dc,         // 2
    Plane,      // 3
}

impl PlaneMode {
    pub fn from_index(i: u32) -> Option<Self> {
        Some(match i {
            0 => PlaneMode::Vertical,
            1 => PlaneMode::Horizontal,
            2 => PlaneMode::Dc,
            3 => PlaneMode::Plane,
            _ => return None,
        })
    }
}

/// Neighbours for an N×N whole-block predictor (N = 16 luma, 8 chroma).
/// `top`/`left` are the N samples above/left; `corner` is `p[-1][-1]`.
pub struct NeighborsNxN {
    /// N samples above / left (N = 16 luma, 8 chroma; unused tail is 0).
    pub top: [i32; 16],
    pub left: [i32; 16],
    pub corner: i32,
    pub top_avail: bool,
    pub left_avail: bool,
}

/// Predict an Intra_16x16 luma macroblock (§ 8.3.3). `out[y][x]`, clipped.
pub fn predict_16x16(mode: PlaneMode, n: &NeighborsNxN) -> [[i32; 16]; 16] {
    let mut p = [[0i32; 16]; 16];
    match mode {
        PlaneMode::Vertical => {
            for y in 0..16 {
                for x in 0..16 {
                    p[y][x] = n.top[x];
                }
            }
        }
        PlaneMode::Horizontal => {
            for y in 0..16 {
                for x in 0..16 {
                    p[y][x] = n.left[y];
                }
            }
        }
        PlaneMode::Dc => {
            let dc = dc_value(n, 16);
            p = [[dc; 16]; 16];
        }
        PlaneMode::Plane => {
            let mut h = 0;
            for xp in 0..8i32 {
                h += (xp + 1) * (n.top[(8 + xp) as usize] - n.top_or_corner(6 - xp));
            }
            let mut v = 0;
            for yp in 0..8i32 {
                v += (yp + 1) * (n.left[(8 + yp) as usize] - n.left_or_corner(6 - yp));
            }
            let b = (5 * h + 32) >> 6;
            let c = (5 * v + 32) >> 6;
            let a = 16 * (n.left[15] + n.top[15]);
            for y in 0..16i32 {
                for x in 0..16i32 {
                    p[y as usize][x as usize] =
                        clip1((a + b * (x - 7) + c * (y - 7) + 16) >> 5);
                }
            }
        }
    }
    p
}

/// Predict an 8×8 chroma block for 4:2:0 (§ 8.3.4). `out[y][x]`, clipped.
pub fn predict_chroma_8x8(mode: PlaneMode, n: &NeighborsNxN) -> [[i32; 8]; 8] {
    let mut p = [[0i32; 8]; 8];
    match mode {
        PlaneMode::Vertical => {
            for y in 0..8 {
                for x in 0..8 {
                    p[y][x] = n.top[x];
                }
            }
        }
        PlaneMode::Horizontal => {
            for y in 0..8 {
                for x in 0..8 {
                    p[y][x] = n.left[y];
                }
            }
        }
        PlaneMode::Dc => chroma_dc(n, &mut p),
        PlaneMode::Plane => {
            let mut h = 0;
            for xp in 0..4i32 {
                h += (xp + 1) * (n.top[(4 + xp) as usize] - n.top_or_corner(2 - xp));
            }
            let mut v = 0;
            for yp in 0..4i32 {
                v += (yp + 1) * (n.left[(4 + yp) as usize] - n.left_or_corner(2 - yp));
            }
            let b = (17 * h + 16) >> 5;
            let c = (17 * v + 16) >> 5;
            let a = 16 * (n.left[7] + n.top[7]);
            for y in 0..8i32 {
                for x in 0..8i32 {
                    p[y as usize][x as usize] =
                        clip1((a + b * (x - 3) + c * (y - 3) + 16) >> 5);
                }
            }
        }
    }
    p
}

impl NeighborsNxN {
    #[inline]
    fn top_or_corner(&self, i: i32) -> i32 {
        if i < 0 {
            self.corner
        } else {
            self.top[i as usize]
        }
    }
    #[inline]
    fn left_or_corner(&self, j: i32) -> i32 {
        if j < 0 {
            self.corner
        } else {
            self.left[j as usize]
        }
    }
}

fn dc_value(n: &NeighborsNxN, size: usize) -> i32 {
    let sum_top: i32 = n.top[..size].iter().sum();
    let sum_left: i32 = n.left[..size].iter().sum();
    let log2 = size.trailing_zeros() as i32; // 4 for 16, 3 for 8
    match (n.top_avail, n.left_avail) {
        (true, true) => (sum_top + sum_left + size as i32) >> (log2 + 1),
        (true, false) => (sum_top + (size as i32 >> 1)) >> log2,
        (false, true) => (sum_left + (size as i32 >> 1)) >> log2,
        (false, false) => 128,
    }
}

/// 4:2:0 chroma DC (§ 8.3.4.1): the 8×8 splits into four 4×4 blocks, each
/// with its own neighbour preference — the diagonal blocks use both
/// top+left, the top-right prefers top, the bottom-left prefers left.
fn chroma_dc(n: &NeighborsNxN, out: &mut [[i32; 8]; 8]) {
    let sum4 = |s: &[i32]| -> i32 { s.iter().sum() };
    let both = |t: &[i32], l: &[i32]| (sum4(t) + sum4(l) + 4) >> 3;
    let one = |s: &[i32]| (sum4(s) + 2) >> 2;
    let (ta, la) = (n.top_avail, n.left_avail);
    let top = n.top;
    let left = n.left;

    // (block x-origin, y-origin) -> dc.
    let dc = |bx: usize, by: usize| -> i32 {
        let t = &top[bx..bx + 4];
        let l = &left[by..by + 4];
        match (bx, by) {
            // Top-left and bottom-right: both neighbours.
            (0, 0) | (4, 4) => match (ta, la) {
                (true, true) => both(t, l),
                (true, false) => one(t),
                (false, true) => one(l),
                (false, false) => 128,
            },
            // Top-right: prefer top.
            (4, 0) => {
                if ta {
                    one(t)
                } else if la {
                    one(l)
                } else {
                    128
                }
            }
            // Bottom-left: prefer left.
            (0, 4) => {
                if la {
                    one(l)
                } else if ta {
                    one(t)
                } else {
                    128
                }
            }
            _ => unreachable!(),
        }
    };

    for &(bx, by) in &[(0, 0), (4, 0), (0, 4), (4, 4)] {
        let v = dc(bx, by);
        for y in by..by + 4 {
            for x in bx..bx + 4 {
                out[y][x] = v;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn neigh(top: [i32; 8], left: [i32; 4], corner: i32) -> Neighbors4x4 {
        Neighbors4x4 { top, left, corner, top_avail: true, left_avail: true }
    }

    #[test]
    fn vertical_copies_top_row_down() {
        let n = neigh([10, 20, 30, 40, 0, 0, 0, 0], [1, 2, 3, 4], 5);
        let p = predict_4x4(Intra4x4Mode::Vertical, &n);
        for y in 0..4 {
            assert_eq!(p[y], [10, 20, 30, 40]);
        }
    }

    #[test]
    fn horizontal_copies_left_col_across() {
        let n = neigh([0; 8], [11, 22, 33, 44], 5);
        let p = predict_4x4(Intra4x4Mode::Horizontal, &n);
        for y in 0..4 {
            assert_eq!(p[y], [[11, 22, 33, 44][y]; 4]);
        }
    }

    #[test]
    fn dc_averages_per_availability() {
        // Both available: (sum top 100 + sum left 20 + 4) >> 3.
        let n = neigh([25, 25, 25, 25, 0, 0, 0, 0], [5, 5, 5, 5], 0);
        let p = predict_4x4(Intra4x4Mode::Dc, &n);
        assert_eq!(p[0][0], (100 + 20 + 4) >> 3); // 15
        // Top only.
        let mut n2 = n;
        n2.left_avail = false;
        assert_eq!(predict_4x4(Intra4x4Mode::Dc, &n2)[0][0], (100 + 2) >> 2); // 25
        // Neither -> mid-grey 128.
        let n3 = Neighbors4x4 { top_avail: false, left_avail: false, ..n };
        assert_eq!(predict_4x4(Intra4x4Mode::Dc, &n3)[0][0], 128);
    }

    #[test]
    fn diag_down_left_on_constant_top_is_constant() {
        // Constant top line -> avg3 of equal values is that value
        // everywhere (the bottom-right special case also averages equals).
        let n = neigh([50; 8], [0; 4], 0);
        let p = predict_4x4(Intra4x4Mode::DiagDownLeft, &n);
        assert_eq!(p, [[50; 4]; 4]);
    }

    #[test]
    fn diag_down_right_constant_field_is_constant() {
        // All neighbours (top, left, corner) equal -> every avg3 yields it.
        let n = neigh([7; 8], [7; 4], 7);
        let p = predict_4x4(Intra4x4Mode::DiagDownRight, &n);
        assert_eq!(p, [[7; 4]; 4]);
    }

    #[test]
    fn directional_modes_preserve_a_constant_field() {
        // For a uniform neighbourhood, every directional predictor must
        // reproduce the constant — a strong structural check across all
        // the index arithmetic without hand-deriving each tap.
        use Intra4x4Mode::*;
        let n = neigh([9; 8], [9; 4], 9);
        for m in [
            DiagDownLeft,
            DiagDownRight,
            VerticalRight,
            HorizontalDown,
            VerticalLeft,
            HorizontalUp,
        ] {
            assert_eq!(predict_4x4(m, &n), [[9; 4]; 4], "mode {m:?}");
        }
    }

    #[test]
    fn vertical_left_known_taps() {
        // y=0 row uses avg2 of consecutive top samples:
        //   x=0: avg2(t0,t1); x=1: avg2(t1,t2); ...
        let n = neigh([0, 4, 8, 12, 16, 20, 24, 28], [0; 4], 0);
        let p = predict_4x4(Intra4x4Mode::VerticalLeft, &n);
        assert_eq!(p[0], [avg2(0, 4), avg2(4, 8), avg2(8, 12), avg2(12, 16)]);
    }

    fn arr16(v: &[i32]) -> [i32; 16] {
        let mut a = [0i32; 16];
        a[..v.len()].copy_from_slice(v);
        a
    }
    fn nxn(top: Vec<i32>, left: Vec<i32>, corner: i32) -> ([i32; 16], [i32; 16], i32) {
        (arr16(&top), arr16(&left), corner)
    }

    #[test]
    fn pred16_vertical_horizontal_dc() {
        let (t, l, c) = nxn(vec![3; 16], vec![7; 16], 0);
        let n = NeighborsNxN { top: t, left: l, corner: c, top_avail: true, left_avail: true };
        assert_eq!(predict_16x16(PlaneMode::Vertical, &n)[5], [3; 16]);
        assert_eq!(predict_16x16(PlaneMode::Horizontal, &n)[5], [7; 16]);
        // DC both: (16*3 + 16*7 + 16) >> 5 = (48+112+16)/32 = 5.
        assert_eq!(predict_16x16(PlaneMode::Dc, &n)[0][0], 5);
    }

    #[test]
    fn pred16_plane_constant_field_is_constant() {
        // Uniform neighbourhood -> H=V=0, a=32p, pred=(32p+16)>>5 = p.
        let (t, l, c) = nxn(vec![100; 16], vec![100; 16], 100);
        let n = NeighborsNxN { top: t, left: l, corner: c, top_avail: true, left_avail: true };
        assert_eq!(predict_16x16(PlaneMode::Plane, &n), [[100; 16]; 16]);
    }

    #[test]
    fn chroma_dc_per_block_preference() {
        // top = 0..8 scaled, left = constant; check the four 4×4 DCs pick
        // the right neighbours. top[0..4]=10 each, top[4..8]=20 each;
        // left all = 40.
        let n = NeighborsNxN { top: arr16(&[10, 10, 10, 10, 20, 20, 20, 20]), left: arr16(&[40; 8]), corner: 0, top_avail: true, left_avail: true };
        let p = predict_chroma_8x8(PlaneMode::Dc, &n);
        // Block (0,0): both -> (40 + 160 + 4)>>3 = 25.
        assert_eq!(p[0][0], (40 + 160 + 4) >> 3);
        // Block (4,0) top-right: prefers top[4..8]=80 -> (80+2)>>2 = 20.
        assert_eq!(p[0][4], (80 + 2) >> 2);
        // Block (0,4) bottom-left: prefers left -> (160+2)>>2 = 40.
        assert_eq!(p[4][0], (160 + 2) >> 2);
        // Block (4,4): both -> (80 + 160 + 4)>>3 = 30.
        assert_eq!(p[4][4], (80 + 160 + 4) >> 3);
    }

    #[test]
    fn chroma_plane_constant_field_is_constant() {
        let n = NeighborsNxN { top: arr16(&[55; 8]), left: arr16(&[55; 8]), corner: 55, top_avail: true, left_avail: true };
        assert_eq!(predict_chroma_8x8(PlaneMode::Plane, &n), [[55; 8]; 8]);
    }

    fn n8(top: [i32; 16], left: [i32; 8], corner: i32) -> Neighbors8x8 {
        Neighbors8x8 { top, left, corner, top_avail: true, left_avail: true, corner_avail: true }
    }

    #[test]
    fn intra8x8_filter_of_constant_is_constant() {
        // The [1 2 1] low-pass filter preserves a constant field exactly.
        let n = n8([99; 16], [99; 8], 99);
        let f = n.filtered();
        assert_eq!(f.top, [99; 16]);
        assert_eq!(f.left, [99; 8]);
        assert_eq!(f.corner, 99);
    }

    #[test]
    fn intra8x8_filter_smooths_a_spike() {
        // A single spike against a flat background gets [1 2 1]-smoothed; the
        // ends replicate. top = [0,40,0,0,...], all-avail with corner 0.
        let mut top = [0i32; 16];
        top[1] = 40;
        let n = n8(top, [0; 8], 0);
        let f = n.filtered();
        // top[0]: corner avail -> (corner + 2*0 + 40 + 2)>>2 = (0+0+40+2)>>2 = 10
        assert_eq!(f.top[0], (0 + 0 + 40 + 2) >> 2);
        // top[1]: (0 + 2*40 + 0 + 2)>>2 = 20
        assert_eq!(f.top[1], (0 + 80 + 0 + 2) >> 2);
        // top[2]: (40 + 0 + 0 + 2)>>2 = 10
        assert_eq!(f.top[2], (40 + 0 + 0 + 2) >> 2);
        // far end replicates: top[15] = (top[14] + 3*top[15] + 2)>>2 = 0
        assert_eq!(f.top[15], 0);
    }

    #[test]
    fn intra8x8_vertical_copies_filtered_top() {
        // Uniform field (top == corner) -> filter is a no-op -> flat columns.
        let n = n8([70; 16], [70; 8], 70);
        let p = predict_8x8(Intra4x4Mode::Vertical, &n);
        assert_eq!(p, [[70; 8]; 8]);
    }

    #[test]
    fn intra8x8_horizontal_copies_filtered_left() {
        let n = n8([70; 16], [70; 8], 70);
        let p = predict_8x8(Intra4x4Mode::Horizontal, &n);
        assert_eq!(p, [[70; 8]; 8]);
    }

    #[test]
    fn intra8x8_dc_uniform_field() {
        // Uniform neighbours -> filter no-op -> DC equals the field value.
        let n = n8([80; 16], [80; 8], 80);
        assert_eq!(predict_8x8(Intra4x4Mode::Dc, &n), [[80; 8]; 8]);
    }

    #[test]
    fn intra8x8_dc_no_neighbours_is_128() {
        let raw = Neighbors8x8 {
            top: [0; 16],
            left: [0; 8],
            corner: 0,
            top_avail: false,
            left_avail: false,
            corner_avail: false,
        };
        assert_eq!(predict_8x8(Intra4x4Mode::Dc, &raw), [[128; 8]; 8]);
    }

    #[test]
    fn derive_intra_mode_rules() {
        // prev_flag=1 (raw −1): take the predicted mode = min(a, b).
        assert_eq!(derive_intra_mode(Some(4), Some(6), -1), 4);
        assert_eq!(derive_intra_mode(Some(6), Some(1), -1), 1);
        // Either neighbour unavailable -> DC (2) predicted.
        assert_eq!(derive_intra_mode(None, Some(0), -1), 2);
        assert_eq!(derive_intra_mode(Some(0), None, -1), 2);
        assert_eq!(derive_intra_mode(None, None, -1), 2);
        // rem path: rem < pred -> rem; rem >= pred -> rem + 1.
        // pred = min(3,5) = 3. rem=2 (<3) -> 2.
        assert_eq!(derive_intra_mode(Some(3), Some(5), 2), 2);
        // pred = 3, rem=3 (>=3) -> 4.
        assert_eq!(derive_intra_mode(Some(3), Some(5), 3), 4);
        // pred = 3, rem=7 -> 8 (the 9th mode, valid for the larger rem).
        assert_eq!(derive_intra_mode(Some(3), Some(3), 7), 8);
        // pred = DC(2) when a neighbour is missing; rem=2 (>=2) -> 3.
        assert_eq!(derive_intra_mode(None, Some(0), 2), 3);
    }

    #[test]
    fn intra8x8_diag_down_left_depends_on_x_plus_y() {
        // With a uniform field, every avg3 collapses to the constant, so
        // the whole block equals it — exercises the x+y indexing safely.
        let n = n8([55; 16], [55; 8], 55);
        let p = predict_8x8(Intra4x4Mode::DiagDownLeft, &n);
        assert_eq!(p, [[55; 8]; 8]);
    }
}
