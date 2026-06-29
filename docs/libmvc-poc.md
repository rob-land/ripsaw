# libmvc — proof-of-concept scope

Status: scoping doc (2026-06-29). Companion to `docs/libmvc.md` (the
architecture survey), `docs/libmvc-injection.md` (injection seam proven),
and `docs/libmvc-optionb-carve.md` (the shipped mvcdep carve).

## What a PoC needs to prove (and what it doesn't)

Three things about libmvc are *already* settled, so the PoC must not
re-litigate them:

- **The front-end parser works.** `src/mvc/` parses NAL/RBSP/SPS (base +
  MVC ext)/PPS/slice-header on real Blu-ray streams, verified
  (`docs/libmvc.md`). The PoC consumes it; it doesn't rebuild it.
- **The hybrid architecture works.** libavcodec's base view is bit-exact
  to the inter-view reference, and JM's dependent decode accepts an
  injected base (`docs/libmvc-injection.md` §2b). So "decode base fast,
  inject into the dependent decode" is proven — by the *mvcdep carve*,
  which already ships a 1.54× decoder.
- **Inter-view list construction** (Annex G IDC 4/5) is parsed
  (`ref_pic_list_modification.rs`).

The one thing nothing has demonstrated: **can we write a correct H.264
decode core in Rust?** That — CABAC + residual + transform + prediction +
deblocking — is the multi-month grind, and the only real risk left. The
PoC exists to de-risk *that*, on the smallest unit that still exercises
the hard parts, validated bit-exact.

## The PoC target: one intra frame, bit-exact

**Decode the base view's first IDR frame in pure Rust and match
libavcodec byte-for-byte.**

Why an IDR (intra) frame of the *base* view:

- **Standalone.** An IDR needs no reference pictures and no DPB — it
  decodes from nothing. That removes motion compensation, the DPB, and
  reference management (all large) from the PoC while keeping the full
  *pixel* pipeline.
- **It exercises the gnarliest parts.** Residual parsing, inverse
  transform, intra prediction, and deblocking are the spec-detail-heavy,
  bug-prone core — and CABAC is the single hardest component. An intra
  frame hits all of them.
- **Reusable.** The dependent view uses the *same* macroblock machinery;
  proving it on the base IDR is the foundation, not a throwaway. (The
  shipping product offloads base decode to libavcodec per Option B, but
  the MB core the PoC builds is exactly what the dependent view later
  needs.)
- **Bit-exact reference on hand.** `ffmpeg -i clip.mkv -frames:v 1 -f
  rawvideo -pix_fmt yuv420p` gives the golden frame to diff against;
  H.264 §8 guarantees numerically identical output, so the bar is an
  exact match, not PSNR.
- **Real and representative.** Blu-ray = High profile, **CABAC**, 8-bit
  4:2:0, progressive. The PoC targets exactly that profile.

## Scope — in

The minimum to decode one High-profile intra frame:

1. **CABAC decode engine** — the arithmetic decoder (§9.3.3.2), context
   initialisation from `cabac_init_idc` + SliceQP (§9.3.1), and the
   context models touched by I-slice syntax. *The biggest single piece.*
2. **Macroblock layer for I-slices** (§7.3.5): `mb_type` (I_NxN /
   I_16x16 / I_PCM), `transform_size_8x8_flag`, intra prediction-mode
   syntax, `intra_chroma_pred_mode`, `coded_block_pattern`,
   `mb_qp_delta`.
3. **Residual decode** (§7.3.5.3 / §9.3): `residual_block_cabac`,
   significance maps, coefficient levels; inverse scan (zig-zag),
   inverse quantisation.
4. **Inverse transform**: 4×4 and 8×8 integer IDCT, plus the
   Hadamard/DC paths for I_16x16 and chroma DC.
5. **Intra prediction**: luma 4×4 (9 modes), 8×8 (9 modes), 16×16
   (4 modes); chroma 8×8 (4 modes); constrained-intra handling.
6. **Deblocking filter** (§8.7) for the reconstructed frame.
7. **Frame assembly**: write reconstructed samples, apply
   `frame_cropping` (the SPS parser already derives the cropped dims).
8. A small **decode harness** + example (`examples/decode_intra.rs`) that
   feeds a real frame in and dumps YUV for the diff.

## Scope — explicitly out (deferred to later phases)

- **Inter prediction / motion compensation**, P/B slices, weighted pred.
- **The DPB** and reference-picture management.
- **Inter-view prediction** (Annex G) — the dependent-view phase, the
  point of libmvc. Deliberately *after* the base intra core works.
- **CAVLC** — Blu-ray is CABAC; CAVLC (for baseline streams) is a later
  add, not on the 3D-BD path.
- **MBAFF / interlaced / fields** — Blu-ray 3D is progressive
  (`frame_mbs_only_flag = 1`, confirmed by the parser).
- **10-bit / 4:2:2 / 4:4:4** — Blu-ray is 8-bit 4:2:0.
- **I_PCM** beyond a trivial passthrough (rare; can stub-then-fill).

## Validation

1. Rip the 4-second MVC clip (Friday the 13th title 4, `--minlength=0`)
   and `extract_to_annex_b` it (both already scripted).
2. Split out the **base-view** IDR access unit (NAL types 1/5, layer 0 —
   the parser already classifies these).
3. Decode it with the Rust PoC → `poc.yuv` (one 1920×1080 4:2:0 frame).
4. `ffmpeg -i clip.mkv -map 0:v:0 -frames:v 1 -f rawvideo -pix_fmt
   yuv420p ref.yuv`.
5. **Assert `poc.yuv == ref.yuv` byte-for-byte.** On a mismatch, a
   per-MB / per-plane diff localises the failing stage (intra vs residual
   vs deblock), since each is independently checkable.

Stretch validation: decode the first *N* intra frames across a couple of
discs to cover more intra-mode/CBP combinations than one frame hits.

## What this reuses vs. builds

| Reuses (done) | Builds (the PoC) |
|---|---|
| `bitstream` (bit reader, ue/se) | CABAC arithmetic engine + contexts |
| `nal`, `rbsp`, `annexb` | MB-layer I-slice syntax |
| `sps` (geometry, QP, transform_8x8) | residual + inverse transform |
| `pps` (entropy mode, QP, scaling) | intra prediction |
| `slice_header` (type, QP, first_mb) | deblocking filter |
| `mkv_extract` (→ Annex B) | frame assembly + crop |

## Effort

| Task | Est. |
|---|---|
| CABAC engine (arith decoder + context init/models) | 1–1.5 wk |
| MB-layer I-slice syntax + residual parse | ~1 wk |
| Inverse transform (4×4/8×8/DC) + dequant | 3–4 d |
| Intra prediction (all luma/chroma modes) | ~1 wk |
| Deblocking filter | 4–5 d |
| Harness, frame assembly, bit-exact validation + debugging | ~1 wk |
| **Total** | **~5–6 wk** |

This is a real chunk — but it is the bounded, high-information slice. It
proves the hardest ~60% of the decoder (everything except inter/DPB) and,
critically, the CABAC engine that the rest of the decoder is built on.

## Progress (2026-06-29)

The independently-validatable building blocks are done and unit-tested:

| Module | What | Validation |
|---|---|---|
| `cabac.rs` | CABAC engine (decision/bypass/terminate, FL + Exp-Golomb bypass, ctx init, tables) | round-trip vs a reference encoder (engine + binarisation logic); tables pending real-frame |
| `transform.rs` | dequant + inverse 4×4 transform, luma/chroma DC Hadamard, inverse zig-zag scan | exactly-computable cases + permutation check |
| `intra.rs` | Intra_4x4 (9 modes), Intra_16x16 + chroma (V/H/DC/Plane) | V/H/DC exact, directional constant-field + known taps |
| `deblock.rs` | luma/chroma edge filters (bS<4 and bS=4) + threshold tables | hand-computed filter outputs; tables pending real-frame |
| `residual.rs` | `residual_block_cabac` — significance map, last flag, run-adaptive level + sign (UEG0) | round-trip vs a matching reference encoder across coeff patterns |

These are the pieces whose correctness can be checked in isolation —
i.e. the entire decode-core *logic*. ~30 unit tests across them.

**Remaining (the keystone — the integration phase):** the
macroblock-layer syntax that drives the engine (`mb_type` / intra-mode /
CBP / `mb_qp_delta` / `coded_block_flag`), the full context-model init
tables (Tables 9-12..9-33), neighbour management + context derivation
(MB-type, cbf, the boundary-strength derivation feeding deblocking), the
slice decode loop (predict + add residual + clip → frame), frame assembly
+ crop, and the real-frame validation harness.

This block is tightly coupled and can only be *validated* end-to-end
against a real frame (the § Validation diff) — so it's an
iterative-debugging effort built with that harness, not more isolated
modules. The isolation-validatable foundation is now complete; this is the
next, distinct work block.

### Integration progress (2026-06-29)

The validation harness and a per-element feedback loop are in place, and
the macroblock decode has started — validated against JM ldecod bit-exact:

- **ldecod trace tool** (`scripts/build-ldecod-trace.sh`, `src/mvc/trace.rs`,
  `examples/cmp_trace.rs`): a `TRACE=1` ldecod emits one line per syntax
  element; the parser captures both the `(value)` header elements **and**
  the `name <level> <run>` residual run/level lines (690,950 elements for
  the base IDR frame), and `first_divergence` pinpoints the first mismatch.
- **MB-header decode** (`src/mvc/mb_header.rs`): I-slice mb_type /
  transform_size_8x8_flag / intra modes / chroma / cbp / mb_qp_delta, with
  context-init transcribed verbatim from JM `ctx_tables.h`.
- **Residual decode** (`src/mvc/residual.rs`, `residual_ctx.rs`): the
  significance map (aliased through the `pos2ctx_map8x8`/`pos2ctx_last8x8`
  position→context maps for 8×8), last-flag, and the run-adaptive level
  decode (UEG0 prefix + EG0 bypass suffix), driven by per-category context
  banks built from the verbatim `INIT_{BCBP,MAP,LAST,ONE,ABS}_I` tables.
- **The whole base-view I-slice decodes bit-exact vs JM**
  (`examples/decode_slice.rs`): **1320 MBs / 11,883 syntax elements match
  the ldecod trace exactly** — header context derivation, neighbour
  contexts, the 8×8 luma residual (incl. MB 0's level −3823 driving the
  saturated TU prefix into the EG0 bypass), and `end_of_slice`. Standing
  up the slice loop caught a silent desync that turned out to be two
  off-by-one typos in the normative CABAC tables (`RANGE_TAB_LPS[31][0]`
  28→29, `TRANS_IDX_LPS[28]` 23→22) — invisible to the round-trip tests
  (the reference encoder shares the table) but fatal against an
  independent decoder. Now pinned by a regression test.

This validates the entire CABAC + context-model + neighbour-derivation +
residual stack on real data. **Remaining for the PoC:** the reconstruction
path — apply the inverse transform/dequant to the decoded coefficients,
add intra prediction, clip, deblock, crop → diff the *pixels* against
ffmpeg/JM YUV. The syntax decode (the hard, unforgiving part) is done;
the remaining work is arithmetic on already-correct coefficients. Chroma
residual + I_4x4/I_16x16 residual categories are still stubbed in the
slice loop (this frame's coded MBs are all I_8×8 luma) — they reuse the
same `decode_residual_block` with their category descriptors plus the
`coded_block_flag` neighbour path.

## How it sequences toward full libmvc

1. **PoC (this doc)** — base IDR intra, bit-exact. Proves the pixel
   pipeline + CABAC.
2. **Inter + DPB** — P/B slices, motion compensation, reference
   management → a full base-view decoder. (Optional for the product,
   since Option B offloads base decode to libavcodec; but the inter MC
   code is needed for the dependent view anyway.)
3. **Dependent view** — reuse the MB core + add inter-view reference-list
   construction (Annex G §8.2.4) and feed in libavcodec's base frame (the
   injection seam from `docs/libmvc-injection.md`). *This is where libmvc
   replaces ldecod.*
4. **Integration** — implement the `MvcDecoder` trait (`decoder.rs`
   stub), the `layout` composer (`layout.rs` stub), and swap libmvc in
   for the ldecod subprocess in `convert/runner.rs`; unlock the
   sister-player realtime path.

## Open questions / risks

- **SIMD timing.** The PoC targets *correctness*, not speed; scalar Rust
  intra decode will be slow. Whether libmvc ultimately beats ldecod (let
  alone Option A's libavcodec-SIMD dependent decode) depends on later
  SIMD work. The PoC should still log fps so we have an early datapoint.
- **CABAC is unforgiving.** *(Materialised and resolved.)* A single
  context-model or table bug silently corrupts everything downstream — and
  exactly this happened: two off-by-one CABAC-table entries that the
  round-trip tests could not catch (encoder and decoder share the table,
  so they drift together) survived until the full-slice diff against JM.
  The mitigation that worked: diff every syntax element against an
  *independent* decoder's trace (`examples/decode_slice.rs`), and when it
  desyncs, instrument both engines to print `range`/`offset` per MB to
  localise, then diff the suspect tables directly against JM's. Lesson:
  self-consistent round-trips are necessary but **not** sufficient; the
  per-element JM trace diff is the real guard.
- **Scaling lists.** High-profile streams may carry non-flat scaling
  matrices; the SPS/PPS parsers currently *skip* them. The PoC must
  actually apply them (or assert flat and bail otherwise) to stay
  bit-exact.
- **Build-vs-reuse, again.** If the PoC proves painful, the fallback for
  the *product* remains the mvcdep carve (shipped) or Option A (Britz
  libavcodec forward-port) — libmvc's unique payoff (pure-Rust, realtime
  playback, stream-from-disc) is what justifies pushing through.

## Recommendation

Build the PoC as scoped: **one base-view IDR frame, CABAC/High profile,
bit-exact vs libavcodec, ~5–6 weeks.** It is the smallest artefact that
answers the only open libmvc question ("can we decode H.264 correctly in
Rust?") and produces a reusable MB-decode core. Gate the multi-month full
decoder on the PoC matching byte-for-byte. Until then, mvcdep + HW encode
remain the shipping fast path.
