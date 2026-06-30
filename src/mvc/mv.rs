// Motion-vector prediction (ITU-T H.264 § 8.4.1.3). Turns the decoded mvd
// into an actual MV: mv = mvp + mvd, where mvp is the median (or, for 16×8 /
// 8×16 partitions, a directional) prediction from the neighbouring
// partitions' MVs and reference indices.
//
// Neighbours: A = left, B = above, C = above-right (or above-left D when
// above-right is unavailable; the caller supplies whichever applies). Each is
// `Some((mvx, mvy, ref_idx))` or `None` (unavailable → MV (0,0), ref −1).

/// Which neighbour is the directional predictor for a 16×8 / 8×16 partition
/// (§ 8.4.1.3.2). `None` = the general median rule (§ 8.4.1.3.1).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Directional {
    /// 16×8 top partition → above (B); 8×16 left → left (A).
    Above,
    Left,
    /// 8×16 right partition → above-right (C); 16×8 bottom → left (A).
    AboveRight,
}

pub type Neighbour = Option<(i32, i32, i32)>;

#[inline]
fn nv(n: Neighbour) -> (i32, i32, i32) {
    n.unwrap_or((0, 0, -1))
}

#[inline]
fn median3(a: i32, b: i32, c: i32) -> i32 {
    a + b + c - a.max(b).max(c) - a.min(b).min(c)
}

/// Predict the MV for a partition with reference `ref_idx` from neighbours
/// A/B/C. `dir` selects the 16×8 / 8×16 directional case.
pub fn predict_mv(a: Neighbour, b: Neighbour, c: Neighbour, ref_idx: i32, dir: Option<Directional>) -> (i32, i32) {
    let (ax, ay, ar) = nv(a);
    let (bx, by, br) = nv(b);
    let (cx, cy, cr) = nv(c);

    // § 8.4.1.3.2 — directional 16×8 / 8×16 predictor (used iff its ref matches).
    if let Some(d) = dir {
        let (mx, my, mr) = match d {
            Directional::Above => (bx, by, br),
            Directional::Left => (ax, ay, ar),
            Directional::AboveRight => (cx, cy, cr),
        };
        if mr == ref_idx {
            return (mx, my);
        }
    }

    // § 8.4.1.3.1 — if B and C are unavailable but A is, B and C inherit A.
    let (bx, by, br, cx, cy, cr) = if b.is_none() && c.is_none() && a.is_some() {
        (ax, ay, ar, ax, ay, ar)
    } else {
        (bx, by, br, cx, cy, cr)
    };

    // If exactly one neighbour has the matching reference, use its MV.
    let m = [(ax, ay, ar), (bx, by, br), (cx, cy, cr)];
    let matching: Vec<&(i32, i32, i32)> = m.iter().filter(|(_, _, r)| *r == ref_idx).collect();
    if matching.len() == 1 {
        return (matching[0].0, matching[0].1);
    }

    // Otherwise the component-wise median.
    (median3(ax, bx, cx), median3(ay, by, cy))
}

/// P_Skip MV (§ 8.4.1.1): zero if A or B is unavailable, or either is a
/// zero-MV ref-0 predictor; else the median prediction.
pub fn predict_skip_mv(a: Neighbour, b: Neighbour, c: Neighbour) -> (i32, i32) {
    let za = a.map(|(x, y, r)| r == 0 && x == 0 && y == 0).unwrap_or(false);
    let zb = b.map(|(x, y, r)| r == 0 && x == 0 && y == 0).unwrap_or(false);
    if a.is_none() || b.is_none() || za || zb {
        return (0, 0);
    }
    predict_mv(a, b, c, 0, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_zero_neighbours_predict_zero() {
        let z = Some((0, 0, 0));
        assert_eq!(predict_mv(z, z, z, 0, None), (0, 0));
    }

    #[test]
    fn single_matching_ref_wins() {
        // Only B has ref 0; A and C have ref 1. mvp = mvB.
        let a = Some((10, 20, 1));
        let b = Some((4, -6, 0));
        let c = Some((30, 40, 1));
        assert_eq!(predict_mv(a, b, c, 0, None), (4, -6));
    }

    #[test]
    fn median_when_all_match() {
        let a = Some((2, 9, 0));
        let b = Some((8, 1, 0));
        let c = Some((5, 5, 0));
        assert_eq!(predict_mv(a, b, c, 0, None), (5, 5)); // median(2,8,5)=5, median(9,1,5)=5
    }

    #[test]
    fn directional_uses_neighbour_when_ref_matches() {
        // 16×8 bottom partition: directional = Left; A ref matches -> mvA.
        let a = Some((7, 3, 0));
        let b = Some((100, 100, 0));
        let c = Some((0, 0, 0));
        assert_eq!(predict_mv(a, b, c, 0, Some(Directional::Left)), (7, 3));
        // If A's ref does NOT match, fall back to median.
        let a2 = Some((7, 3, 1));
        assert_eq!(predict_mv(a2, b, c, 0, Some(Directional::Left)), (median3(7, 100, 0), median3(3, 100, 0)));
    }

    #[test]
    fn b_c_unavailable_inherit_a() {
        // B, C unavailable, A available -> B=C=A, so all three match -> median = A.
        let a = Some((5, -4, 0));
        assert_eq!(predict_mv(a, None, None, 0, None), (5, -4));
    }

    #[test]
    fn skip_is_zero_when_neighbour_zero() {
        assert_eq!(predict_skip_mv(None, Some((0, 0, 0)), None), (0, 0));
        assert_eq!(predict_skip_mv(Some((3, 3, 0)), Some((0, 0, 0)), Some((3, 3, 0))), (0, 0));
        // Non-trivial neighbours -> median prediction.
        let a = Some((6, 6, 0));
        let b = Some((6, 6, 0));
        assert_eq!(predict_skip_mv(a, b, Some((6, 6, 0))), (6, 6));
    }
}
