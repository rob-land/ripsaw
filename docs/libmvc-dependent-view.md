# libmvc — the dependent view (Annex G inter-view prediction)

The 3D payoff and the realtime decider. The base-view intra + inter (P/B)
decode is bit-exact vs JM on synthetic streams (`docs/libmvc-inter.md`); this
arc adds the **dependent view** — view 1 of an MVC stereo pair, which predicts
from the base view (view 0) across views as well as in time.

## Real-data target (in hand)

`scripts/gen-mvc-truth.sh` produces a real MVC decode target from the 3D disc
(Friday the 13th Part 3, title 4 = a 4 s clip). ffmpeg has no MVC encoder, so a
real NAL-20 dependent view can only come from disc.

Stream structure (verified with `examples/probe_subset_sps` + `scan_nals`):
- **Stereo High** (profile_idc 128), level 41, **1920×1080**, 2 views
  (view_id 0 = base, 1 = dependent), `max_num_ref_frames 3`, 23.976 fps.
- Subset SPS (NAL 15) carries the MVC extension: `views=2`,
  **anchor refs v1: L0=[0]** — the dependent anchor predicts from the base
  view (inter-view), L1 empty.
- Per access unit: base view = NAL 5/1 slices, dependent = NAL 20 slices.
  **6 slices per frame** at 1080p. First AU: base IDR (6 slices) + dependent
  anchor (6 slices), bounded by the access-unit delimiter (NAL 9).
- The whole-title front-end parse (subset SPS, NAL-20 slice headers with the
  MVC extension: view_id, anchor_pic_flag, inter_view_flag) **already works**
  — libmvc's front-end is real-data-verified.

Ground truth: JM `ldecod` with **`DecodeAllLayers=1`** decodes both views and
writes per-view YUV (`*_ViewId0000.yuv` base, `*_ViewId0001.yuv` dependent).
Without that flag ldecod prints "Found SVC extension NALU (20). Ignoring." The
first AU's dependent frame differs from the base by only 4683/3,110,400 bytes
(max ±1) — a low-disparity frame, but still a valid bit-exact target.

(`RIPSAW_BASE_INJECT` + the mvcdep carve is a *separate* speed path — it skips
JM's base MB decode and injects a libavcodec base; for ground truth we just
want JM to decode everything, hence `DecodeAllLayers=1`.)

## Status: FULL 3D stereo pair bit-exact (the 3D payoff)

`examples/decode_mvc_dep` decodes the **entire first access unit's stereo pair**
of the real disc, bit-exact vs JM's per-view ground truth, with zero JM pixels
in the decode:
- **Base view** (view 0): the 6-slice 1920×1080 IDR via `decode_intra_frame`.
- **Dependent view** (view 1): the full 6-slice P-frame via
  `recon_inter::decode_p_frame` with the base frame as the inter-view L0
  reference. Inter-view prediction is sample-only in Stereo High, so the inter
  core is reused unchanged.

Both the intra and P paths are multi-slice (`&[&[u8]]`, per-slice CABAC re-init
+ cross-slice neighbour unavailability). Two MVC-specific things:
- `decode_p_frame` has an `idr` param: the dependent anchor is
  `idr_pic_flag = 1` (idr_pic_id + simple ref marking) yet `slice_type = P`.
- the dependent slices reference the subset SPS (NAL 15) + their own PPS (the
  NAL 8 *after* the subset SPS), resolved separately from the base's.

## Temporal decode — 25/96 frames bit-exact (`examples/decode_mvc_clip`)

The full temporal clip decode with a minimal DPB now works for the first 25
access units of the real clip — BOTH views, bit-exact vs JM:
- **base view**: a single-ref P chain, plus a mid-GOP non-IDR I-frame (routed
  by slice_type). `decode_p_frame` takes a ref LIST and decodes `ref_idx` per
  partition (per-b8 for P_8x8) — added `mb_inter::decode_ref_idx` /
  INIT_REF_NO_P; a per-view sliding-window DPB feeds L0.
- **dependent view**: routed by the MVC extension — `anchor_pic_flag` →
  inter-view-only `L0 = [base]`; otherwise the temporal 2-ref
  `L0 = [prev dependent, current base]`. `non_idr_flag` selects the header.
- P_8x8 now expands all sub_mb_types (8×8/8×4/4×8/4×4).

It graceful-stops at frame 25 on the remaining feature: **intra MBs inside a
P-slice** (I_NxN / I_16x16 / I_PCM — real P-frames use them; `decode_p_frame`
returns a clean error). Implementing that (factor recon.rs's per-MB intra
reconstruction into a reusable fn; add the intra pred-mode contexts to the P
path; decode the intra header after the P mb_type) should carry the decode
through the whole clip. Then B-slices if any, then the Ripsaw runner
integration.

## What libmvc needs (the gaps)

The dependent-view decode reuses the **entire inter core** (MC, MV prediction,
residual, deblock, spatial direct, implicit-weighted bi-pred — all done) plus:

1. **Multi-slice frames.** Real 1080p frames are sliced (6×). Each slice has
   its own `first_mb_in_slice` and CABAC re-init; intra/inter prediction and
   the deblock must treat MBs in *other* slices as unavailable. `recon`/
   `recon_inter` currently assume one slice spanning the whole frame.
2. **Bigger/realer base decode.** 1080p, longer GOPs, multi-ref, B-pyramid,
   MMCO/sliding-window ref marking, possibly long-term refs — more than the
   synthetic streams exercised. Needed because the dependent view's temporal
   refs and the base inter-view ref both come from a correctly-managed DPB.
3. **Annex G reference lists (§ G.8.2.4).** The dependent view's L0/L1 are the
   normal temporal lists with the **inter-view reference** (the base view's
   picture at the same access unit) appended per `anchor_ref` /
   `non_anchor_ref` from the subset SPS. The MC then reads base-view samples
   for inter-view-predicted blocks — Stereo High inter-view prediction is
   **sample-only** (the dependent view has its own MVs into the base picture;
   it never inherits base MB modes — see `docs/libmvc-optionb-carve.md`).
4. **NAL-20 slice decode.** The coded-slice-extension slice header + the same
   MB layer as a normal slice, in the dependent view's context.

## Suggested first increment

Decode the **base IDR** of the first AU (multi-slice 1080p intra) and diff vs
`*_ViewId0000.yuv` — this forces the multi-slice work (item 1) and validates
the intra decoder on real Blu-ray content, independent of the dependent view.
Then the dependent anchor (item 3/4) using the decoded base as the inter-view
L0 reference, diffed vs `*_ViewId0001.yuv`.

### Probe finding (`examples/decode_mvc_base`)

Running the intra decoder on the first base IDR slice:
- **Parse is bit-exact** on real content: 1920×1088 (120×68 MBs), High,
  CABAC; the slice header ends at **bit 58, matching the JM trace** (16-bit
  `frame_num`/POC). `pic_init_qp_minus26=0`, `slice_qp_delta=-26` →
  **slice_qp = 0** (JM uses the same).
- **CABAC is bit-exact**: MB0 = `mb_type 0`/`transform_size_8x8 1` (I_8x8),
  first 8×8 coeff level `-3823` — exactly the trace.
- **The gap was the custom scaling matrix** (now fixed). The PPS carries a
  full scaling matrix (`pic_scaling_matrix_present_flag=1`); its intra-8×8
  list has DC weight 6, not 16. At slice_qp=0 the flat-16 dequant over-scaled
  (−3823 → residual −299, clipped to 0); with the real list the residual is
  −112 (JM's ~−113). Wiring it in (commit "apply custom scaling matrices")
  took the base view from 0 → **65+ matching luma rows**.
- **Remaining gaps** (both needed before the base frame fully decodes):
  1. A specific intra case — the decode diverges at MB 548, an I_8×8 with
     `intra4x4_pred_mode 4` (Diagonal_Down_Right), the first occurrence of that
     mode; likely a mode/derivation or 8×8 reference-filter edge case the
     synthetic QP-28 frame didn't exercise (value 40 vs JM 21 at MB-local
     (1,4)). Diff: instrument the MB-548 8×8 prediction vs JM.
  2. **Multi-slice** (items above) — only slice 0 is decoded; slices 1–5 need
     `first_mb_in_slice` + per-slice CABAC re-init + cross-slice neighbour
     unavailability.
  (`disable_deblocking_filter_idc=1` for these slices, so the JM YUV ground
  truth is also pre-deblock — a clean target.)
