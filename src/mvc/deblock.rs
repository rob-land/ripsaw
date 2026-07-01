// In-loop deblocking filter (ITU-T H.264 § 8.7). Fourth building block of
// the libmvc decode core (docs/libmvc-poc.md): smooths block-edge
// discontinuities on the reconstructed frame.
//
// This module implements the per-edge sample filters (§ 8.7.2.3 for
// boundary strength bS < 4, § 8.7.2.4 for bS == 4) as pure functions over
// an 8-sample line `[p3 p2 p1 p0 | q0 q1 q2 q3]`. The boundary-strength
// derivation (§ 8.7.2.1, which needs neighbouring-MB coding info) and the
// edge-walking order live with the macroblock decoder; for an intra frame
// bS is 4 on MB edges and 3 on internal edges.
//
// The α/β/tc0 threshold *tables* are included for the integration, but the
// filter logic is validated here against explicit thresholds, so a table
// transcription slip can't hide behind a passing test (the tables are
// confirmed by the real-frame diff, like the CABAC tables).

#[inline]
fn clip1(v: i32) -> i32 {
    v.clamp(0, 255)
}
#[inline]
fn clip3(lo: i32, hi: i32, v: i32) -> i32 {
    v.clamp(lo, hi)
}

/// One luma edge, boundary strength 1..=3 (§ 8.7.2.3). `s` is
/// `[p3,p2,p1,p0,q0,q1,q2,q3]`, modified in place. `alpha`/`beta` are the
/// activity thresholds; `tc0` the base clipping value for this bS/indexA.
pub fn filter_luma_normal(s: &mut [i32; 8], alpha: i32, beta: i32, tc0: i32) {
    let [_p3, p2, p1, p0, q0, q1, q2, _q3] = *s;
    if !filterable(p0, q0, p1, q1, alpha, beta) {
        return;
    }
    let ap = (p2 - p0).abs();
    let aq = (q2 - q0).abs();
    let tc = tc0 + (ap < beta) as i32 + (aq < beta) as i32;
    let delta = clip3(-tc, tc, (((q0 - p0) << 2) + (p1 - q1) + 4) >> 3);
    s[3] = clip1(p0 + delta);
    s[4] = clip1(q0 - delta);
    if ap < beta {
        s[2] = p1 + clip3(-tc0, tc0, (p2 + ((p0 + q0 + 1) >> 1) - 2 * p1) >> 1);
    }
    if aq < beta {
        s[5] = q1 + clip3(-tc0, tc0, (q2 + ((p0 + q0 + 1) >> 1) - 2 * q1) >> 1);
    }
}

/// One luma edge, boundary strength 4 (§ 8.7.2.4) — the strong filter used
/// on intra MB boundaries.
pub fn filter_luma_strong(s: &mut [i32; 8], alpha: i32, beta: i32) {
    let [p3, p2, p1, p0, q0, q1, q2, q3] = *s;
    if !filterable(p0, q0, p1, q1, alpha, beta) {
        return;
    }
    let small = (p0 - q0).abs() < ((alpha >> 2) + 2);
    if (p2 - p0).abs() < beta && small {
        s[3] = (p2 + 2 * p1 + 2 * p0 + 2 * q0 + q1 + 4) >> 3;
        s[2] = (p2 + p1 + p0 + q0 + 2) >> 2;
        s[1] = (2 * p3 + 3 * p2 + p1 + p0 + q0 + 4) >> 3;
    } else {
        s[3] = (2 * p1 + p0 + q1 + 2) >> 2;
    }
    if (q2 - q0).abs() < beta && small {
        s[4] = (q2 + 2 * q1 + 2 * q0 + 2 * p0 + p1 + 4) >> 3;
        s[5] = (q2 + q1 + q0 + p0 + 2) >> 2;
        s[6] = (2 * q3 + 3 * q2 + q1 + q0 + p0 + 4) >> 3;
    } else {
        // q0' weak strong-filter tap uses p1 (not p0), § 8.7.2.4 eq. 8-476.
        s[4] = (2 * q1 + q0 + p1 + 2) >> 2;
    }
}

/// One chroma edge. bS < 4 modifies only p0/q0 with `tc = tc0 + 1`
/// (§ 8.7.2.3); bS == 4 uses the simple two-tap (§ 8.7.2.4).
pub fn filter_chroma(s: &mut [i32; 8], alpha: i32, beta: i32, tc0: i32, strong: bool) {
    let [_p3, _p2, p1, p0, q0, q1, _q2, _q3] = *s;
    if !filterable(p0, q0, p1, q1, alpha, beta) {
        return;
    }
    if strong {
        s[3] = (2 * p1 + p0 + q1 + 2) >> 2;
        s[4] = (2 * q1 + q0 + p1 + 2) >> 2;
    } else {
        let tc = tc0 + 1;
        let delta = clip3(-tc, tc, (((q0 - p0) << 2) + (p1 - q1) + 4) >> 3);
        s[3] = clip1(p0 + delta);
        s[4] = clip1(q0 - delta);
    }
}

#[inline]
fn filterable(p0: i32, q0: i32, p1: i32, q1: i32, alpha: i32, beta: i32) -> bool {
    (p0 - q0).abs() < alpha && (p1 - p0).abs() < beta && (q1 - q0).abs() < beta
}

/// α threshold — H.264 Table 8-16, indexed by indexA (0..=51).
#[rustfmt::skip]
pub static ALPHA: [i32; 52] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    4, 4, 5, 6, 7, 8, 9, 10, 12, 13, 15, 17, 20, 22, 25, 28,
    32, 36, 40, 45, 50, 56, 63, 71, 80, 90, 101, 113, 127, 144, 162, 182,
    203, 226, 255, 255,
];

/// β threshold — H.264 Table 8-16, indexed by indexB (0..=51).
#[rustfmt::skip]
pub static BETA: [i32; 52] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 6, 6, 7, 7, 8, 8,
    9, 9, 10, 10, 11, 11, 12, 12, 13, 13, 14, 14, 15, 15, 16, 16,
    17, 17, 18, 18,
];

/// tc0 — H.264 Table 8-17, `[indexA][bS-1]` for bS in 1..=3.
#[rustfmt::skip]
pub static TC0: [[i32; 3]; 52] = [
    [0,0,0],[0,0,0],[0,0,0],[0,0,0],[0,0,0],[0,0,0],[0,0,0],[0,0,0],
    [0,0,0],[0,0,0],[0,0,0],[0,0,0],[0,0,0],[0,0,0],[0,0,0],[0,0,0],
    [0,0,0],[0,0,1],[0,0,1],[0,0,1],[0,0,1],[0,1,1],[0,1,1],[1,1,1],
    [1,1,1],[1,1,1],[1,1,1],[1,1,2],[1,1,2],[1,1,2],[1,1,2],[1,2,3],
    [1,2,3],[2,2,3],[2,2,4],[2,3,4],[2,3,4],[3,3,5],[3,4,6],[3,4,6],
    [4,5,7],[4,5,8],[4,6,9],[5,7,10],[6,8,11],[6,8,13],[7,10,14],[8,11,16],
    [9,12,18],[10,13,20],[11,15,23],[13,17,25],
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_region_unchanged() {
        let mut s = [100, 100, 100, 100, 100, 100, 100, 100];
        let before = s;
        filter_luma_normal(&mut s, 20, 4, 2);
        assert_eq!(s, before);
        filter_luma_strong(&mut s, 20, 4);
        assert_eq!(s, before);
    }

    #[test]
    fn strong_edge_not_filtered() {
        // |p0 - q0| = 100 >= alpha(20) -> filterable() is false; untouched.
        let mut s = [100, 100, 100, 100, 200, 200, 200, 200];
        let before = s;
        filter_luma_normal(&mut s, 20, 4, 2);
        assert_eq!(s, before);
    }

    #[test]
    fn normal_filter_known_values() {
        // p = 100s, q = 110s. alpha=20, beta=4, tc0=2.
        //   filterable: |10|<20, |0|<4, |0|<4 -> yes
        //   ap=aq=0<beta -> tc = 2+1+1 = 4
        //   delta = clip(-4,4, ((10<<2)+(100-110)+4)>>3) = clip(34>>3=4) = 4
        //   p0' = 104, q0' = 106
        //   p1' = 100 + clip(-2,2, (100 + 105 - 200)>>1=2) = 102
        //   q1' = 110 + clip(-2,2, (110 + 105 - 220)>>1=-3 -> -2) = 108
        let mut s = [100, 100, 100, 100, 110, 110, 110, 110];
        filter_luma_normal(&mut s, 20, 4, 2);
        assert_eq!(s, [100, 100, 102, 104, 106, 108, 110, 110]);
    }

    #[test]
    fn strong_filter_smooths_three_taps_each_side() {
        // A gentle ramp across the edge, strong filter (bS=4).
        // p3..p0 = 90,92,94,96 ; q0..q3 = 104,106,108,110
        // small = |96-104|=8 < (alpha>>2)+2 = (40>>2)+2 = 12 -> true
        // |p2-p0|=|94-96|=2 < beta(8) -> 3-tap p side
        //   p0' = (94 + 2*94 + 2*96 + 2*104 + 106 + 4) >> 3
        //       = (94+188+192+208+106+4)>>3 = 792>>3 = 99
        //   p1' = (94 + 94 + 96 + 104 + 2) >> 2 = 390>>2 = 97
        //   p2' = (2*90 + 3*94 + 94 + 96 + 104 + 4) >> 3 = (180+282+94+96+104+4)>>3 = 760>>3 = 95
        let mut s = [90, 94, 94, 96, 104, 106, 108, 110];
        filter_luma_strong(&mut s, 40, 8);
        assert_eq!(s[1], 95); // p2'
        assert_eq!(s[2], 97); // p1'
        assert_eq!(s[3], 99); // p0'
    }

    #[test]
    fn strong_filter_weak_tap_uses_p1_not_p0() {
        // bS=4 but NOT "small" (|p0-q0| >= (alpha>>2)+2) -> the single-tap
        // form (§ 8.7.2.4 eq. 8-475/8-476). q0' MUST use p1, not p0 — the
        // exact case a typo missed, caught only by the full-frame JM diff.
        // p3,p2,p1,p0 = 67,67,67,69 ; q0,q1,q2,q3 = 75,77,77,77 ; alpha 13.
        // filterable: |69-75|=6<13, |67-69|=2<beta, |77-75|=2<beta.
        // small = 6 < (13>>2)+2 = 5 -> false -> single tap.
        // p0' = (2*67 + 69 + 77 + 2) >> 2 = 70
        // q0' = (2*77 + 75 + p1=67 + 2) >> 2 = 74   (would be 75 with p0=69)
        let mut s = [67, 67, 67, 69, 75, 77, 77, 77];
        filter_luma_strong(&mut s, 13, 8);
        assert_eq!(s[3], 70); // p0'
        assert_eq!(s[4], 74); // q0' — uses p1=67
    }

    #[test]
    fn chroma_only_touches_p0_q0_in_normal_mode() {
        let mut s = [100, 100, 100, 100, 110, 110, 110, 110];
        filter_chroma(&mut s, 20, 4, 2, false);
        // p1,q1 (indices 2,5) untouched; only p0/q0 change.
        assert_eq!(s[2], 100);
        assert_eq!(s[5], 110);
        assert_ne!(s[3], 100);
        assert_ne!(s[4], 110);
    }

    #[test]
    fn threshold_tables_have_expected_anchors() {
        assert_eq!(ALPHA[15], 0);
        assert_eq!(ALPHA[16], 4);
        assert_eq!(ALPHA[51], 255);
        assert_eq!(BETA[16], 2);
        assert_eq!(BETA[51], 18);
        // tc0 from JM CLIP_TAB (cols bS=1,2,3), H.264 Table 8-17.
        assert_eq!(TC0[51], [13, 17, 25]);
        assert_eq!(TC0[31], [1, 2, 3]);
        assert_eq!(TC0[23], [1, 1, 1]);
        assert_eq!(TC0[15], [0, 0, 0]);
    }
}
