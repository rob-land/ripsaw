# Britz FFmpeg MVC fork — vendored reference material

This directory captures the Britz fork's MVC delta, extracted on
2026-05-28 from `github.com/Britz/FFmpeg @ mvc` (single commit
`1217f9077`, dated 2013-02-21, atop FFmpeg 0.11.2 commit `69cc119d6`).

The Britz fork is **not used at runtime**. See `docs/mvc3d.md` for the
sample-validation findings that ruled it out as a direct dependency.
The material here is preserved as reference input for a forward-port
rewrite against current FFmpeg.

## Layout

```
third_party/ffmpeg-mvc-britz/
├── README.md              this file
├── new-files/
│   ├── h264_mvc.c         1,284 lines — the MVC decoder algorithm itself
│   └── h264_mvc.h         242 lines  — its public surface (within libavcodec)
└── patches/
    ├── h264.c.patch       hooks into the legacy monolithic h264 decoder
    ├── h264.h.patch
    ├── h264_parser.c.patch
    ├── h264_ps.c.patch    sub-SPS parsing (the MVC parameter set extension)
    ├── h264_refs.c.patch  inter-view reference picture handling
    ├── h264_sei.c.patch
    ├── mpegvideo.h.patch  adds `view_id` field to `Picture`
    ├── vaapi_h264.c.patch hardware-decode wiring
    └── Makefile.patch     build glue (`OBJS += h264_mvc.o`)
```

## Why the patches do not apply to current FFmpeg

A `git apply --check` of every patch fails. The fundamental reason is
that **`libavcodec/h264.c` does not exist anymore**. Modern FFmpeg
(7.x) has split the monolithic 2012-era decoder into:

- `h264dec.c` — codec init, packet decode entry
- `h264_slice.c` — per-slice decode loop
- `h264_picture.c` — DPB / picture lifecycle
- `h264_mb.c`, `h264_mb_template.c` — macroblock decode
- `h2645_parse.c`, `h2645_sei.c` — H.264/H.265 shared parsing
- and more

The other files Britz touched (`h264.h`, `h264_ps.c`, `h264_refs.c`,
`mpegvideo.h`, `vaapi_h264.c`) also exist in current FFmpeg but with
substantially different structure. The patches fail at the first hunk
in each file.

## Forward-port plan

1. **`h264_mvc.c` and `h264_mvc.h` can be imported almost verbatim.**
   The algorithm — sub-SPS parsing, view-id assignment, inter-view
   prediction reference handling — is mostly self-contained and reads
   the H.264 bitstream via APIs that still exist (`get_ue_golomb`,
   `get_bits1`, etc.).
2. **Each Britz patch must be rewritten as a modern integration**:
   - Sub-SPS / subset SPS parsing → `h264_ps.c` (still exists; add a
     new parser for NAL type 15)
   - View-id on `Picture` → `H264Picture` in `h264dec.h`
   - Slice header bits for MVC → `h264_slice.c`
   - Reference list construction for dependent view → `h264_refs.c`
3. **The output side Britz left as TODO must be finished.**
   - Modern FFmpeg has `AVFrameSideData` with type
     `AV_FRAME_DATA_STEREO3D`. The dependent-view AVFrame should carry
     `AV_STEREO3D_2D` flagged as the right-eye view, or — better —
     both views should be packed into one AVFrame using
     `AV_PIX_FMT_*` with a stereo layout descriptor.
   - Alternative: emit two AVFrames per coded frame with a custom
     side-data tag identifying which eye.
4. **Cross-check correctness against the JM reference decoder
   (`ldecod`).** It is slow but reference-correct; it must produce the
   same YUV bytes as our forward-ported decoder on the same input.
5. **Test corpus**: physical-BD SSIF streams. Modern MakeMKV `mvcC`
   MKVs are a separate problem — see `docs/mvc3d.md` § "Modern
   MakeMKV `mvcC` MKVs".

Realistic effort: 2–4 weeks for someone fluent in libavcodec internals.
Not on the v1 critical path; tracked as a Phase 2 deliverable.

## Licensing

The Britz fork is LGPL v2.1+ (same as upstream FFmpeg 0.11). Code
ported from `h264_mvc.{c,h}` into a forward-ported decoder retains
that licence and must be marked accordingly in the new file headers.
3drip's GPL-3-or-later application licence is compatible with linking
against LGPL libav* libraries.
