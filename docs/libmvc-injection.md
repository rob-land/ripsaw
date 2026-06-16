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
view a fast decoder produces *is* the inter-view reference.

Reproduce: `examples/extract_mvcc_mkv` (annexb) + the ldecod cfg in
`src/convert/runner.rs::build_decoder_cfg` with `DecFrmNum` set + an
ffmpeg `rawvideo` decode + per-frame MD5. Needs an MVC source and
`scripts/build-ldecod.sh`-built ldecod.

### 2b. Injection hook proven end-to-end (2026-06-16)

The "is it the right data" question above is necessary but not
sufficient — we also have to show a dependent-view decoder *accepts an
externally supplied base frame as its inter-view reference and decodes
correctly*. Tested by patching JM ldecod (`scripts/ldecod-base-inject.patch`)
with a hook in `exit_picture()` that, for the base view only, either
captures or **overwrites** the cropped reconstruction from a file —
exactly where the base picture is finalized and just before it pads and
becomes the inter-view reference (`mbuffer_mvc.c` puts that very
`StorablePicture` into the dependent slice's `listX`).

On the same clip (96 frames), comparing dependent output (`ViewId0001`)
of patched vs stock ldecod:

| Run | Result |
|---|---|
| Inject JM's own captured base (decode order) | `ViewId0001` **96/96 identical** to stock |
| Inject **libavcodec's** base directly | `ViewId0001` **96/96 identical** to stock |
| Inject a **perturbed** base (+40 luma) | `ViewId0001` **96/96 frames changed** vs stock |

Also confirmed: the base view here has decode order == display order
(captured-decode-order base matched ffmpeg's display-order base 96/96),
so no POC realignment was needed for this stream.

The first two rows prove **libavcodec base → JM dependent decode →
byte-identical correct dependent output**. The third row is the
causality control: perturbing the injected base changes *every*
dependent frame, so the dependent decoder genuinely consumes the
injected pixels (the hook is not a no-op). The injection architecture is
empirically settled end-to-end on real Blu-ray data; only base *pixels*
are needed by the dependent view (no base MVs/modes), exactly as Annex G
predicts.

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

1. **Prove the hook, not the whole decoder. — DONE (2026-06-16, § 2b).**
   Patched JM ldecod to replace the base view's reconstruction with
   externally supplied (libavcodec) frames; `ViewId0001` came back
   96/96 identical to stock, and perturbing the injected base changed
   96/96 dependent frames. The injection hook is proven end-to-end.
2. If the hook holds, decide B vs C on the speed target: B ships a
   working hybrid fast (base at libavcodec speed, dep at JM speed →
   the ~3–5× the survey predicted); C is the long road to 10×+.
3. Either way, the finished front-end parsers (SPS/PPS/slice header) and
   the proven injection feed both.

## 5b. Measured speedup ceiling (2026-06-16)

Before committing to the Option B carve, measured what it actually buys
on this host (20 cores), timing 400 access units of the real feature:

| Stage | Current (ldecod) | Option B hybrid |
|---|---|---|
| base-view decode | 4.98 s (JM) | **0.39 s** (libavcodec, ~1025 fps) |
| dependent-view decode | 5.27 s (JM) | 5.27 s (JM — unchanged) |
| **decode total** | **10.25 s** | **5.66 s → 1.81×** |
| encode (libx264 `-preset medium -crf 18`, FSBS) | 6.68 s | 6.68 s |
| **convert total (serial decode→encode)** | **16.93 s** | **12.34 s → 1.37×** |

Key findings:

- **The dependent view is the floor.** It costs *more* than the base
  (5.27 s vs 4.98 s) and Option B leaves it at JM reference speed. So the
  decode-only ceiling is **~1.9×** (10.25/5.27), not the 3–5× the
  original survey hoped for — offloading the base can at best halve
  decode, and the base is slightly less than half.
- **libavcodec base decode is ~13× faster than JM's** (0.39 s vs 4.98 s),
  i.e. essentially free. The base offload itself is not the question;
  the dependent view is.
- **End-to-end depends on the encoder.** With software x264, encode is
  ~39 % of convert, so Option B yields only **~1.37×** end-to-end
  (a 95-min feature: ~97 min → ~71 min). With a ~free HW encoder
  (NVENC), convert becomes decode-bound and Option B approaches its
  **~1.8×** decode figure.
- **Orthogonal win available without any decoder surgery:** decode and
  encode currently run serially via YUV files. Pipelining them overlaps
  ~6.7 s of encode under decode → ~max(decode, encode) instead of the
  sum, ~1.65× on its own, and it stacks with HW encode. Cheaper than the
  carve and independent of it.

**Verdict.** Option B is a real but modest win, hard-capped near ~1.9×
decode / ~1.8× end-to-end (and only with HW encode; ~1.4× with x264).
It is worth the ~2–3 week carve only if (a) HW encode is the default so
decode is the bottleneck, or (b) it is a stepping stone to also
accelerating the dependent view (SIMD / the Rust dependent decoder on the
finished front end), which is where the 3–5×+ actually lives.

### 5c. HW-encode measurements (2026-06-16)

This host has **no NVIDIA GPU** (`nvidia-smi` absent, `h264_nvenc` fails
with "Cannot load libcuda.so.1") — it's an **Intel Iris Xe** iGPU, so the
working HW encoders are **VAAPI** and **QSV** (Intel iHD driver). Encode
of the same 400-frame FSBS content (base‖base proxy; x264 on the proxy
was 6.92 s vs 6.68 s on real base‖dep, ~3 % — proxy validated):

| Encoder | encode 400f | vs x264 |
|---|---|---|
| libx264 `-preset medium -crf 18` | 6.92 s (~58 fps) | 1.0× |
| **h264_vaapi** `-qp 18` | **1.65 s** (~242 fps) | **4.2×** |
| h264_qsv `-global_quality 18` | 2.34 s (~171 fps) | 3.0× |

Convert (serial decode→encode) per 400 AU, and Option B's value at each:

| Config | convert | decode share | Option B end-to-end |
|---|---|---|---|
| current decode + x264 | 17.17 s | 60 % | 1.36× |
| current decode + **VAAPI** | **11.90 s** | **86 %** | **1.63×** |
| current decode + QSV | 12.59 s | 81 % | 1.57× |

95-min feature wall times: current+x264 **98 min** → +VAAPI **68 min**
(1.44× from the encoder swap alone, zero decoder work) → Option B+VAAPI
**42 min** (2.35× combined).

Implications:
- **The single cheapest win is defaulting to a HW encoder** (VAAPI here):
  98→68 min, no decoder surgery, and it makes convert **86 % decode-bound**.
- Once HW-encode-bound, Option B's decode speedup translates almost
  directly: **1.63×** end-to-end (vs 1.36× with x264). So HW encode and
  Option B are complementary, and Option B is clearly more valuable once
  HW encode is the default.
- Pipelining decode↔encode helps the *x264* case (encode is 40 %) but
  barely helps the HW-encode case (encode is only ~14 %, hidden entirely
  under decode), so it is **not** worth pursuing once HW encode is on.
- NVENC numbers can't be taken here; on an NVIDIA host NVENC is typically
  a touch faster than VAAPI, so it would land in the same "encode becomes
  negligible, convert is decode-bound, Option B ≈ 1.6×" regime.

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
