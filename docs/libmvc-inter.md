# libmvc — the inter + dependent-view arc

The intra decoder is complete and bit-exact vs JM (`docs/libmvc-poc.md`).
This is the next arc: inter prediction (P/B slices) and then the dependent
(3D) view — the actual MVC payoff, and the part that decides whether
realtime MVC→FSBS is feasible. Motion compensation is the throughput-
critical core shared by both base inter decode and the dependent view.

## Validation harness

`scripts/gen-intra-test-frame.sh` emits, alongside the intra frame, an
**inter test stream**: `inter.h264` = IDR + one P-slice (single ref, no B,
real motion, QP 28, CABAC). JM ground truth: `inter_post.yuv` (final
2-frame YUV) and `inter_predeblock.bin` (the P-frame's pre-deblock recon),
plus the per-element `trace_dec.txt`. The trace `@` counter is cumulative
across slices; the P-slice's macroblock elements follow the IDR's.

The decoded base IDR (bit-exact, the intra decoder) is the **reference
frame** the P-slice's motion compensation reads from.

## Done (bit-exact vs JM)

- **Slice header** for P/B was already complete (`slice_header.rs`, the MVC
  front-end): ref-idx overrides, `ref_pic_list_modification`,
  `pred_weight_table`, `dec_ref_pic_marking`, `cabac_init_idc`.
- **Inter CABAC context init** (`mb_inter.rs`): `INIT_MB_TYPE_P[model]`
  transcribed for all 3 cabac_init_idc models; `InterContexts::new(idc, qp)`.
- **mb_skip_flag** (P): ctxIdxInc = (left-not-skip) + (up-not-skip), context
  `mb_type_contexts[1][a+b]`. JM trace inversion: traced value is
  `(bin != 1)`, so **trace 0 = skipped, 1 = coded**.
- **Inter mb_type** (`decode_inter_mb_type`): the JM bin tree; the traced
  value `act_sym` = 1 P_16x16 / 2 P_16x8 / 3 P_8x16 / 4 P_8x8 / ≥6 intra
  (incl. the I_16x16 suffix bins + I_PCM terminate).
- **mvd** (`decode_mvd_component`): bin-0 on `mv_res_contexts[0][5k+inc]`
  then the UEG3 magnitude (`unary_exp_golomb_mv`, exp_start 8, EG3 suffix) +
  bypass sign; `mvd_ctx_inc` from the neighbour-|mvd| sum. INIT_MV_RES_P
  transcribed (3 models).
- **Full P-MB residual** (`decode_pslice`): coded_block_pattern (shared
  `decode_cbp_ctx`), transform_size_8x8_flag, mb_qp_delta (shared
  `decode_dquant_ctx`), and the inter residual (`decode_mb_residual` with
  `is_inter`: cbf default_bit 0, 4×4/8×8 dispatch, P-model residual contexts
  via `ResidualContexts::new(qp, true)`). The first coded MB decodes end to
  end (skip → mb_type → mvd → cbp → transform → qp → residual →
  end_of_slice) = **88 elements match JM** (incl. the run/level residual).
  Tables: INIT_{CBP,TRANSFORM_SIZE,DELTA_QP,BCBP,MAP,LAST,ONE,ABS}_P.
- **Motion compensation** (`mc.rs`): `mc_luma` (quarter-pel 6-tap, all 16
  fractional positions) + `mc_chroma` (eighth-pel bilinear), border-clamped.
  Unit-tested structurally; the authoritative check is the full-frame pixel
  diff once the full P-MB decode feeds it MVs.
- **MV prediction** (`mv.rs`): `predict_mv` — the § 8.4.1.3.1 median (with the
  B,C-unavailable-inherit-A and exactly-one-matching-ref rules) + the
  § 8.4.1.3.2 directional 16×8 / 8×16 case; `predict_skip_mv` for P_Skip
  (§ 8.4.1.1). Unit-tested. mv = mvp + mvd. **This completes the inter MB
  decode side.**

- **First inter PIXELS bit-exact** (`examples/decode_inter_mb`): the skip
  prefix (P_Skip, MV 0 → copy the reference) and the first coded MB
  (P_16×8: `mc_luma`/`mc_chroma` per partition + the inter residual) match
  JM's P-frame exactly. MB 32's bottom partition has MV (42,0) — a half-pel
  horizontal position — so it exercises and **validates the 6-tap mc_luma
  interpolation on real data**. Uses JM's decoded IDR (frame 0 of
  inter_post.yuv) as the reference, isolating the MC/MV/residual integration.

- **FULL P-SLICE reconstruction bit-exact** (`examples/decode_pframe`): the
  whole P-frame — all 48 MBs (32 P_Skip + P_16×16/16×8/8×16/8×8) — decoded
  and reconstructed, Y+U+V matching JM's `inter_predeblock.bin` exactly. Per-
  4×4-block grids (`g_mv`/`g_mvd`/`g_ref`) drive the mvd ctxIdxInc and the
  full `predict_mv` (median + the 16×8/8×16 directional cases) /
  `predict_skip_mv`; per-MB `cbpv` (cbp neighbour ctx) and **`t8grid`
  (transform_size_8x8_flag neighbour ctxIdxInc = left.t8 + up.t8)** grids
  feed the syntax contexts. `decode_sub_mb_type` (INIT_B8_TYPE_P) handles
  P_8×8 (all P_L0_8×8 here). Partition→MV→MC→residual per partition, chroma
  MV = luma MV (4:2:0). The mb_type sequence matches the JM trace
  (2×7,4,3,1,4,3,3,4,1,1). **Lesson (4th time): a hardcoded `transform[0]`
  context survived the first-MB pixel test but desynced CABAC three MBs after
  the first transform-8×8 neighbour — only the whole-frame decode caught it.**

## Remaining: inter deblock → B-slices → dependent view

1. ~~Per-MB MV + ref grids~~ DONE (`g_mv`/`g_mvd`/`g_ref`).
2. ~~Full P-slice decode~~ DONE (`decode_pframe`, bit-exact). Caveat: this
   stream's P-slice has no intra-in-P MBs and all P_8×8 subs are P_L0_8×8 —
   `decode_pframe` asserts those, so intra-in-P + non-8×8 sub-partitions are
   still untested (need a stream that exercises them).
3. ~~**Own reference frame**~~ DONE. The intra reconstruction is now the
   library `recon::decode_intra_frame` (→ `Frame` with planes + MbInfo/QP
   grids) + `Frame::deblock_intra` (§ 8.7). `examples/decode_inter_full`
   decodes `inter.h264` end to end with **zero JM pixels**: libmvc decodes the
   IDR (validated vs JM frame 0), then uses its **own post-deblock IDR** as the
   P-slice MC reference (P-frame matches `inter_predeblock.bin`). NB the MC
   reference must be the *post-deblock* IDR — this stream deblocks the IDR but
   not the P-slice; pre-deblock was off by 1-2/pixel.
4. ~~**Inter deblock**~~ DONE (`deblock_inter` in `decode_inter_full`). The
   § 8.7.2.1 bS: per 4-sample segment, 2 if either side has nonzero luma
   coeffs, 1 if refs differ or |Δmv| ≥ 4 (¼-pel), else 0. Luma 4 edges × 4
   segments (transform-8×8 skips internal 4,12); chroma edges 0,4 take the
   co-located luma edge's (0,8) bS. The P-frame matches JM BOTH pre-deblock
   (`inter_predeblock.bin`) and post-deblock (`inter_post.yuv` frame 1 —
   1065/18432 samples change). New `CbpBits::luma4x4_nonzero`; per-MB qp_grid +
   per-4×4 nonzero grid; running QP (qp += mb_qp_delta mod 52). **The whole
   base-view inter path is now bit-exact end to end, zero JM pixels.**
5. **B-slices** (next) — see below. On the realtime-MVC critical path (real
   Blu-ray uses B-frames in both views).
6. **Dependent view** (Annex G inter-view prediction) — the 3D payoff and the
   realtime decider. The inter core (MC + MV pred + residual + deblock) is now
   reusable for it. Needs a real MVC test stream (NAL 20
   coded-slice-extension); the single-view ffmpeg harness can't produce one —
   use the disc clip via MakeMKV keep-MVC.

## B-slice arc (scoped, not yet built)

Harness: `scripts/gen-intra-test-frame.sh` now emits `bframe.h264` (I P B B in
decode order, `qp 26`, spatial direct, `num_ref_idx l0=l1=1`,
`bframes=2 b-pyramid=none ref=2`). `bframe_post.yuv` = all 4 frames in
**display** order (I B B P). The DUMP_RECON hook keeps only the **last** decoded
frame's pre-deblock (the 2nd B) — extend it to dump per-picture for pixel-exact
B validation; the CABAC trace validates syntax as-is.

First B-slice (from the trace): `direct_spatial_mv_pred_flag=1`, then 32 B_Skip
MBs (spatial direct), then coded MBs. The first coded MB is `mb_type 2` with 4
`mvd_l0` + cbp/residual. mb_type values seen include 0 (B_Direct_16x16), 2, 3,
13, 22 (B_8x8), and an intra suffix (≥ 23).

What B needs beyond the (done) P inter core:
- **B `mb_type` CABAC tree** (JM `readMB_typeInfo_CABAC` B branch) → act_sym
  0..48: B_Direct_16x16, B_{L0,L1,Bi}_{16x16,16x8,8x16}, B_8x8, intra suffix.
  And **B `sub_mb_type`** (12 types incl. direct/L0/L1/Bi at 8x8/8x4/4x8/4x4).
- **Two reference lists** L0/L1 with the § 8.2.4.2.3 POC-based default order;
  POC from `pic_order_cnt_lsb` (already parsed).
- **Spatial direct** (§ 8.4.1.2.2): MVs from spatial neighbours + the
  colZeroFlag test against the **co-located** ref's MV/refIdx → need to store
  the co-located picture's per-4×4 MV/ref grid (the P-frame's, kept from step
  4's decode). B_Skip = B_Direct with no residual.
- **Bi-prediction**: average the L0 and L1 `mc_luma`/`mc_chroma` predictions
  (§ 8.4.2.3, default; `weighted_bipred_idc` here 0).
- **B deblock**: the bS rules extend to two lists (compare both refs/MVs).

## P-MB decode order (from the trace, MB 32)

```
mb_skip_flag(1)            # coded
mb_type(2)
mvd_l0 × (2 per partition) # 4 here -> 2 partitions
coded_block_pattern
transform_size_8x8_flag
mb_qp_delta
residual ("Luma sng" …)   # reuses the intra residual decode (mb_residual.rs)
end_of_slice_flag
```
ref_idx_l0 is absent because num_ref_idx_l0_active == 1 (single ref).

## Inter mb_type decode (JM read_MB_typeInfo_CABAC_p_slice)

Bin tree on `mb_type_contexts[1]` (the P sub-array):
```
ctx4 == 1 -> intra: ctx7 ? 7 : 6  (7 = I_16x16 prefix, then AC/cbp/pred bins
                                    on ctx8/9/10 + biari_final for I_PCM)
ctx4 == 0 -> inter:
  ctx5 == 1 -> ctx7 ? act_sym=2 : 3
  ctx5 == 0 -> ctx6 ? act_sym=4 : 1
```
The traced `mb_type` value is this `act_sym`. **Open: confirm the act_sym →
partition mapping** empirically (the trace shows mvd counts: type 2/3 → 4
mvd = 2 partitions; type 0 → 8 mvd = P_8x8 w/ sub_mb_type). Build the decode,
emit act_sym, diff vs trace, and read the partition count back from it.

## mvd decode (JM read_mvd_CABAC)

`k` = component (0=x,1=y). ctxIdxInc from neighbour |mvd| sum
(`absSum < 3 -> 0`, `> 32 -> 3`, else `2`) added to `5*k`:
- bin0: `mv_res_contexts[0][5*k + inc]`.
- if nonzero: `unary_exp_golomb_mv_decode(mv_res_contexts[1] + 5*k, 3) + 1`
  (UEG3: truncated-unary prefix + EG3 suffix), then a bypass sign bit.

Needs a per-4×4-block `mvd` grid for the neighbour context (JM
`mb_data[].mvd[list][y][x][k]`). Tables: `INIT_MV_RES_P[model][2][10]`
(transcribe).

## Roadmap (each step trace- then pixel-validated)

1. **Inter mb_type + mvd + sub_mb_type + ref_idx** → P-MB syntax bit-exact
   vs trace (extend `decode_pslice`). Tables: MB_TYPE_P (done), B8_TYPE_P,
   MV_RES_P, REF_NO_P.
2. **MV prediction** (median of neighbour MVs, § 8.4.1.3) + the per-block
   MV/ref grids; the actual MV = mvp + mvd.
3. **Reference list construction** (§ 8.2.4) + a minimal **DPB** (here just
   the one IDR ref). Annex G § 8.2.4 extends this for the dependent view.
4. **Motion compensation**: luma 6-tap half-pel + quarter-pel
   (§ 8.4.2.2.1), chroma 1/8-pel bilinear; reconstruct = MC pred + residual
   (residual decode already exists). Diff vs `inter_predeblock.bin`.
5. **Deblock** for inter (bS now depends on MVs/refs/coded-block — § 8.7.2.1)
   → diff vs `inter_post.yuv`.
6. **B-slices** (two ref lists, bi-pred, temporal/spatial direct).
7. **Dependent view**: reuse the whole inter core + Annex G inter-view
   reference-list construction; feed the base frame (from libavcodec via the
   mvcdep seam, or libmvc's own base decode) as an inter-view reference.
   This is the 3D payoff and where a realtime MVC→FSBS benchmark becomes
   meaningful.
