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
  `examples/decode_pslice` validates skip prefix + the first coded MB's
  mb_type + mvd (P_16x8, mvd 0,0,42,0) = **70 elements match JM** (incl. the
  magnitude-42 EG3 suffix).
- **Motion compensation** (`mc.rs`): `mc_luma` (quarter-pel 6-tap, all 16
  fractional positions) + `mc_chroma` (eighth-pel bilinear), border-clamped.
  Unit-tested structurally; the authoritative check is the full-frame pixel
  diff once the full P-MB decode feeds it MVs.

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
