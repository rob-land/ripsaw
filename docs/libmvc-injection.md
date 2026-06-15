# libmvc — base-view frame injection: scope & de-risking

Status: scoping doc (2026-06-15). Companion to `docs/libmvc.md` (which
chose Option B as the eventual decoder strategy) and `docs/mvc3d.md`.

The central architectural risk for a *faster-than-ldecod* MVC decoder is
the **base-frame injection seam**: can we decode the base view with a
fast, off-the-shelf decoder (libavcodec) and feed its reconstructed
frame into the dependent-view decode as an inter-view reference? Every
hybrid option (B and C in `docs/libmvc.md`) lives or dies on this seam.

## 1. What "injection" means (the MVC reference semantics)

A stereo MVC stream is two coded views per access unit (AU):

- **base view** (`view_id` 0): an ordinary H.264 High-profile bitstream.
  Any conformant decoder decodes it; the dependent NALs are ignored.
- **dependent view** (`view_id` 1): coded slices (NAL type 20) that
  predict from (a) the dependent view's *own* earlier frames, exactly
  like normal H.264 inter prediction, **and** (b) the **base view's
  reconstructed frame of the same AU**, added to the reference picture
  lists as an *inter-view reference* (Annex G § 8.2.4, § H.8.2).

The elegance of Annex G is that inter-view prediction is **not** a new
prediction mode — it is ordinary motion-compensated prediction where one
of the entries in `RefPicList0` (and possibly `RefPicList1`) happens to
be the base-view picture instead of a temporal one. The dependent-view
decoder needs no new pixel math; it needs the base-view frame **present
in its DPB, at the right list position, with the right POC/view_idx**.

So "injection" = make the base view's reconstructed YUV available to the
dependent-view decoder as a reference picture. That's the whole job.
`ref_pic_list_modification` IDC 4/5 (already parsed in
`src/mvc/ref_pic_list_modification.rs`) is how the slice reorders that
inter-view entry into a specific list index.

## 2. De-risking result (the experiment that motivated this doc)

The foundational assumption — "a fast decoder's base view is *exactly*
the reference the dependent view expects" — was tested on a real 3D
Blu-ray (Friday the 13th Part 3, MakeMKV MVC rip), 2026-06-15:

1. Decode the first 25 base-view frames with **ffmpeg/libavcodec**
   (`-map 0:v:0 -frames:v 50 -f rawvideo -pix_fmt yuv420p`). libavcodec
   ignores the dependent NALs and the `sps_id 1` subset SPS, emitting the
   2D base view, cropped 1920×1080, in display order.
2. Decode the same stream with **JM ldecod** (`DecodeAllLayers=1`,
   `DecFrmNum=50`), which writes `out_ViewId0000.yuv` (base) and
   `out_ViewId0001.yuv` (dependent), both cropped 1920×1080.
3. MD5 each 1920×1080 4:2:0 frame and compare base vs base.

**Result: 25/25 frames byte-identical, positionally (same index).**
`frame0 ffmpeg == frame0 ldecod`, …, through frame 24.

Interpretation:
- libavcodec's base reconstruction equals the reference decoder's base
  reconstruction **bit-for-bit** — as H.264 § 8 guarantees ("all
  decoders shall produce numerically identical results"), now confirmed
  on real Blu-ray data rather than assumed.
- Output ordering matched without reconciliation, so for this stream the
  base view has no display/decode reorder gap we have to bridge (likely
  IPPP base GOPs). A B-frame base GOP would need POC-based realignment
  before injection; the dependent decoder addresses pictures by POC
  anyway, so this is a bookkeeping concern, not a correctness one.

**Conclusion: the injection seam is sound at the data level.** The base
view a fast decoder produces *is* the inter-view reference. What remains
is plumbing it into a dependent-view decoder — not proving it's the
right data.

Reproduce: `examples/extract_mvcc_mkv` (annexb) + the ldecod cfg in
`src/convert/runner.rs::build_decoder_cfg` with `DecFrmNum` set + an
ffmpeg `rawvideo` decode + per-frame MD5. Needs an MVC source and
`scripts/build-ldecod.sh`-built ldecod.

## 3. The seam, concretely

```
        MVC Annex B (base + dependent NALs, interleaved per AU)
                 │
     ┌───────────┴────────────┐
     ▼                         ▼
  base NALs                dependent NALs (type 20)
  (view_id 0)              (view_id 1)
     │                         │
     ▼                         ▼
 libavcodec H.264         dependent-view decoder
 (fast, SIMD, threaded)   (libmvc / JM-carve)
     │                         │  RefPicList0 = [ temporal refs…,
     │  reconstructed base     │                 INJECTED base frame ]
     └──────── YUV ───────────▶│  (motion comp uses it like any ref)
                               ▼
                       dependent-view YUV
                               │
              ┌────────────────┴───────────────┐
              ▼                                 ▼
        ViewId0000 (base)                 ViewId0001 (dep)
              └──────── compose (SBS / TAB / MV-HEVC) ───────┘
```

The dependent-view decoder is a near-complete H.264 slice decoder
(CAVLC + CABAC, inverse transform, intra + inter prediction, deblocking,
its own DPB) — **minus** the parts already built (front-end parsing) and
**plus** one hook: when constructing `RefPicList0/1`, insert the
externally supplied base frame at the position Annex G § 8.2.4 dictates.

## 4. Mechanism options, re-evaluated for the seam

`docs/libmvc.md` already surveyed A–D. With injection now proven sound,
the choice narrows to *how to build the dependent-view decoder*:

| Option | Dependent-view decoder | Injection hook | Effort | Risk |
|---|---|---|---|---|
| **B. JM carve** | Vendor JM's macroblock decode + `mbuffer_mvc.c`, drive it ourselves | JM already injects the inter-view ref internally; we replace JM's *base* decode with libavcodec's frame | ~2–3 wk | Low correctness (JM is the reference), tedious C carve, only ~3–5× faster (JM dep decode still reference-speed) |
| **C. Rust dep decoder** | Write the dependent-view slice decoder in Rust on top of the finished front end | We own RefPicList construction → trivial to inject | multi-month | High effort; full MB layer + CABAC; but pure Rust, and only ONE view to decode |
| **A. FFmpeg fork** | Forward-port Britz MVC patches into libavcodec | Inside libavcodec's DPB | 2–4 wk + perpetual fork | Both views get SIMD/threading (fastest), but fork maintenance forever |

Key realisation from the experiment: because the base view is **fully
offloaded to libavcodec** and bit-exact, the dependent decoder never
decodes the base view at all — it only needs to (1) decode dependent
slices and (2) accept the base frame as a ref. That shrinks Option C: we
implement *one* view's macroblock decode, not two, and skip all base/AVC
SPS/PPS handling beyond what the front end already does.

## 5. Recommended path

1. **Next PoC — prove the hook, not the whole decoder.** Take Option B's
   lowest-effort wedge: build JM `ldecod` in a mode where the base view
   is **replaced** by libavcodec's frame (stub out JM's base decode; load
   `out_ViewId0000.yuv` frames as the base DPB) and confirm the dependent
   view still decodes bit-identically to stock ldecod's `ViewId0001`.
   That proves the injection hook end-to-end with ~a few days of C, no
   new decoder. Success criterion: `ViewId0001` unchanged when the base
   is injected from libavcodec instead of decoded by JM.
2. If the hook holds, decide B vs C on the speed target: B ships a
   working hybrid fast (base at libavcodec speed, dep at JM speed →
   the ~3–5× the survey predicted); C is the long road to 10×+.
3. Either way, the finished front-end parsers (SPS/PPS/slice header) and
   the proven injection feed both.

## 6. Open questions for the next session

- **Base-view reorder.** Confirm whether any disc in the corpus has a
  B-frame base GOP; if so, align ffmpeg display-order output to the
  dependent decoder's POC-indexed DPB. (Decode-order output from
  libavcodec via `-fflags +bitexact` / reordering off may be simpler.)
- **Frame handoff format.** libavcodec base frame → dependent decoder:
  raw YUV planes vs `AVFrame` refcounted buffers vs shared DPB slot.
- **Field/MBAFF.** Blu-ray 3D is progressive (`frame_mbs_only_flag=1`,
  confirmed by the parser); field coding is out of scope for v1.
- **Timing.** Measure ffmpeg base-view decode fps on this host to bound
  the achievable speedup ceiling for Option B (base no longer the
  bottleneck; dep-view JM decode becomes it).
