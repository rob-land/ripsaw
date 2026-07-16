// H.264 reference-picture list construction (§ 8.2.4) and decoded-reference-
// picture marking (§ 8.2.5), including long-term references. The per-MB decoder
// (`decode_p_frame` / `decode_b_frame`) is agnostic to long-term vs short-term —
// it just indexes the reference list it is handed — so long-term support lives
// entirely here, in how the DPB is marked and how the lists are ordered.
//
// Frame-coded streams only (no fields): PicNum == FrameNumWrap and
// LongTermPicNum == LongTermFrameIdx.

use crate::mvc::slice_header::Mmco;

/// One reference picture in the DPB, reduced to what list construction and
/// marking need. The actual pixels/motion are held alongside by the caller,
/// indexed in lockstep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DpbRef {
    pub frame_num: i32,
    pub poc: i32,
    pub long_term: bool,
    /// Valid only when `long_term`.
    pub long_term_frame_idx: i32,
}

impl DpbRef {
    pub fn short(frame_num: i32, poc: i32) -> Self {
        DpbRef { frame_num, poc, long_term: false, long_term_frame_idx: 0 }
    }
    /// FrameNumWrap (§ 8.2.4.1) — the short-term PicNum, wrapped so recently
    /// coded pictures sort above ones just before a frame_num wraparound.
    fn frame_num_wrap(&self, cur_frame_num: i32, max_frame_num: i32) -> i32 {
        if self.frame_num > cur_frame_num {
            self.frame_num - max_frame_num
        } else {
            self.frame_num
        }
    }
}

/// § 8.2.4.2.1 — initial RefPicList0 for a P/SP slice: short-term references by
/// **descending** PicNum, then long-term references by **ascending**
/// LongTermFrameIdx. Returns indices into `dpb` (before any
/// ref_pic_list_modification / truncation to num_ref_idx_l0_active).
pub fn init_p_list0(dpb: &[DpbRef], cur_frame_num: i32, max_frame_num: i32) -> Vec<usize> {
    let mut short: Vec<usize> = (0..dpb.len()).filter(|&i| !dpb[i].long_term).collect();
    short.sort_by_key(|&i| std::cmp::Reverse(dpb[i].frame_num_wrap(cur_frame_num, max_frame_num)));
    let mut long: Vec<usize> = (0..dpb.len()).filter(|&i| dpb[i].long_term).collect();
    long.sort_by_key(|&i| dpb[i].long_term_frame_idx);
    short.into_iter().chain(long).collect()
}

/// § 8.2.4.2.3 — initial RefPicList0 / RefPicList1 for a B slice.
///
/// L0: short-term with POC < current by descending POC, then POC > current by
/// ascending POC; L1: short-term with POC > current ascending, then POC <
/// current descending. Long-term (ascending LongTermFrameIdx) is appended to
/// both. Finally, when RefPicList1 has more than one entry and is identical to
/// RefPicList0, its first two entries are swapped (§ 8.2.4.2.3). Returns indices
/// into `dpb`.
pub fn init_b_lists(dpb: &[DpbRef], cur_poc: i32) -> (Vec<usize>, Vec<usize>) {
    let short: Vec<usize> = (0..dpb.len()).filter(|&i| !dpb[i].long_term).collect();
    let mut before: Vec<usize> = short.iter().copied().filter(|&i| dpb[i].poc < cur_poc).collect();
    before.sort_by_key(|&i| std::cmp::Reverse(dpb[i].poc));
    let mut after: Vec<usize> = short.iter().copied().filter(|&i| dpb[i].poc > cur_poc).collect();
    after.sort_by_key(|&i| dpb[i].poc);
    let mut long: Vec<usize> = (0..dpb.len()).filter(|&i| dpb[i].long_term).collect();
    long.sort_by_key(|&i| dpb[i].long_term_frame_idx);

    let mut l0: Vec<usize> = before.iter().copied().chain(after.iter().copied()).chain(long.iter().copied()).collect();
    let mut l1: Vec<usize> = after.into_iter().chain(before).chain(long).collect();
    let _ = &mut l0;
    if l1.len() > 1 && l1 == l0 {
        l1.swap(0, 1);
    }
    (l0, l1)
}

/// § 8.2.5.3 — sliding-window marking. When the DPB already holds
/// `num_ref_frames` references (short + long), the short-term picture with the
/// smallest FrameNumWrap is marked unused (evicted); long-term pictures are
/// never evicted here. Returns the index to remove, or `None` if there is room.
pub fn sliding_window_victim(dpb: &[DpbRef], cur_frame_num: i32, max_frame_num: i32, num_ref_frames: usize) -> Option<usize> {
    if dpb.len() < num_ref_frames.max(1) {
        return None;
    }
    dpb.iter()
        .enumerate()
        .filter(|(_, r)| !r.long_term)
        .min_by_key(|(_, r)| r.frame_num_wrap(cur_frame_num, max_frame_num))
        .map(|(i, _)| i)
}

/// § 8.2.5.4 — apply the adaptive (MMCO) marking operations to the DPB, which
/// must hold the reference pictures *before* the current one is inserted.
/// Mutates `dpb` (evicting or relabelling entries) and returns the
/// LongTermFrameIdx to assign to the *current* picture (MMCO 6), if any.
pub fn apply_mmco(dpb: &mut Vec<DpbRef>, mmco: &[Mmco], cur_frame_num: i32, max_frame_num: i32) -> Option<i32> {
    let curr_pic_num = cur_frame_num;
    let mut current_long_idx = None;
    for op in mmco {
        match *op {
            Mmco::ForgetShort { diff_pic_nums_minus1 } => {
                let pic_num_x = curr_pic_num - (diff_pic_nums_minus1 as i32 + 1);
                dpb.retain(|r| r.long_term || r.frame_num_wrap(cur_frame_num, max_frame_num) != pic_num_x);
            }
            Mmco::ForgetLong { long_term_pic_num } => {
                dpb.retain(|r| !(r.long_term && r.long_term_frame_idx == long_term_pic_num as i32));
            }
            Mmco::ShortToLong { diff_pic_nums_minus1, long_term_frame_idx } => {
                let pic_num_x = curr_pic_num - (diff_pic_nums_minus1 as i32 + 1);
                let lti = long_term_frame_idx as i32;
                // A long-term index is unique: free any current holder first.
                dpb.retain(|r| !(r.long_term && r.long_term_frame_idx == lti));
                if let Some(r) = dpb.iter_mut().find(|r| !r.long_term && r.frame_num_wrap(cur_frame_num, max_frame_num) == pic_num_x) {
                    r.long_term = true;
                    r.long_term_frame_idx = lti;
                }
            }
            Mmco::SetMaxLong { max_long_term_frame_idx_plus1 } => {
                let max_idx = max_long_term_frame_idx_plus1 as i32 - 1;
                dpb.retain(|r| !(r.long_term && r.long_term_frame_idx > max_idx));
            }
            Mmco::ResetAll => {
                dpb.clear();
                current_long_idx = None;
            }
            Mmco::CurrentToLong { long_term_frame_idx } => {
                let lti = long_term_frame_idx as i32;
                dpb.retain(|r| !(r.long_term && r.long_term_frame_idx == lti));
                current_long_idx = Some(lti);
            }
        }
    }
    current_long_idx
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(frame_num: i32, poc: i32) -> DpbRef {
        DpbRef::short(frame_num, poc)
    }
    fn lt(idx: i32, poc: i32) -> DpbRef {
        DpbRef { frame_num: 0, poc, long_term: true, long_term_frame_idx: idx }
    }

    #[test]
    fn p_list0_short_desc_then_long_asc() {
        // Short-term frame_nums 2,4,3 → desc PicNum [4,3,2]; long-term idx 1,0
        // → asc [0,1]; appended after the short-term ones.
        let dpb = vec![st(2, 4), lt(1, 0), st(4, 8), st(3, 6), lt(0, 2)];
        let list = init_p_list0(&dpb, 4, 16);
        // indices: st4=2, st3=3, st2=0, lt0=4, lt1=1
        assert_eq!(list, vec![2, 3, 0, 4, 1]);
    }

    #[test]
    fn p_list0_frame_num_wrap_orders_recent_first() {
        // cur_frame_num 1, max 16: frame_num 15 wraps to −1, so the fresh
        // frame_num 0 (PicNum 0) sorts ABOVE the pre-wrap 15 (PicNum −1).
        let dpb = vec![st(15, 30), st(0, 34)];
        let list = init_p_list0(&dpb, 1, 16);
        assert_eq!(list, vec![1, 0]);
    }

    #[test]
    fn b_lists_split_by_poc_around_current() {
        // Current POC 4; short-term POCs 0,2 (before) and 6,8 (after).
        let dpb = vec![st(0, 0), st(1, 2), st(3, 6), st(4, 8)];
        let (l0, l1) = init_b_lists(&dpb, 4);
        // L0 = before desc [2,0] then after asc [6,8] → idx [1,0,2,3]
        assert_eq!(l0, vec![1, 0, 2, 3]);
        // L1 = after asc [6,8] then before desc [2,0] → idx [2,3,1,0]
        assert_eq!(l1, vec![2, 3, 1, 0]);
    }

    #[test]
    fn b_lists_swap_first_two_when_l1_equals_l0() {
        // Only future refs → L0 and L1 both = [after asc]; identical with >1
        // entry ⇒ swap L1[0],L1[1].
        let dpb = vec![st(3, 6), st(4, 8)];
        let (l0, l1) = init_b_lists(&dpb, 4);
        assert_eq!(l0, vec![0, 1]);
        assert_eq!(l1, vec![1, 0]);
    }

    #[test]
    fn sliding_window_evicts_smallest_framenum_short_term() {
        // Full DPB (4 refs, num_ref_frames 4): evict the oldest short-term
        // (smallest FrameNumWrap); the long-term is never a victim.
        let dpb = vec![lt(0, 0), st(3, 6), st(1, 2), st(2, 4)];
        let v = sliding_window_victim(&dpb, 3, 16, 4);
        assert_eq!(v, Some(2)); // st frame_num 1
        // Room to spare ⇒ no eviction.
        assert_eq!(sliding_window_victim(&dpb[..2], 3, 16, 4), None);
    }

    #[test]
    fn mmco3_promotes_short_term_to_long() {
        // CurrPicNum 5; MMCO 3 diff 2 → PicNum 5-3=2 becomes long-term idx 0.
        let mut dpb = vec![st(2, 4), st(4, 8), st(3, 6)];
        let cur = apply_mmco(&mut dpb, &[Mmco::ShortToLong { diff_pic_nums_minus1: 2, long_term_frame_idx: 0 }], 5, 16);
        assert_eq!(cur, None);
        let promoted = dpb.iter().find(|r| r.frame_num == 2).unwrap();
        assert!(promoted.long_term && promoted.long_term_frame_idx == 0);
    }

    #[test]
    fn mmco6_marks_current_long_and_mmco1_forgets_short() {
        // MMCO 1 forgets PicNum 5-1=4; MMCO 6 marks the current pic long-term 1.
        let mut dpb = vec![st(4, 8), st(3, 6)];
        let cur = apply_mmco(&mut dpb, &[Mmco::ForgetShort { diff_pic_nums_minus1: 0 }, Mmco::CurrentToLong { long_term_frame_idx: 1 }], 5, 16);
        assert_eq!(cur, Some(1));
        assert!(dpb.iter().all(|r| r.frame_num != 4)); // frame_num 4 evicted
    }
}
