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
}
